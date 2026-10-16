//! Block compression: codec selection and the codecs themselves.

use crate::format::*;
use anyhow::{bail, Result};
use rayon::prelude::*;
use std::io::{Read, Write};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Kind {
    Text,
    Binary,
    Executable,
    Incompressible,
}

#[derive(Clone, Copy, PartialEq, Debug, clap::ValueEnum)]
pub enum Mode {
    /// zstd level 3 — very fast
    Fast,
    /// zstd level 19 with long-distance matching
    Balanced,
    /// zstd 22 or LZMA2 9e (+BCJ for executables), picked per block from a sample
    Max,
    /// like max, but fully compresses every block with every codec and keeps the smallest
    Exhaustive,
}

pub fn classify(ext: &str, head: &[u8]) -> Kind {
    const INCOMP: &[&str] = &[
        "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif", "jxl", "mp3", "aac", "m4a", "ogg",
        "opus", "flac", "mp4", "m4v", "mkv", "mov", "webm", "avi", "zip", "gz", "tgz", "bz2", "xz", "zst",
        "7z", "rar", "jar", "apk", "woff", "woff2", "br", "lz4", "docx", "xlsx", "pptx", "odt", "snugly",
    ];
    if INCOMP.contains(&ext) {
        return Kind::Incompressible;
    }
    if head.starts_with(b"\x7fELF")
        || head.starts_with(b"MZ")
        || head.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
        || head.starts_with(&[0xca, 0xfe, 0xba, 0xbe])
        || matches!(ext, "exe" | "dll" | "so" | "o" | "a" | "dylib" | "node" | "wasm")
    {
        return Kind::Executable;
    }
    if head.is_empty() {
        return Kind::Text;
    }
    let printable = head
        .iter()
        .filter(|&&b| b == b'\n' || b == b'\r' || b == b'\t' || (0x20..0x7f).contains(&b) || b >= 0x80)
        .count();
    if printable * 100 / head.len() >= 95 && !head.contains(&0) {
        Kind::Text
    } else {
        Kind::Binary
    }
}

fn zstd_compress(data: &[u8], level: i32) -> Result<Vec<u8>> {
    use zstd::zstd_safe::CParameter;
    let mut c = zstd::bulk::Compressor::new(level)?;
    let wlog = (64 - (data.len().max(1 << 16) as u64 - 1).leading_zeros()).clamp(17, 27);
    c.set_parameter(CParameter::WindowLog(wlog))?;
    c.set_parameter(CParameter::EnableLongDistanceMatching(true))?;
    Ok(c.compress(data)?)
}

fn xz_compress(data: &[u8], x86: bool) -> Result<Vec<u8>> {
    use xz2::stream::{Check, Filters, LzmaOptions, Stream};
    let mut opts = LzmaOptions::new_preset(9 | 0x8000_0000)?; // 9e
    let dict = (data.len() as u32).clamp(1 << 20, 1 << 27).next_power_of_two().min(1 << 27);
    opts.dict_size(dict);
    let mut filters = Filters::new();
    if x86 {
        filters.x86();
    }
    filters.lzma2(&opts);
    let stream = Stream::new_stream_encoder(&filters, Check::None)?;
    let mut enc = xz2::write::XzEncoder::new_stream(Vec::new(), stream);
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

fn run_codec(codec: u8, data: &[u8]) -> Result<Vec<u8>> {
    match codec {
        CODEC_ZSTD => zstd_compress(data, 22),
        CODEC_XZ => xz_compress(data, false),
        CODEC_XZ_X86 => xz_compress(data, true),
        _ => unreachable!(),
    }
}

/// Takes ownership so a stored block is returned as-is instead of being copied.
pub fn decompress(codec: u8, data: Vec<u8>, raw_len: usize) -> Result<Vec<u8>> {
    if codec == CODEC_STORE {
        if data.len() != raw_len {
            bail!("block length mismatch");
        }
        return Ok(data);
    }
    let data = data.as_slice();
    let out = match codec {
        CODEC_ZSTD => {
            let mut d = zstd::stream::read::Decoder::new(data)?;
            d.window_log_max(31)?;
            let mut v = Vec::with_capacity(raw_len.min(data.len().saturating_mul(64)).min(256 << 20));
            d.take(raw_len as u64 + 1).read_to_end(&mut v)?;
            v
        }
        CODEC_XZ | CODEC_XZ_X86 => {
            let mut v = Vec::with_capacity(raw_len.min(data.len().saturating_mul(64)).min(256 << 20));
            xz2::read::XzDecoder::new(data).take(raw_len as u64 + 1).read_to_end(&mut v)?;
            v
        }
        c => bail!("unknown codec {c}"),
    };
    if out.len() != raw_len {
        bail!("block length mismatch");
    }
    Ok(out)
}

/// Evenly spaced slices of the block, used to predict which codec wins.
fn sample_of(raw: &[u8]) -> Vec<u8> {
    const SLICES: usize = 4;
    const SLICE: usize = 2 << 20;
    let step = raw.len() / SLICES;
    (0..SLICES).flat_map(|i| &raw[i * step..(i * step + SLICE).min(raw.len())]).copied().collect()
}

/// Pick the best codec for a raw block. Returns (codec, compressed bytes).
pub fn compress_block(raw: &[u8], kind: Kind, mode: Mode) -> Result<(u8, Vec<u8>)> {
    // Cheap entropy check: if a fast pass over a few slices saves under 1%, the block is
    // random/encrypted/already-compressed — store it instead of grinding a slow codec over it.
    if mode != Mode::Fast && raw.len() > 4 << 20 {
        let probe: Vec<u8> = (0..8)
            .flat_map(|i| {
                let s = i * (raw.len() / 8);
                &raw[s..(s + (256 << 10)).min(raw.len())]
            })
            .copied()
            .collect();
        if zstd::bulk::compress(&probe, 1)?.len() as f64 > probe.len() as f64 * 0.99 {
            return Ok((CODEC_STORE, raw.to_vec()));
        }
    }
    let best = match (mode, kind) {
        // Media is mostly incompressible, but containers (HEIC/MOV metadata, index tables) still
        // give LZMA a few percent, so only max/exhaustive spend the time on it.
        (Mode::Fast | Mode::Balanced, Kind::Incompressible) | (Mode::Fast, _) => (CODEC_ZSTD, zstd_compress(raw, 3)?),
        (Mode::Balanced, _) => (CODEC_ZSTD, zstd_compress(raw, 19)?),
        (Mode::Max | Mode::Exhaustive, k) => {
            let mut codecs = vec![CODEC_ZSTD, CODEC_XZ];
            if k == Kind::Executable {
                codecs.push(CODEC_XZ_X86);
            }
            if mode == Mode::Max && raw.len() > 12 << 20 {
                // Score each codec on a sample; fully run the winner, plus the
                // runner-up only if the sample can't separate them (within 1.5%).
                let s = sample_of(raw);
                let mut scored: Vec<(usize, u8)> =
                    codecs.par_iter().map(|&c| Ok((run_codec(c, &s)?.len(), c))).collect::<Result<_>>()?;
                scored.sort();
                codecs = scored
                    .iter()
                    .filter(|(sz, _)| (*sz as f64) <= scored[0].0 as f64 * 1.015)
                    .map(|&(_, c)| c)
                    .collect();
            }
            codecs
                .par_iter()
                .map(|&c| Ok((c, run_codec(c, raw)?)))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .min_by_key(|(_, v)| v.len())
                .unwrap()
        }
    };
    if best.1.len() as f64 >= raw.len() as f64 * 0.985 {
        return Ok((CODEC_STORE, raw.to_vec()));
    }
    Ok(best)
}
