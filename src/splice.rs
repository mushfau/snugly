//! "Spliced" recompression for containers that embed deflate streams: ZIP-family files
//! (docx/xlsx/pptx/odt/epub/jar/zip/whl/apk) and PNG.
//!
//! The file is viewed as a *skeleton* (headers, directory, stored entries, chunk framing)
//! with *pieces* cut out of it. The concatenated pieces form a payload made of streams, each
//! `prefix + deflate + suffix` (for PNG: zlib header + deflate + adler32, spread over IDAT
//! chunks; for ZIP: one bare deflate stream per entry). Every deflate stream is inflated with
//! preflate, which also records the tiny "corrections" needed to rebuild the identical bytes.
//!
//! Transformed layout: `[plain 0][plain 1]…[meta][meta_len u32 LE]`, plain data first so the
//! solid compressor sees it as ordinary text/binary. A stream that can't be rebuilt exactly,
//! or whose corrections would be too large, simply stays in the skeleton untouched.

use crate::format::{get_varint, put_varint};
use anyhow::{anyhow, bail, Context, Result};

/// Hard ceilings so a hostile archive can't make the inverse allocate absurd amounts.
const MAX_OUT: u64 = 1 << 34;
const MAX_STREAMS: u64 = 1 << 20;

pub fn is_zip(d: &[u8]) -> bool {
    d.len() >= 30 && d.starts_with(b"PK\x03\x04")
}
pub fn is_png(d: &[u8]) -> bool {
    d.len() >= 33 && d.starts_with(b"\x89PNG\r\n\x1a\n")
}

struct Stream {
    prefix: Vec<u8>,
    suffix: Vec<u8>,
    plain: Vec<u8>,
    corr: Vec<u8>,
}

fn le16(d: &[u8], i: usize) -> Option<usize> {
    Some(u16::from_le_bytes(d.get(i..i + 2)?.try_into().ok()?) as usize)
}
fn le32(d: &[u8], i: usize) -> Option<usize> {
    Some(u32::from_le_bytes(d.get(i..i + 4)?.try_into().ok()?) as usize)
}
fn be32(d: &[u8], i: usize) -> Option<usize> {
    Some(u32::from_be_bytes(d.get(i..i + 4)?.try_into().ok()?) as usize)
}

/// Inflate one deflate stream with preflate; None unless it rebuilds exactly and cheaply.
fn inflate(deflate: &[u8], careful: bool) -> Option<(Vec<u8>, Vec<u8>)> {
    if deflate.len() < 16 {
        return None;
    }
    let cfg = crate::recomp::preflate_config(careful);
    let r = std::panic::catch_unwind(|| preflate_rs::preflate_whole_deflate_stream(deflate, &cfg)).ok()?;
    let (res, plain) = r.ok()?;
    if res.compressed_size != deflate.len() || res.corrections.len() > deflate.len() / 2 {
        return None;
    }
    Some((plain.text().to_vec(), res.corrections))
}

/// Deflate entries of a ZIP, from its central directory: (offset, length) in file order.
fn zip_deflate_ranges(d: &[u8]) -> Option<Vec<(usize, usize)>> {
    let tail_start = d.len().saturating_sub(65_557);
    let eocd = (tail_start..d.len().saturating_sub(21)).rev().find(|&i| d[i..].starts_with(b"PK\x05\x06"))?;
    let entries = le16(d, eocd + 10)?;
    let cd_off = le32(d, eocd + 16)?;
    if cd_off == 0xFFFF_FFFF || entries == 0xFFFF {
        return None; // ZIP64: not handled
    }
    let mut p = cd_off;
    let mut out = Vec::new();
    for _ in 0..entries {
        if !d.get(p..)?.starts_with(b"PK\x01\x02") {
            return None;
        }
        let flags = le16(d, p + 8)?;
        let method = le16(d, p + 10)?;
        let csize = le32(d, p + 20)?;
        let (n, e, c) = (le16(d, p + 28)?, le16(d, p + 30)?, le16(d, p + 32)?);
        let local = le32(d, p + 42)?;
        p += 46 + n + e + c;
        if method != 8 || flags & 1 != 0 || csize == 0 || csize == 0xFFFF_FFFF {
            continue; // stored, encrypted or zip64 entries stay in the skeleton
        }
        if !d.get(local..)?.starts_with(b"PK\x03\x04") {
            return None;
        }
        let data = local + 30 + le16(d, local + 26)? + le16(d, local + 28)?;
        if data.checked_add(csize)? > d.len() {
            return None;
        }
        out.push((data, csize));
    }
    out.sort();
    if out.windows(2).any(|w| w[0].0 + w[0].1 > w[1].0) {
        return None;
    }
    Some(out)
}

/// PNG: IDAT data ranges (the zlib stream, chunked).
fn png_idat_ranges(d: &[u8]) -> Option<Vec<(usize, usize)>> {
    let mut p = 8;
    let mut out = Vec::new();
    while p + 12 <= d.len() {
        let len = be32(d, p)?;
        let ty = d.get(p + 4..p + 8)?;
        let data = p + 8;
        if data.checked_add(len)?.checked_add(4)? > d.len() {
            return None;
        }
        if ty == b"IDAT" && len > 0 {
            out.push((data, len));
        }
        p = data + len + 4;
        if ty == b"IEND" {
            break;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// `careful` makes preflate verify each stream itself, so one stream that can't be rebuilt
/// is left in the skeleton instead of failing the whole file's end-to-end check.
pub fn forward(d: &[u8], careful: bool) -> Option<Vec<u8>> {
    let (streams, pieces): (Vec<Stream>, Vec<(usize, usize)>) = if is_png(d) {
        let ranges = png_idat_ranges(d)?;
        let payload: Vec<u8> = ranges.iter().flat_map(|&(o, l)| &d[o..o + l]).copied().collect();
        if payload.len() < 2 + 16 + 4 || payload[0] & 0x0f != 8 || payload[1] & 0x20 != 0 {
            return None;
        }
        let def = &payload[2..payload.len() - 4];
        let (plain, corr) = inflate(def, careful)?;
        (vec![Stream { prefix: payload[..2].to_vec(), suffix: payload[payload.len() - 4..].to_vec(), plain, corr }], ranges)
    } else if is_zip(d) {
        let mut streams = Vec::new();
        let mut pieces = Vec::new();
        for (o, l) in zip_deflate_ranges(d)? {
            if let Some((plain, corr)) = inflate(&d[o..o + l], careful) {
                streams.push(Stream { prefix: vec![], suffix: vec![], plain, corr });
                pieces.push((o, l));
            }
        }
        if streams.is_empty() {
            return None;
        }
        (streams, pieces)
    } else {
        return None;
    };

    let mut meta = Vec::new();
    put_varint(&mut meta, d.len() as u64);
    put_varint(&mut meta, streams.len() as u64);
    for s in &streams {
        put_varint(&mut meta, s.plain.len() as u64);
        for b in [&s.corr, &s.prefix, &s.suffix] {
            put_varint(&mut meta, b.len() as u64);
            meta.extend_from_slice(b);
        }
    }
    put_varint(&mut meta, pieces.len() as u64);
    let mut cursor = 0;
    let mut skeleton = Vec::with_capacity(d.len() / 4);
    for &(o, l) in &pieces {
        put_varint(&mut meta, (o - cursor) as u64);
        put_varint(&mut meta, l as u64);
        skeleton.extend_from_slice(&d[cursor..o]);
        cursor = o + l;
    }
    skeleton.extend_from_slice(&d[cursor..]);
    meta.extend_from_slice(&skeleton);

    let mut out = Vec::with_capacity(streams.iter().map(|s| s.plain.len()).sum::<usize>() + meta.len() + 4);
    for s in &streams {
        out.extend_from_slice(&s.plain);
    }
    out.extend_from_slice(&meta);
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    Some(out)
}

pub fn inverse(t: &[u8]) -> Result<Vec<u8>> {
    let n = t.len();
    if n < 4 {
        bail!("truncated splice transform");
    }
    let mlen = u32::from_le_bytes(t[n - 4..].try_into()?) as usize;
    let mstart = n.checked_sub(4 + mlen).context("bad splice transform")?;
    let meta = &t[mstart..n - 4];
    let mut q = 0;
    let total = get_varint(meta, &mut q)?;
    let nstreams = get_varint(meta, &mut q)?;
    if total > MAX_OUT || nstreams > MAX_STREAMS {
        bail!("splice transform out of range");
    }
    let take = |q: &mut usize| -> Result<&[u8]> {
        let l = get_varint(meta, q)? as usize;
        let s = meta.get(*q..q.checked_add(l).context("overflow")?).context("truncated")?;
        *q += l;
        Ok(s)
    };

    // Rebuild the payload: prefix + deflate(plain, corrections) + suffix, stream by stream.
    let mut payload = Vec::new();
    let mut plain_pos = 0usize;
    for _ in 0..nstreams {
        let pl = get_varint(meta, &mut q)? as usize;
        let corr = take(&mut q)?;
        let prefix = take(&mut q)?;
        let suffix = take(&mut q)?;
        let plain = t.get(plain_pos..plain_pos.checked_add(pl).context("overflow")?).context("truncated")?;
        if plain_pos + pl > mstart {
            bail!("plain data overlaps metadata");
        }
        plain_pos += pl;
        let def = std::panic::catch_unwind(|| preflate_rs::recreate_whole_deflate_stream(plain, corr))
            .map_err(|_| anyhow!("preflate panicked on corrupt data"))?
            .map_err(|e| anyhow!("preflate: {e:?}"))?;
        payload.extend_from_slice(prefix);
        payload.extend_from_slice(&def);
        payload.extend_from_slice(suffix);
        if payload.len() as u64 > total {
            bail!("splice payload larger than the file");
        }
    }

    // Interleave skeleton and payload pieces.
    let npieces = get_varint(meta, &mut q)?;
    let mut spans = Vec::with_capacity((npieces as usize).min(1 << 20));
    for _ in 0..npieces {
        spans.push((get_varint(meta, &mut q)? as usize, get_varint(meta, &mut q)? as usize));
    }
    let skeleton = &meta[q..];
    // Never trust the recorded total for the allocation: it can't exceed what we actually hold.
    let mut out = Vec::with_capacity((total as usize).min(payload.len() + skeleton.len()));
    let (mut sk, mut pp) = (0usize, 0usize);
    for (gap, len) in spans {
        out.extend_from_slice(skeleton.get(sk..sk.checked_add(gap).context("overflow")?).context("bad splice skeleton")?);
        sk += gap;
        out.extend_from_slice(payload.get(pp..pp.checked_add(len).context("overflow")?).context("bad splice pieces")?);
        pp += len;
        if out.len() as u64 > total {
            bail!("splice output larger than recorded");
        }
    }
    out.extend_from_slice(&skeleton[sk..]);
    if pp != payload.len() || out.len() as u64 != total {
        bail!("splice transform doesn't add up");
    }
    Ok(out)
}
