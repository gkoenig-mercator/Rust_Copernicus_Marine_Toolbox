//! Pure Rust NetCDF classic writer.
//!
//! Writes the 64-bit offset format (CDF-2), readable by every NetCDF library
//! (netCDF4-python, xarray with netcdf4/scipy/h5netcdf... engines, ncdump,
//! Panoply, ...). Switches to CDF-5 when a variable is larger than 4 GiB.
//! No record (unlimited) dimension is used.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{bail, Result};

use crate::zarr::{DType, Kind};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NcType {
    Byte = 1,
    Char = 2,
    Short = 3,
    Int = 4,
    Float = 5,
    Double = 6,
    UByte = 7,
    UShort = 8,
    UInt = 9,
    Int64 = 10,
    UInt64 = 11,
}

impl NcType {
    fn size(self) -> usize {
        match self {
            NcType::Byte | NcType::Char | NcType::UByte => 1,
            NcType::Short | NcType::UShort => 2,
            NcType::Int | NcType::UInt | NcType::Float => 4,
            NcType::Double | NcType::Int64 | NcType::UInt64 => 8,
        }
    }

    /// NetCDF type used to store a Zarr dtype.
    /// CDF-2 has no unsigned/64-bit integers, so those are widened.
    pub fn for_dtype(dtype: &DType, cdf5: bool) -> Self {
        match (dtype.kind, dtype.size, cdf5) {
            (Kind::Float, 4, _) => NcType::Float,
            (Kind::Float, _, _) => NcType::Double,
            (Kind::Int, 1, _) | (Kind::Bool, _, _) => NcType::Byte,
            (Kind::Int, 2, _) => NcType::Short,
            (Kind::Int, 4, _) => NcType::Int,
            (Kind::Int, _, true) => NcType::Int64,
            (Kind::UInt, 1, true) => NcType::UByte,
            (Kind::UInt, 2, true) => NcType::UShort,
            (Kind::UInt, 4, true) => NcType::UInt,
            (Kind::UInt, _, true) => NcType::UInt64,
            (Kind::UInt, 1, false) => NcType::Short,
            (Kind::UInt, 2, false) => NcType::Int,
            _ => NcType::Double,
        }
    }

    fn write_value(self, out: &mut Vec<u8>, value: f64) {
        match self {
            NcType::Byte => out.push(value as i8 as u8),
            NcType::Char | NcType::UByte => out.push(value as u8),
            NcType::Short => out.extend((value as i16).to_be_bytes()),
            NcType::UShort => out.extend((value as u16).to_be_bytes()),
            NcType::Int => out.extend((value as i32).to_be_bytes()),
            NcType::UInt => out.extend((value as u32).to_be_bytes()),
            NcType::Float => out.extend((value as f32).to_be_bytes()),
            NcType::Double => out.extend(value.to_be_bytes()),
            NcType::Int64 => out.extend((value as i64).to_be_bytes()),
            NcType::UInt64 => out.extend((value as u64).to_be_bytes()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum AttrValue {
    Text(String),
    Numbers(NcType, Vec<f64>),
}

impl AttrValue {
    /// Convert a JSON attribute (from .zattrs) to a NetCDF attribute.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        use serde_json::Value;
        let number = |v: &Value| -> Option<f64> {
            match v {
                Value::Number(n) => n.as_f64(),
                Value::Bool(b) => Some(*b as u8 as f64),
                Value::String(s) if s == "NaN" => Some(f64::NAN),
                _ => None,
            }
        };
        let numeric_type = |values: &[&Value]| {
            let all_int = values
                .iter()
                .all(|v| v.as_i64().is_some_and(|i| i32::try_from(i).is_ok()) || v.is_boolean());
            if all_int {
                NcType::Int
            } else {
                NcType::Double
            }
        };
        match value {
            Value::String(s) => Some(AttrValue::Text(s.clone())),
            Value::Number(_) | Value::Bool(_) => Some(AttrValue::Numbers(
                numeric_type(&[value]),
                vec![number(value)?],
            )),
            Value::Array(items) if items.iter().all(|i| i.is_string()) => Some(AttrValue::Text(
                items
                    .iter()
                    .filter_map(|i| i.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            )),
            Value::Array(items) if !items.is_empty() => {
                let refs: Vec<&Value> = items.iter().collect();
                let values: Option<Vec<f64>> = items.iter().map(number).collect();
                Some(AttrValue::Numbers(numeric_type(&refs), values?))
            }
            _ => None,
        }
    }

    pub fn cast(self, nc_type: NcType) -> Self {
        match self {
            AttrValue::Numbers(_, values) => AttrValue::Numbers(nc_type, values),
            text => text,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    pub value: AttrValue,
}

pub struct Variable {
    pub name: String,
    pub dimensions: Vec<usize>,
    pub attributes: Vec<Attribute>,
    pub nc_type: NcType,
    /// Source data, little-endian, of type `source_dtype`
    pub data: Vec<u8>,
    pub source_dtype: DType,
}

#[derive(Default)]
pub struct NetCdfFile {
    /// Force the CDF-5 format (also used automatically for > 4 GiB variables)
    pub cdf5: bool,
    pub dimensions: Vec<(String, usize)>,
    pub attributes: Vec<Attribute>,
    pub variables: Vec<Variable>,
}

fn pad4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

struct Header {
    cdf5: bool,
    buf: Vec<u8>,
}

impl Header {
    fn count(&mut self, n: usize) {
        if self.cdf5 {
            self.buf.extend((n as u64).to_be_bytes());
        } else {
            self.buf.extend((n as u32).to_be_bytes());
        }
    }
    fn int(&mut self, n: u32) {
        self.buf.extend(n.to_be_bytes());
    }
    fn name(&mut self, name: &str) {
        self.count(name.len());
        self.buf.extend(name.as_bytes());
        self.buf.resize(pad4(self.buf.len()), 0);
    }
    fn attributes(&mut self, attributes: &[Attribute]) {
        if attributes.is_empty() {
            self.int(0);
            self.count(0);
            return;
        }
        self.int(0x0C); // NC_ATTRIBUTE
        self.count(attributes.len());
        for attribute in attributes {
            self.name(&attribute.name);
            match &attribute.value {
                AttrValue::Text(text) => {
                    self.int(NcType::Char as u32);
                    self.count(text.len());
                    self.buf.extend(text.as_bytes());
                }
                AttrValue::Numbers(nc_type, values) => {
                    self.int(*nc_type as u32);
                    self.count(values.len());
                    for v in values {
                        nc_type.write_value(&mut self.buf, *v);
                    }
                }
            }
            self.buf.resize(pad4(self.buf.len()), 0);
        }
    }
}

impl NetCdfFile {
    fn variable_sizes(&self) -> Vec<usize> {
        self.variables
            .iter()
            .map(|v| {
                let n: usize = v.dimensions.iter().map(|&d| self.dimensions[d].1).product();
                n * v.nc_type.size()
            })
            .collect()
    }

    pub fn needs_cdf5(&self) -> bool {
        self.variable_sizes()
            .iter()
            .any(|&s| s >= (u32::MAX as usize) - 4)
    }

    fn header(&self, cdf5: bool, begins: &[u64]) -> Vec<u8> {
        let mut h = Header {
            cdf5,
            buf: Vec::new(),
        };
        h.buf.extend(if cdf5 { b"CDF\x05" } else { b"CDF\x02" });
        h.count(0); // numrecs
        if self.dimensions.is_empty() {
            h.int(0);
            h.count(0);
        } else {
            h.int(0x0A); // NC_DIMENSION
            h.count(self.dimensions.len());
            for (name, len) in &self.dimensions {
                h.name(name);
                h.count(*len);
            }
        }
        h.attributes(&self.attributes);
        if self.variables.is_empty() {
            h.int(0);
            h.count(0);
        } else {
            h.int(0x0B); // NC_VARIABLE
            h.count(self.variables.len());
            let sizes = self.variable_sizes();
            for (i, variable) in self.variables.iter().enumerate() {
                h.name(&variable.name);
                h.count(variable.dimensions.len());
                for &d in &variable.dimensions {
                    h.count(d);
                }
                h.attributes(&variable.attributes);
                h.int(variable.nc_type as u32);
                let vsize = pad4(sizes[i]);
                if cdf5 {
                    h.buf.extend((vsize as u64).to_be_bytes());
                } else {
                    h.buf
                        .extend((vsize.min(u32::MAX as usize) as u32).to_be_bytes());
                }
                h.buf.extend(begins[i].to_be_bytes()); // 64-bit offsets
            }
        }
        h.buf
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let cdf5 = self.cdf5 || self.needs_cdf5();
        let sizes = self.variable_sizes();
        let header_len = self.header(cdf5, &vec![0; self.variables.len()]).len() as u64;
        let mut begins = Vec::with_capacity(sizes.len());
        let mut offset = header_len;
        for size in &sizes {
            begins.push(offset);
            offset += pad4(*size) as u64;
        }
        let mut writer = BufWriter::with_capacity(1 << 20, File::create(path)?);
        writer.write_all(&self.header(cdf5, &begins))?;
        for (variable, size) in self.variables.iter().zip(&sizes) {
            let count = variable.data.len() / variable.source_dtype.size;
            if count * variable.nc_type.size() != *size {
                bail!("Variable {} has an unexpected size", variable.name);
            }
            write_data(&mut writer, variable)?;
            let padding = pad4(*size) - size;
            writer.write_all(&vec![0u8; padding])?;
        }
        writer.flush()?;
        Ok(())
    }
}

fn write_data(writer: &mut impl Write, variable: &Variable) -> Result<()> {
    let dtype = variable.source_dtype;
    let same_layout = variable.nc_type.size() == dtype.size
        && matches!(
            (variable.nc_type, dtype.kind),
            (NcType::Float | NcType::Double, Kind::Float)
                | (
                    NcType::Byte | NcType::Short | NcType::Int | NcType::Int64,
                    Kind::Int
                )
                | (
                    NcType::UByte | NcType::UShort | NcType::UInt | NcType::UInt64,
                    Kind::UInt
                )
                | (NcType::Byte, Kind::Bool)
        );
    const BLOCK: usize = 1 << 16;
    let mut buffer = Vec::with_capacity(BLOCK * 8);
    if same_layout {
        // Only a byte swap is needed (little -> big endian)
        for block in variable.data.chunks(BLOCK * dtype.size) {
            buffer.clear();
            for element in block.chunks_exact(dtype.size) {
                buffer.extend(element.iter().rev());
            }
            writer.write_all(&buffer)?;
        }
    } else {
        let count = variable.data.len() / dtype.size;
        for start in (0..count).step_by(BLOCK) {
            buffer.clear();
            for i in start..(start + BLOCK).min(count) {
                variable
                    .nc_type
                    .write_value(&mut buffer, dtype.get_f64(&variable.data, i));
            }
            writer.write_all(&buffer)?;
        }
    }
    Ok(())
}
