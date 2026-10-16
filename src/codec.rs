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
    /// arm64 machine code (Apple Silicon Mach-O, aarch64 ELF, ARM64 PE).
    ExecutableArm64,
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
        return if is_arm64(head) { Kind::ExecutableArm64 } else { Kind::Executable };
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

/// `workers` > 1 splits the block into jobs compressed in parallel by zstd itself. Jobs overlap
/// so each one still sees the data before it: on a 128 MiB text block, 32 MiB jobs with a
/// 1/2-window overlap cost about 0.1% in size, 64 MiB jobs with 1/4 about 0.03%. Used only when
/// there are fewer blocks than threads, so large archives are unaffected.
/// Whether an executable's header says it contains arm64 code. Universal (fat) Mach-O files
/// count as arm64 if any slice is: on Apple Silicon Macs that is the slice that matters, and in
/// max mode the per-block codec trial still catches blocks where the x86 filter does better.
fn is_arm64(h: &[u8]) -> bool {
    let u16le = |i: usize| h.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32le = |i: usize| h.get(i..i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let u32be = |i: usize| h.get(i..i + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()));
    const ARM64: u32 = 0x0100_000c;
    if h.starts_with(b"\x7fELF") {
        // e_machine, in the file's byte order (EI_DATA: 1 little, 2 big). EM_AARCH64 = 183.
        let m = match h.get(5) {
            Some(1) => u16le(18),
            Some(2) => h.get(18..20).map(|b| u16::from_be_bytes([b[0], b[1]])),
            _ => None,
        };
        return m == Some(183);
    }
    if h.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
        return u32le(4) == Some(ARM64);
    }
    if h.starts_with(&[0xca, 0xfe, 0xba, 0xbe]) {
        // Fat Mach-O: big-endian count, then 20-byte entries starting with cputype. Java class
        // files share the magic but have a version number (45+) where the count would be.
        let Some(n) = u32be(4) else { return false };
        return (1..=8).contains(&n) && (0..n as usize).any(|i| u32be(8 + 20 * i) == Some(ARM64));
    }
    if h.starts_with(b"MZ") {
        // PE: e_lfanew -> "PE\0\0" + machine (IMAGE_FILE_MACHINE_ARM64 = 0xAA64).
        let Some(off) = u32le(0x3c).map(|o| o as usize) else { return false };
        return h.get(off..off + 4) == Some(b"PE\0\0") && u16le(off + 4) == Some(0xaa64);
    }
    false
}

fn zstd_compress(data: &[u8], level: i32, workers: u32) -> Result<Vec<u8>> {
    use zstd::zstd_safe::CParameter;
    let mut c = zstd::bulk::Compressor::new(level)?;
    let wlog = (64 - (data.len().max(1 << 16) as u64 - 1).leading_zeros()).clamp(17, 27);
    c.set_parameter(CParameter::WindowLog(wlog))?;
    c.set_parameter(CParameter::EnableLongDistanceMatching(true))?;
    if workers > 1 && data.len() >= 16 << 20 {
        let (job, overlap) = if workers > 2 { (32u32 << 20, 8) } else { (64 << 20, 7) };
        c.set_parameter(CParameter::NbWorkers(workers))?;
        c.set_parameter(CParameter::JobSize(job))?;
        c.set_parameter(CParameter::OverlapSizeLog(overlap))?;
    }
    Ok(c.compress(data)?)
}

#[derive(Clone, Copy, PartialEq)]
enum Bcj {
    None,
    X86,
    Arm64,
}

/// LZMA2 at `preset` (9e in max mode, 6 in balanced: on 128 MiB blocks with a dictionary the size
/// of the block they differ by under 0.2%), optionally behind a branch filter that turns
/// relative call/jump targets into absolute ones so repeated calls look alike. For arm64 the
/// literal context is set to lc=2, lp=2 (instructions are 4-byte aligned), worth about 1.4%.
fn xz_compress(data: &[u8], bcj: Bcj, preset: u32) -> Result<Vec<u8>> {
    use liblzma::stream::{Check, Filters, LzmaOptions, Stream};
    let mut opts = LzmaOptions::new_preset(preset)?;
    let dict = (data.len() as u32).clamp(1 << 20, 1 << 27).next_power_of_two().min(1 << 27);
    opts.dict_size(dict);
    let mut filters = Filters::new();
    match bcj {
        Bcj::None => {}
        Bcj::X86 => {
            filters.x86();
        }
        Bcj::Arm64 => {
            opts.literal_context_bits(2);
            opts.literal_position_bits(2);
            filters.arm64();
        }
    }
    filters.lzma2(&opts);
    let stream = Stream::new_stream_encoder(&filters, Check::None)?;
    let mut enc = liblzma::write::XzEncoder::new_stream(Vec::new(), stream);
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

fn run_codec(codec: u8, data: &[u8], workers: u32) -> Result<Vec<u8>> {
    match codec {
        CODEC_ZSTD => zstd_compress(data, 22, workers),
        CODEC_XZ => xz_compress(data, Bcj::None, MAX_PRESET),
        CODEC_XZ_X86 => xz_compress(data, Bcj::X86, MAX_PRESET),
        CODEC_XZ_ARM64 => xz_compress(data, Bcj::Arm64, MAX_PRESET),
        CODEC_BSC => crate::bsc::compress(data),
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
        CODEC_XZ | CODEC_XZ_X86 | CODEC_XZ_ARM64 => {
            let mut v = Vec::with_capacity(raw_len.min(data.len().saturating_mul(64)).min(256 << 20));
            liblzma::read::XzDecoder::new(data).take(raw_len as u64 + 1).read_to_end(&mut v)?;
            v
        }
        CODEC_BSC => crate::bsc::decompress(data, raw_len)?,
        c => bail!("unknown codec {c}"),
    };
    if out.len() != raw_len {
        bail!("block length mismatch");
    }
    Ok(out)
}

/// bsc must beat zstd -19 by this factor on the sample to be chosen (its decoding is much slower).
const BSC_MARGIN: f64 = 0.97;
/// Same rule for LZMA2 with a branch filter on executables (it encodes more slowly than zstd).
const XZ_MARGIN: f64 = 0.97;
const MAX_PRESET: u32 = 9 | 0x8000_0000; // 9e
const BALANCED_PRESET: u32 = 6;

/// Evenly spaced slices of the block, used to predict which codec wins.
fn sample_of(raw: &[u8]) -> Vec<u8> {
    const SLICES: usize = 4;
    const SLICE: usize = 2 << 20;
    let step = raw.len() / SLICES;
    (0..SLICES).flat_map(|i| &raw[i * step..(i * step + SLICE).min(raw.len())]).copied().collect()
}

/// Pick the best codec for a raw block. Returns (codec, compressed bytes).
/// `workers` is how many threads this block may use (see `zstd_compress`).
pub fn compress_block(raw: &[u8], kind: Kind, mode: Mode, workers: u32) -> Result<(u8, Vec<u8>)> {
    // Cheap entropy check: if a fast pass over a few slices saves under 1%, the block is
    // random/encrypted/already-compressed — store it instead of grinding a slow codec over it.
    if mode != Mode::Fast && raw.len() > 4 << 20 {
        let t = std::time::Instant::now();
        let probe: Vec<u8> = (0..8)
            .flat_map(|i| {
                let s = i * (raw.len() / 8);
                &raw[s..(s + (256 << 10)).min(raw.len())]
            })
            .copied()
            .collect();
        let incompressible = zstd::bulk::compress(&probe, 1)?.len() as f64 > probe.len() as f64 * 0.99;
        crate::stats::S.probe.add(t, probe.len() as u64, 0);
        if incompressible {
            return Ok((CODEC_STORE, raw.to_vec()));
        }
    }
    let best = match (mode, kind) {
        // Media is mostly incompressible, but containers (HEIC/MOV metadata, index tables) still
        // give LZMA a few percent, so only max/exhaustive spend the time on it.
        (Mode::Fast | Mode::Balanced, Kind::Incompressible) | (Mode::Fast, _) => (CODEC_ZSTD, zstd_compress(raw, 3, workers)?),
        // Text and other binary data: libbsc (BWT) compresses ~10x faster than zstd -19 but
        // decodes ~20x slower, and only wins on size for some data (prose, XML, databases);
        // zstd's long-range matching wins on repetitive data such as source trees. Use bsc only
        // where a sample shows it clearly smaller.
        (Mode::Balanced, Kind::Text | Kind::Binary) if raw.len() > 12 << 20 => {
            let s = sample_of(raw);
            let (b, z) = rayon::join(|| crate::bsc::compress(&s).map(|v| v.len()), || zstd_compress(&s, 19, 1).map(|v| v.len()));
            match b {
                Ok(b) if (b as f64) <= z? as f64 * BSC_MARGIN => match crate::bsc::compress(raw) {
                    Ok(v) => (CODEC_BSC, v),
                    Err(_) => (CODEC_ZSTD, zstd_compress(raw, 19, workers)?),
                },
                _ => (CODEC_ZSTD, zstd_compress(raw, 19, workers)?),
            }
        }
        // Executables: LZMA2 behind the matching branch filter usually beats zstd -19 by 10–30%
        // (arm64 most of all); decide on a sample.
        (Mode::Balanced, Kind::Executable | Kind::ExecutableArm64) => {
            let (bcj, codec) = if kind == Kind::ExecutableArm64 { (Bcj::Arm64, CODEC_XZ_ARM64) } else { (Bcj::X86, CODEC_XZ_X86) };
            let s = if raw.len() > 12 << 20 { sample_of(raw) } else { raw.to_vec() };
            let (x, z) = rayon::join(|| xz_compress(&s, bcj, BALANCED_PRESET).map(|v| v.len()), || zstd_compress(&s, 19, 1).map(|v| v.len()));
            if (x? as f64) <= z? as f64 * XZ_MARGIN {
                (codec, xz_compress(raw, bcj, BALANCED_PRESET)?)
            } else {
                (CODEC_ZSTD, zstd_compress(raw, 19, workers)?)
            }
        }
        (Mode::Balanced, _) => (CODEC_ZSTD, zstd_compress(raw, 19, workers)?),
        (Mode::Max | Mode::Exhaustive, k) => {
            let mut codecs = vec![CODEC_ZSTD, CODEC_XZ];
            match k {
                Kind::Executable => codecs.push(CODEC_XZ_X86),
                Kind::ExecutableArm64 => codecs.push(CODEC_XZ_ARM64),
                _ => codecs.push(CODEC_BSC),
            }
            if mode == Mode::Max && raw.len() > 12 << 20 {
                // Score each codec on a sample; fully run the winner, plus the
                // runner-up only if the sample can't separate them (within 1.5%).
                let s = sample_of(raw);
                let mut scored: Vec<(usize, u8)> =
                    codecs.par_iter().map(|&c| Ok((run_codec(c, &s, 1)?.len(), c))).collect::<Result<_>>()?;
                scored.sort();
                codecs = scored
                    .iter()
                    .filter(|(sz, _)| (*sz as f64) <= scored[0].0 as f64 * 1.015)
                    .map(|&(_, c)| c)
                    .collect();
            }
            codecs
                .par_iter()
                .map(|&c| Ok((c, run_codec(c, raw, workers)?)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executables_are_grouped_by_processor() {
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[5] = 1; // little-endian
        elf[18] = 183; // EM_AARCH64
        assert_eq!(classify("", &elf), Kind::ExecutableArm64);
        elf[18] = 62; // EM_X86_64
        assert_eq!(classify("", &elf), Kind::Executable);

        let mut macho = vec![0xcf, 0xfa, 0xed, 0xfe];
        macho.extend_from_slice(&0x0100_000cu32.to_le_bytes());
        assert_eq!(classify("", &macho), Kind::ExecutableArm64);

        // Universal binary with x86_64 and arm64 slices.
        let mut fat = vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2];
        for cpu in [0x0100_0007u32, 0x0100_000c] {
            fat.extend_from_slice(&cpu.to_be_bytes());
            fat.extend_from_slice(&[0; 16]);
        }
        assert_eq!(classify("", &fat), Kind::ExecutableArm64);
        // A Java class file shares the magic; its version (here 52) is not a slice count.
        assert_eq!(classify("class", &[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52]), Kind::Executable);

        let mut pe = vec![0u8; 0x100];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c] = 0x80;
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0xaa64u16.to_le_bytes());
        assert_eq!(classify("exe", &pe), Kind::ExecutableArm64);
        pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        assert_eq!(classify("exe", &pe), Kind::Executable);
    }

    #[test]
    fn every_lzma_variant_round_trips() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8 & 0x3f).collect();
        for codec in [CODEC_XZ, CODEC_XZ_X86, CODEC_XZ_ARM64] {
            let c = run_codec(codec, &data, 1).unwrap();
            assert_eq!(decompress(codec, c, data.len()).unwrap(), data, "codec {codec}");
        }
        for (bcj, codec) in [(Bcj::X86, CODEC_XZ_X86), (Bcj::Arm64, CODEC_XZ_ARM64)] {
            let c = xz_compress(&data, bcj, BALANCED_PRESET).unwrap();
            assert_eq!(decompress(codec, c, data.len()).unwrap(), data);
        }
    }
}
