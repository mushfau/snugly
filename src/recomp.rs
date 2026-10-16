//! Reversible "recompression" transforms.
//!
//! Files that are already compressed (gzip, JPEG) defeat general-purpose compressors.
//! These transforms turn them into something much more compressible, and record exactly
//! what is needed to rebuild the original bytes bit-for-bit:
//!
//! * gzip: each deflate stream is inflated with `preflate-rs`, which also records a
//!   small "corrections" stream describing how the original encoder made its choices,
//!   so the identical deflate bitstream can be regenerated.
//! * JPEG: `lepton_jpeg` re-encodes the Huffman-coded DCT coefficients with a much
//!   better arithmetic coder (~20% smaller) and decodes back to the identical file.
//!
//! Every transform is verified at archive time by running the inverse and comparing
//! bytes; anything that doesn't round-trip exactly is stored untransformed.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Cursor;

pub const T_NONE: u8 = 0;
pub const T_GZIP: u8 = 1;
pub const T_JPEG: u8 = 2;
pub const T_SPLICE: u8 = 3;

use crate::format::{get_varint, put_varint};

pub fn is_gzip(d: &[u8]) -> bool {
    d.len() >= 18 && d[0] == 0x1f && d[1] == 0x8b && d[2] == 8
}
pub fn is_jpeg(d: &[u8]) -> bool {
    d.len() >= 4 && d[0] == 0xff && d[1] == 0xd8 && d[2] == 0xff
}

/// Length of a gzip member header, or None if it isn't one we understand.
fn gzip_header_len(d: &[u8]) -> Option<usize> {
    if !is_gzip(d) {
        return None;
    }
    let fl = d[3];
    if fl & 0xe0 != 0 {
        return None;
    }
    let mut p = 10;
    if fl & 4 != 0 {
        let xl = u16::from_le_bytes([*d.get(p)?, *d.get(p + 1)?]) as usize;
        p += 2 + xl;
    }
    if fl & 8 != 0 {
        p += d.get(p..)?.iter().position(|&b| b == 0)? + 1;
    }
    if fl & 16 != 0 {
        p += d.get(p..)?.iter().position(|&b| b == 0)? + 1;
    }
    if fl & 2 != 0 {
        p += 2;
    }
    (p < d.len()).then_some(p)
}

/// Layout: [plain text of every member, concatenated] [meta] [meta_len: u32 LE]
/// meta = n_members, per member {hdr, corrections, 8-byte trailer, plain_len}, trailing garbage.
/// Plain text goes first so the classifier and the solid compressor see it as ordinary data.
pub fn gzip_forward(d: &[u8]) -> Option<Vec<u8>> {
    let cfg = preflate_rs::PreflateConfig::default();
    let mut plain_all = Vec::new();
    let mut meta = Vec::new();
    let mut members = 0u64;
    let mut member_meta = Vec::new();
    let mut p = 0;
    loop {
        let h = gzip_header_len(&d[p..])?;
        let (res, plain) = preflate_rs::preflate_whole_deflate_stream(&d[p + h..], &cfg).ok()?;
        let end = p + h + res.compressed_size;
        if end + 8 > d.len() {
            return None;
        }
        let text = plain.text();
        put_varint(&mut member_meta, h as u64);
        member_meta.extend_from_slice(&d[p..p + h]);
        put_varint(&mut member_meta, res.corrections.len() as u64);
        member_meta.extend_from_slice(&res.corrections);
        member_meta.extend_from_slice(&d[end..end + 8]);
        put_varint(&mut member_meta, text.len() as u64);
        plain_all.extend_from_slice(text);
        members += 1;
        p = end + 8;
        if p >= d.len() || members >= 1024 || gzip_header_len(&d[p..]).is_none() {
            break;
        }
    }
    put_varint(&mut meta, members);
    meta.extend_from_slice(&member_meta);
    put_varint(&mut meta, (d.len() - p) as u64);
    meta.extend_from_slice(&d[p..]);
    let mut out = plain_all;
    out.extend_from_slice(&meta);
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    Some(out)
}

fn gzip_inverse(t: &[u8]) -> Result<Vec<u8>> {
    let n = t.len();
    if n < 4 {
        bail!("truncated gzip transform");
    }
    let mlen = u32::from_le_bytes(t[n - 4..].try_into()?) as usize;
    let mstart = n.checked_sub(4 + mlen).context("bad gzip transform")?;
    let meta = &t[mstart..n - 4];
    let mut plain_pos = 0usize;
    let mut q = 0usize;
    let mut out = Vec::new();
    let members = get_varint(meta, &mut q)?;
    for _ in 0..members {
        let h = get_varint(meta, &mut q)? as usize;
        out.extend_from_slice(meta.get(q..q + h).context("truncated")?);
        q += h;
        let cl = get_varint(meta, &mut q)? as usize;
        let corr = meta.get(q..q + cl).context("truncated")?;
        q += cl;
        let trailer = meta.get(q..q + 8).context("truncated")?;
        q += 8;
        let pl = get_varint(meta, &mut q)? as usize;
        let plain = t.get(plain_pos..plain_pos + pl).context("truncated")?;
        plain_pos += pl;
        let deflate = preflate_rs::recreate_whole_deflate_stream(plain, corr)
            .map_err(|e| anyhow!("preflate: {e:?}"))?;
        out.extend_from_slice(&deflate);
        out.extend_from_slice(trailer);
    }
    let tl = get_varint(meta, &mut q)? as usize;
    out.extend_from_slice(meta.get(q..q + tl).context("truncated")?);
    Ok(out)
}

pub fn jpeg_forward(d: &[u8]) -> Option<Vec<u8>> {
    use lepton_jpeg::{encode_lepton_verify, EnabledFeatures, DEFAULT_THREAD_POOL};
    let r = std::panic::catch_unwind(|| {
        encode_lepton_verify(d, &EnabledFeatures::compat_lepton_vector_write(), &DEFAULT_THREAD_POOL)
    });
    match r {
        Ok(Ok((v, _))) => Some(v),
        _ => None,
    }
}

fn jpeg_inverse(t: &[u8]) -> Result<Vec<u8>> {
    use lepton_jpeg::{decode_lepton, EnabledFeatures, DEFAULT_THREAD_POOL};
    // The read defaults accept any image size, so a crafted archive could make the decoder
    // allocate gigabytes. snugly only ever encodes JPEGs within the writer's limits, so the same
    // limits are safe here and bound memory to what a genuine large photo needs.
    let w = EnabledFeatures::compat_lepton_vector_write();
    let mut feats = EnabledFeatures::compat_lepton_vector_read();
    feats.max_jpeg_width = w.max_jpeg_width;
    feats.max_jpeg_height = w.max_jpeg_height;
    feats.max_jpeg_file_size = w.max_jpeg_file_size;
    let mut out = Vec::new();
    decode_lepton(&mut Cursor::new(t), &mut out, &feats, &DEFAULT_THREAD_POOL)
    .map_err(|e| anyhow!("lepton: {e:?}"))?;
    Ok(out)
}

pub fn inverse(transform: u8, t: &[u8]) -> Result<Vec<u8>> {
    std::panic::catch_unwind(|| inverse_inner(transform, t)).map_err(|_| anyhow!("recompression decoder panicked on corrupt data"))?
}

fn inverse_inner(transform: u8, t: &[u8]) -> Result<Vec<u8>> {
    match transform {
        T_NONE => Ok(t.to_vec()),
        T_GZIP => gzip_inverse(t),
        T_JPEG => jpeg_inverse(t),
        T_SPLICE => crate::splice::inverse(t),
        x => bail!("unknown transform {x}"),
    }
}

/// Try every applicable transform; keep it only if the inverse reproduces `d` exactly.
pub fn forward(d: &[u8]) -> Option<(u8, Vec<u8>)> {
    let cand = if is_gzip(d) {
        gzip_forward(d).map(|c| (T_GZIP, c))
    } else if is_jpeg(d) {
        jpeg_forward(d).map(|c| (T_JPEG, c))
    } else if crate::splice::is_zip(d) || crate::splice::is_png(d) {
        crate::splice::forward(d).map(|c| (T_SPLICE, c))
    } else {
        None
    }?;
    match inverse(cand.0, &cand.1) {
        Ok(r) if r == d => Some(cand),
        _ => None,
    }
}
