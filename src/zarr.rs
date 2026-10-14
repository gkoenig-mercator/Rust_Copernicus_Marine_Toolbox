//! Minimal read-only Zarr v2 client over HTTPS (ARCO datasets of the
//! Marine Data Store), with selection by arbitrary index lists.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use indexmap::IndexMap;
use log::debug;
use serde_json::{Map, Value};

use crate::codecs::Compressor;
use crate::http::Http;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Bool,
    Int,
    UInt,
    Float,
}

/// Element type. Data is always kept in little-endian once decoded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DType {
    pub kind: Kind,
    pub size: usize,
    big_endian: bool,
}

impl DType {
    pub fn parse(dtype: &str) -> Result<Self> {
        let bytes = dtype.as_bytes();
        if bytes.len() < 3 {
            bail!("Unsupported dtype '{dtype}'");
        }
        let big_endian = bytes[0] == b'>';
        let kind = match bytes[1] {
            b'b' => Kind::Bool,
            b'i' => Kind::Int,
            b'u' => Kind::UInt,
            b'f' => Kind::Float,
            _ => bail!("Unsupported dtype '{dtype}'"),
        };
        let size: usize = dtype[2..]
            .parse()
            .with_context(|| format!("Unsupported dtype '{dtype}'"))?;
        if !matches!(size, 1 | 2 | 4 | 8) || (kind == Kind::Float && size < 4) {
            bail!("Unsupported dtype '{dtype}'");
        }
        Ok(Self {
            kind,
            size,
            big_endian,
        })
    }

    /// Read element `i` of a little-endian buffer as f64.
    pub fn get_f64(&self, data: &[u8], i: usize) -> f64 {
        let b = &data[i * self.size..(i + 1) * self.size];
        match (self.kind, self.size) {
            (Kind::Float, 4) => f32::from_le_bytes(b.try_into().unwrap()) as f64,
            (Kind::Float, 8) => f64::from_le_bytes(b.try_into().unwrap()),
            (Kind::Int, 1) => b[0] as i8 as f64,
            (Kind::Int, 2) => i16::from_le_bytes(b.try_into().unwrap()) as f64,
            (Kind::Int, 4) => i32::from_le_bytes(b.try_into().unwrap()) as f64,
            (Kind::Int, 8) => i64::from_le_bytes(b.try_into().unwrap()) as f64,
            (Kind::UInt | Kind::Bool, 1) => b[0] as f64,
            (Kind::UInt, 2) => u16::from_le_bytes(b.try_into().unwrap()) as f64,
            (Kind::UInt, 4) => u32::from_le_bytes(b.try_into().unwrap()) as f64,
            (Kind::UInt, 8) => u64::from_le_bytes(b.try_into().unwrap()) as f64,
            _ => unreachable!(),
        }
    }

    /// Encode a value in this dtype (little-endian), used for fill values.
    pub fn encode_f64(&self, value: f64) -> Vec<u8> {
        match (self.kind, self.size) {
            (Kind::Float, 4) => (value as f32).to_le_bytes().to_vec(),
            (Kind::Float, 8) => value.to_le_bytes().to_vec(),
            (Kind::Int, 1) => vec![value as i8 as u8],
            (Kind::Int, 2) => (value as i16).to_le_bytes().to_vec(),
            (Kind::Int, 4) => (value as i32).to_le_bytes().to_vec(),
            (Kind::Int, 8) => (value as i64).to_le_bytes().to_vec(),
            (Kind::UInt | Kind::Bool, 1) => vec![value as u8],
            (Kind::UInt, 2) => (value as u16).to_le_bytes().to_vec(),
            (Kind::UInt, 4) => (value as u32).to_le_bytes().to_vec(),
            (Kind::UInt, 8) => (value as u64).to_le_bytes().to_vec(),
            _ => unreachable!(),
        }
    }

    fn make_little_endian(self, data: &mut [u8]) {
        if self.big_endian && self.size > 1 {
            data.chunks_exact_mut(self.size).for_each(|c| c.reverse());
        }
    }
}

pub fn parse_fill_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(*b as u8 as f64),
        Value::String(s) => match s.as_str() {
            "NaN" => Some(f64::NAN),
            "Infinity" => Some(f64::INFINITY),
            "-Infinity" => Some(f64::NEG_INFINITY),
            _ => None,
        },
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct ZarrArray {
    pub name: String,
    pub shape: Vec<usize>,
    pub chunks: Vec<usize>,
    pub dtype: DType,
    pub fill_value: Option<f64>,
    pub dimensions: Vec<String>,
    pub attributes: Map<String, Value>,
    compressor: Option<Compressor>,
    separator: String,
}

impl ZarrArray {
    fn from_metadata(name: &str, zarray: &Value, zattrs: Option<&Value>) -> Result<Self> {
        let usizes = |v: &Value| -> Vec<usize> {
            v.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_u64)
                        .map(|x| x as usize)
                        .collect()
                })
                .unwrap_or_default()
        };
        if zarray["order"].as_str() == Some("F") {
            bail!("Fortran ordered Zarr arrays are not supported ({name})");
        }
        if zarray["filters"].as_array().is_some_and(|f| !f.is_empty()) {
            bail!("Zarr filters are not supported ({name})");
        }
        let mut attributes = zattrs
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let dimensions = attributes
            .remove("_ARRAY_DIMENSIONS")
            .and_then(|d| d.as_array().cloned())
            .map(|d| {
                d.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            name: name.to_string(),
            shape: usizes(&zarray["shape"]),
            chunks: usizes(&zarray["chunks"]),
            dtype: DType::parse(zarray["dtype"].as_str().unwrap_or_default())
                .with_context(|| format!("Array {name}"))?,
            fill_value: parse_fill_value(&zarray["fill_value"]),
            dimensions,
            attributes,
            compressor: Compressor::from_zarr_metadata(&zarray["compressor"])?,
            separator: zarray["dimension_separator"]
                .as_str()
                .unwrap_or(".")
                .to_string(),
        })
    }

    fn chunk_key(&self, chunk_index: &[usize]) -> String {
        if chunk_index.is_empty() {
            return format!("{}/0", self.name);
        }
        let key: Vec<String> = chunk_index.iter().map(|c| c.to_string()).collect();
        format!("{}/{}", self.name, key.join(&self.separator))
    }

    fn chunk_len(&self) -> usize {
        self.chunks.iter().product::<usize>().max(1)
    }

    /// Number of chunks touched by a selection (one index list per dimension).
    pub fn number_of_chunks(&self, selection: &[Vec<usize>]) -> usize {
        selection
            .iter()
            .zip(&self.chunks)
            .map(|(indices, chunk)| {
                let mut ids: Vec<usize> = indices.iter().map(|i| i / chunk).collect();
                ids.sort_unstable();
                ids.dedup();
                ids.len()
            })
            .product()
    }

    pub fn chunk_size_bytes(&self) -> usize {
        self.chunk_len() * self.dtype.size
    }
}

/// Odometer-style increment of the outer dimension counters.
/// Returns false once every combination has been visited.
fn increment(counters: &mut [usize], lists: &[&Vec<(usize, usize)>]) -> bool {
    for d in (0..counters.len()).rev() {
        counters[d] += 1;
        if counters[d] < lists[d].len() {
            return true;
        }
        counters[d] = 0;
    }
    false
}

pub struct ZarrStore {
    pub url: String,
    pub arrays: IndexMap<String, ZarrArray>,
    pub attributes: Map<String, Value>,
    http: Http,
}

impl ZarrStore {
    pub async fn open(http: &Http, url: &str) -> Result<Self> {
        let url = url.trim_end_matches('/').to_string();
        let metadata = http
            .get_json(&format!("{url}/.zmetadata"))
            .await
            .with_context(|| format!("Could not open Zarr store {url} (consolidated metadata)"))?;
        let metadata = metadata["metadata"]
            .as_object()
            .context("Invalid .zmetadata")?;
        let mut arrays = IndexMap::new();
        for (key, zarray) in metadata {
            if let Some(name) = key.strip_suffix("/.zarray") {
                let zattrs = metadata.get(&format!("{name}/.zattrs"));
                arrays.insert(
                    name.to_string(),
                    ZarrArray::from_metadata(name, zarray, zattrs)?,
                );
            }
        }
        let attributes = metadata
            .get(".zattrs")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            url,
            arrays,
            attributes,
            http: http.clone(),
        })
    }

    pub fn array(&self, name: &str) -> Result<&ZarrArray> {
        self.arrays
            .get(name)
            .with_context(|| format!("Variable '{name}' not found in {}", self.url))
    }

    async fn fetch_chunk(http: Http, url: String, array: ZarrArray) -> Result<Option<Vec<u8>>> {
        let Some(raw) = http.get_bytes_optional(&url).await? else {
            debug!("Missing chunk {url}, using fill value");
            return Ok(None);
        };
        let expected = array.chunk_size_bytes();
        let decoded = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let mut data = match &array.compressor {
                Some(compressor) => compressor.decompress(&raw, expected)?,
                None => raw.to_vec(),
            };
            if data.len() != expected {
                bail!("Chunk {url} has {} bytes, expected {expected}", data.len());
            }
            array.dtype.make_little_endian(&mut data);
            Ok(data)
        })
        .await??;
        Ok(Some(decoded))
    }

    /// Read the elements at the cartesian product of the given indices
    /// (one list per dimension, in output order). Returns little-endian bytes.
    pub async fn read(
        &self,
        array_name: &str,
        selection: &[Vec<usize>],
        concurrency: usize,
        progress: Option<&indicatif::ProgressBar>,
    ) -> Result<Vec<u8>> {
        let array = self.array(array_name)?.clone();
        let ndim = array.shape.len();
        if selection.len() != ndim {
            bail!("Selection rank mismatch for {array_name}");
        }
        let elem = array.dtype.size;
        let out_shape: Vec<usize> = selection.iter().map(Vec::len).collect();
        let out_len: usize = out_shape.iter().product();
        let fill = array.dtype.encode_f64(array.fill_value.unwrap_or(0.0));
        let mut out: Vec<u8> = fill.iter().copied().cycle().take(out_len * elem).collect();
        if out_len == 0 {
            return Ok(out);
        }

        // Per dimension: chunk id -> list of (output index, index inside chunk)
        let per_dim: Vec<BTreeMap<usize, Vec<(usize, usize)>>> = selection
            .iter()
            .zip(&array.chunks)
            .map(|(indices, &chunk)| {
                let mut map: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();
                for (o, &i) in indices.iter().enumerate() {
                    map.entry(i / chunk).or_default().push((o, i % chunk));
                }
                map
            })
            .collect();

        // Cartesian product of the chunk ids
        let mut combos: Vec<Vec<usize>> = vec![vec![]];
        for dim in &per_dim {
            combos = combos
                .into_iter()
                .flat_map(|prefix| {
                    dim.keys().map(move |&c| {
                        let mut v = prefix.clone();
                        v.push(c);
                        v
                    })
                })
                .collect();
        }

        // Output strides and chunk strides (C order)
        let strides = |shape: &[usize]| -> Vec<usize> {
            let mut s = vec![1; shape.len()];
            for d in (0..shape.len().saturating_sub(1)).rev() {
                s[d] = s[d + 1] * shape[d + 1];
            }
            s
        };
        let out_strides = strides(&out_shape);
        let chunk_strides = strides(&array.chunks);

        let mut results = futures::stream::iter(combos.into_iter().map(|combo| {
            let url = format!("{}/{}", self.url, array.chunk_key(&combo));
            let fut = Self::fetch_chunk(self.http.clone(), url, array.clone());
            async move { (combo, fut.await) }
        }))
        .buffer_unordered(concurrency.max(1));

        while let Some((combo, chunk)) = results.next().await {
            if let Some(progress) = progress {
                progress.inc(1);
            }
            let Some(chunk) = chunk? else { continue };
            let lists: Vec<&Vec<(usize, usize)>> = combo
                .iter()
                .enumerate()
                .map(|(d, c)| &per_dim[d][c])
                .collect();
            if ndim == 0 {
                out[..elem].copy_from_slice(&chunk[..elem]);
                continue;
            }
            // Iterate over all dims except the last, copy along the last one
            let mut counters = vec![0usize; ndim - 1];
            loop {
                let mut out_base = 0;
                let mut in_base = 0;
                for d in 0..ndim - 1 {
                    let (o, i) = lists[d][counters[d]];
                    out_base += o * out_strides[d];
                    in_base += i * chunk_strides[d];
                }
                for &(o, i) in lists[ndim - 1] {
                    let dst = (out_base + o) * elem;
                    let src = (in_base + i) * elem;
                    out[dst..dst + elem].copy_from_slice(&chunk[src..src + elem]);
                }
                if !increment(&mut counters, &lists) {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Read a full 1-D array as f64 (coordinates).
    pub async fn read_coordinate(&self, name: &str, concurrency: usize) -> Result<Vec<f64>> {
        let array = self.array(name)?;
        let selection: Vec<Vec<usize>> = array.shape.iter().map(|&n| (0..n).collect()).collect();
        let dtype = array.dtype;
        let bytes = self.read(name, &selection, concurrency, None).await?;
        Ok((0..bytes.len() / dtype.size)
            .map(|i| dtype.get_f64(&bytes, i))
            .collect())
    }
}
