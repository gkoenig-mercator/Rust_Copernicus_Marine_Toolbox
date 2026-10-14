//! Pure Rust decompression of the Zarr v2 codecs used by the Marine Data Store:
//! blosc (lz4, zlib, zstd, blosclz + byte shuffle), zlib, gzip, zstd and lz4.

use std::io::Read;

use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum Compressor {
    Blosc,
    Zlib,
    Gzip,
    Zstd,
    Lz4,
}

impl Compressor {
    pub fn from_zarr_metadata(compressor: &Value) -> Result<Option<Self>> {
        if compressor.is_null() {
            return Ok(None);
        }
        Ok(Some(match compressor["id"].as_str().unwrap_or_default() {
            "blosc" => Compressor::Blosc,
            "zlib" => Compressor::Zlib,
            "gzip" => Compressor::Gzip,
            "zstd" => Compressor::Zstd,
            "lz4" => Compressor::Lz4,
            other => bail!("Unsupported Zarr compressor '{other}'"),
        }))
    }

    pub fn decompress(&self, data: &[u8], expected_size: usize) -> Result<Vec<u8>> {
        match self {
            Compressor::Blosc => blosc_decompress(data),
            Compressor::Zlib => read_all(flate2::read::ZlibDecoder::new(data), expected_size),
            Compressor::Gzip => read_all(flate2::read::GzDecoder::new(data), expected_size),
            Compressor::Zstd => zstd_decompress(data, expected_size),
            Compressor::Lz4 => {
                // numcodecs LZ4: 4 bytes little-endian uncompressed size + LZ4 block
                ensure!(data.len() >= 4, "Invalid lz4 chunk");
                let size = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
                Ok(lz4_flex::block::decompress(&data[4..], size)?)
            }
        }
    }
}

fn read_all(mut reader: impl Read, expected_size: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected_size);
    reader.read_to_end(&mut out)?;
    Ok(out)
}

fn zstd_decompress(data: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    let decoder = ruzstd::StreamingDecoder::new(data).map_err(|e| anyhow::anyhow!("zstd: {e}"))?;
    read_all(decoder, expected_size)
}

// ---------------------------------------------------------------------------
// Blosc (format version 2, as written by c-blosc 1.x / numcodecs)
// ---------------------------------------------------------------------------

const BLOSC_DOSHUFFLE: u8 = 0x01;
const BLOSC_MEMCPYED: u8 = 0x02;
const BLOSC_DOBITSHUFFLE: u8 = 0x04;
const BLOSC_DONT_SPLIT: u8 = 0x10;
const BLOSC_MAX_SPLITS: usize = 16;
const BLOSC_MIN_BUFFERSIZE: usize = 128;
const BLOSC_HEADER_SIZE: usize = 16;

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    data.get(offset..offset + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .context("Truncated blosc buffer")
}

pub fn blosc_decompress(src: &[u8]) -> Result<Vec<u8>> {
    ensure!(src.len() >= BLOSC_HEADER_SIZE, "Blosc buffer too small");
    let flags = src[2];
    let typesize = (src[3] as usize).max(1);
    let nbytes = read_u32(src, 4)? as usize;
    let blocksize = read_u32(src, 8)? as usize;
    let cbytes = read_u32(src, 12)? as usize;
    ensure!(cbytes <= src.len(), "Truncated blosc buffer");
    let mut dest = vec![0u8; nbytes];
    if nbytes == 0 {
        return Ok(dest);
    }
    if flags & BLOSC_MEMCPYED != 0 {
        dest.copy_from_slice(
            src.get(BLOSC_HEADER_SIZE..BLOSC_HEADER_SIZE + nbytes)
                .context("Truncated blosc buffer")?,
        );
        return Ok(dest);
    }
    ensure!(blocksize > 0, "Invalid blosc block size");
    if flags & BLOSC_DOBITSHUFFLE != 0 && typesize > 1 {
        bail!("Blosc bitshuffle is not supported yet");
    }
    let compformat = (flags & 0xe0) >> 5;
    let do_shuffle = flags & BLOSC_DOSHUFFLE != 0 && typesize > 1;
    let dont_split = flags & BLOSC_DONT_SPLIT != 0;
    let nblocks = nbytes.div_ceil(blocksize);
    let leftover = nbytes % blocksize;
    let mut tmp = vec![0u8; blocksize];

    for block in 0..nblocks {
        let is_leftover_block = block == nblocks - 1 && leftover > 0;
        let bsize = if is_leftover_block {
            leftover
        } else {
            blocksize
        };
        let mut offset = read_u32(src, BLOSC_HEADER_SIZE + 4 * block)? as usize;
        let nsplits = if !dont_split
            && typesize <= BLOSC_MAX_SPLITS
            && bsize / typesize >= BLOSC_MIN_BUFFERSIZE
            && !is_leftover_block
        {
            typesize
        } else {
            1
        };
        let split_size = bsize / nsplits;
        let target = if do_shuffle {
            &mut tmp[..bsize]
        } else {
            let start = block * blocksize;
            &mut dest[start..start + bsize]
        };
        for split in 0..nsplits {
            let csize = read_u32(src, offset)? as usize;
            offset += 4;
            let compressed = src
                .get(offset..offset + csize)
                .context("Truncated blosc block")?;
            let out = &mut target[split * split_size..(split + 1) * split_size];
            if csize == split_size {
                out.copy_from_slice(compressed);
            } else {
                decompress_blosc_split(compformat, compressed, out)?;
            }
            offset += csize;
        }
        if do_shuffle {
            let start = block * blocksize;
            unshuffle(typesize, &tmp[..bsize], &mut dest[start..start + bsize]);
        }
    }
    Ok(dest)
}

fn decompress_blosc_split(compformat: u8, src: &[u8], out: &mut [u8]) -> Result<()> {
    match compformat {
        0 => blosclz_decompress(src, out),
        1 => {
            let n = lz4_flex::block::decompress_into(src, out)?;
            ensure!(n == out.len(), "lz4: unexpected decompressed size");
            Ok(())
        }
        3 => {
            let mut decoder = flate2::read::ZlibDecoder::new(src);
            decoder.read_exact(out)?;
            Ok(())
        }
        4 => {
            let mut decoder =
                ruzstd::StreamingDecoder::new(src).map_err(|e| anyhow::anyhow!("zstd: {e}"))?;
            decoder.read_exact(out)?;
            Ok(())
        }
        2 => bail!("Blosc snappy compression is not supported"),
        other => bail!("Unknown blosc compression format {other}"),
    }
}

fn unshuffle(typesize: usize, src: &[u8], dest: &mut [u8]) {
    let n = src.len() / typesize;
    for j in 0..typesize {
        let stream = &src[j * n..(j + 1) * n];
        for (i, byte) in stream.iter().enumerate() {
            dest[i * typesize + j] = *byte;
        }
    }
    let done = n * typesize;
    dest[done..].copy_from_slice(&src[done..]);
}

/// BloscLZ decoder (derived from FastLZ).
fn blosclz_decompress(src: &[u8], out: &mut [u8]) -> Result<()> {
    const MAX_DISTANCE: usize = 8191;
    let err = || anyhow::anyhow!("Corrupted blosclz stream");
    let mut ip = 0usize;
    let mut op = 0usize;
    let mut ctrl = (*src.first().ok_or_else(err)? & 31) as usize;
    ip += 1;
    loop {
        if ctrl >= 32 {
            let mut len = (ctrl >> 5) - 1;
            let mut ofs = (ctrl & 31) << 8;
            if len == 6 {
                loop {
                    let code = *src.get(ip).ok_or_else(err)?;
                    ip += 1;
                    len += code as usize;
                    if code != 255 {
                        break;
                    }
                }
            }
            let code = *src.get(ip).ok_or_else(err)? as usize;
            ip += 1;
            len += 3;
            let mut distance = ofs + code;
            if code == 255 && ofs == (31 << 8) {
                ofs = (*src.get(ip).ok_or_else(err)? as usize) << 8;
                ofs += *src.get(ip + 1).ok_or_else(err)? as usize;
                ip += 2;
                distance = ofs + MAX_DISTANCE;
            }
            // the reference is one byte before `op - distance`
            let reference = op.checked_sub(distance + 1).ok_or_else(err)?;
            if op + len > out.len() {
                return Err(err());
            }
            for k in 0..len {
                out[op + k] = out[reference + k];
            }
            op += len;
        } else {
            let literal = ctrl + 1;
            let bytes = src.get(ip..ip + literal).ok_or_else(err)?;
            out.get_mut(op..op + literal)
                .ok_or_else(err)?
                .copy_from_slice(bytes);
            ip += literal;
            op += literal;
        }
        if ip >= src.len() {
            break;
        }
        ctrl = src[ip] as usize;
        ip += 1;
    }
    ensure!(op == out.len(), "blosclz: unexpected decompressed size");
    Ok(())
}
