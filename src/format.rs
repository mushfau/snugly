//! On-disk format (v1).
//!
//! ```text
//! header (33 bytes): "SNG\x01" | flags u8 | salt [16] | argon2 m_kib u32 | t u32 | p u32
//! blocks ...                              (sealed with XChaCha20-Poly1305 if encrypted)
//! index blob | footer (32 bytes)          <- snapshot 1
//! more blocks ... | index blob | footer   <- snapshot 2 (appended; never rewrites earlier bytes)
//! ```
//! footer = index_off u64 | index_len u64 | index_raw_len u64 | "SNUGEND\x01".
//! The last valid footer wins; an interrupted append leaves the previous one intact.
//! index blob = zstd(index), then sealed if the archive is encrypted.

use crate::crypto::KdfParams;
use anyhow::{bail, Context, Result};

pub const MAGIC: &[u8; 4] = b"SNG\x01";
pub const END_MAGIC: &[u8; 8] = b"SNUGEND\x01";
pub const HEADER_LEN: u64 = 33;
pub const FOOTER_LEN: u64 = 32;
pub const FLAG_ENCRYPTED: u8 = 1;

pub const CODEC_STORE: u8 = 0;
pub const CODEC_ZSTD: u8 = 1;
pub const CODEC_XZ: u8 = 2;
pub const CODEC_XZ_X86: u8 = 3;
pub const CODEC_BSC: u8 = 4;
/// LZMA2 with the ARM64 branch filter (xz 5.4+), for Apple Silicon and other arm64 code.
pub const CODEC_XZ_ARM64: u8 = 5;

pub const E_FILE: u8 = 0;
pub const E_DIR: u8 = 1;
pub const E_SYMLINK: u8 = 2;

// ------------------------------------------------------------------ header

#[derive(Clone, Debug)]
pub struct Header {
    pub kdf: Option<KdfParams>,
}

impl Header {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(HEADER_LEN as usize);
        o.extend_from_slice(MAGIC);
        match &self.kdf {
            Some(k) => {
                o.push(FLAG_ENCRYPTED);
                o.extend_from_slice(&k.salt);
                o.extend_from_slice(&k.m_kib.to_le_bytes());
                o.extend_from_slice(&k.t.to_le_bytes());
                o.extend_from_slice(&k.p.to_le_bytes());
            }
            None => o.resize(HEADER_LEN as usize, 0),
        }
        o
    }

    pub fn decode(b: &[u8]) -> Result<Header> {
        if b.len() < HEADER_LEN as usize || &b[..4] != MAGIC {
            if b.len() >= 4 && &b[..3] == b"SNG" {
                bail!("archive is snugly format v{}, this build reads v1", b[3]);
            }
            bail!("not a snugly archive");
        }
        let flags = b[4];
        if flags & !FLAG_ENCRYPTED != 0 {
            bail!("unknown archive flags {flags:#x}");
        }
        let u = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let kdf = (flags & FLAG_ENCRYPTED != 0).then(|| KdfParams {
            salt: b[5..21].try_into().unwrap(),
            m_kib: u(21),
            t: u(25),
            p: u(29),
        });
        Ok(Header { kdf })
    }
}

// ------------------------------------------------------------------ index

#[derive(Clone, Debug)]
pub struct BlockEntry {
    pub offset: u64,
    pub comp_len: u64,
    pub raw_len: u64,
    pub codec: u8,
}

#[derive(Clone, Debug)]
pub struct ChunkEntry {
    pub block: u32,
    pub offset: u64,
    pub len: u64,
    /// BLAKE3 of the chunk — lets later snapshots dedup against everything already stored.
    pub hash: [u8; 32],
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub mode: u32,
    pub mtime_s: i64,
    pub mtime_ns: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub kind: u8,
    pub meta: Meta,
    pub size: u64,
    pub hash: [u8; 32],
    pub transform: u8,
    pub chunks: Vec<u32>,
    pub target: String,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub time_s: i64,
    pub label: String,
    /// Blocks with id >= first_block were added by this snapshot.
    pub first_block: u32,
    pub entries: Vec<Entry>,
}

/// How files are cut into chunks. Recorded in the archive so a later snugly with different
/// defaults keeps cutting the same way when it adds snapshots, and dedup keeps working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunker {
    /// 1 = FastCDC (2020 paper, `fastcdc::v2020`), normalization level 1.
    pub algorithm: u8,
    pub min: u32,
    pub avg: u32,
    pub max: u32,
    /// Files up to this size are stored as a single chunk.
    pub whole_file: u32,
}

impl Default for Chunker {
    fn default() -> Self {
        Chunker { algorithm: 1, min: 16 * 1024, avg: 64 * 1024, max: 256 * 1024, whole_file: 32 * 1024 }
    }
}

impl Chunker {
    fn valid(&self) -> bool {
        // The bounds fastcdc accepts (it panics outside them), plus sane ordering.
        self.algorithm == 1
            && (64..=1 << 20).contains(&self.min)
            && (256..=4 << 20).contains(&self.avg)
            && (1024..=16 << 20).contains(&self.max)
            && self.min <= self.avg
            && self.avg <= self.max
            && self.whole_file <= 16 << 20
    }
}

#[derive(Default)]
pub struct Index {
    pub blocks: Vec<BlockEntry>,
    pub chunks: Vec<ChunkEntry>,
    pub snapshots: Vec<Snapshot>,
    /// Absent from archives written before it was recorded; those used the defaults.
    pub chunker: Chunker,
}

/// Optional tagged sections after the snapshots: tag, length, payload. Readers skip tags they
/// don't know, so new fields can be added without breaking older archives or readers.
const SECTION_CHUNKER: u64 = 1;

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn get_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = *buf.get(*pos).context("truncated varint")?;
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            bail!("bad varint");
        }
    }
}

fn put_str(o: &mut Vec<u8>, s: &str) {
    put_varint(o, s.len() as u64);
    o.extend_from_slice(s.as_bytes());
}

fn get_str(buf: &[u8], p: &mut usize) -> Result<String> {
    let l = get_varint(buf, p)? as usize;
    let s = String::from_utf8(buf.get(*p..p.checked_add(l).context("overflow")?).context("truncated")?.to_vec())?;
    *p += l;
    Ok(s)
}

fn get_u8(buf: &[u8], p: &mut usize) -> Result<u8> {
    let b = *buf.get(*p).context("truncated")?;
    *p += 1;
    Ok(b)
}

fn get_32(buf: &[u8], p: &mut usize) -> Result<[u8; 32]> {
    let h: [u8; 32] = buf.get(*p..*p + 32).context("truncated")?.try_into()?;
    *p += 32;
    Ok(h)
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}
fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn encode_entry(o: &mut Vec<u8>, e: &Entry) {
    put_str(o, &e.path);
    o.push(e.kind);
    put_varint(o, e.meta.mode as u64);
    put_varint(o, zigzag(e.meta.mtime_s));
    put_varint(o, e.meta.mtime_ns as u64);
    put_varint(o, e.meta.uid as u64);
    put_varint(o, e.meta.gid as u64);
    match e.kind {
        E_FILE => {
            put_varint(o, e.size);
            o.extend_from_slice(&e.hash);
            o.push(e.transform);
            put_varint(o, e.chunks.len() as u64);
            for &c in &e.chunks {
                put_varint(o, c as u64);
            }
        }
        E_SYMLINK => put_str(o, &e.target),
        _ => {}
    }
}

fn decode_entry(buf: &[u8], p: &mut usize, chunks: &[ChunkEntry]) -> Result<Entry> {
    let path = get_str(buf, p)?;
    let kind = get_u8(buf, p)?;
    let meta = Meta {
        mode: get_varint(buf, p)? as u32,
        mtime_s: unzigzag(get_varint(buf, p)?),
        mtime_ns: (get_varint(buf, p)? as u32).min(999_999_999),
        uid: get_varint(buf, p)? as u32,
        gid: get_varint(buf, p)? as u32,
    };
    let mut e = Entry { path, kind, meta, size: 0, hash: [0; 32], transform: 0, chunks: Vec::new(), target: String::new() };
    match kind {
        E_FILE => {
            e.size = get_varint(buf, p)?;
            e.hash = get_32(buf, p)?;
            e.transform = get_u8(buf, p)?;
            let n = get_varint(buf, p)? as usize;
            let mut total = 0u64;
            e.chunks.reserve(n.min(1 << 24));
            for _ in 0..n {
                let c = get_varint(buf, p)?;
                let ce = chunks.get(c as usize).context("file references missing chunk")?;
                total = total.saturating_add(ce.len);
                e.chunks.push(c as u32);
            }
            if e.transform == 0 && total != e.size {
                bail!("size mismatch for {}", e.path);
            }
        }
        E_SYMLINK => e.target = get_str(buf, p)?,
        E_DIR => {}
        k => bail!("unknown entry kind {k}"),
    }
    Ok(e)
}

impl Index {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        put_varint(&mut o, self.blocks.len() as u64);
        for b in &self.blocks {
            put_varint(&mut o, b.offset);
            put_varint(&mut o, b.comp_len);
            put_varint(&mut o, b.raw_len);
            o.push(b.codec);
        }
        put_varint(&mut o, self.chunks.len() as u64);
        for c in &self.chunks {
            put_varint(&mut o, c.block as u64);
            put_varint(&mut o, c.offset);
            put_varint(&mut o, c.len);
            o.extend_from_slice(&c.hash);
        }
        put_varint(&mut o, self.snapshots.len() as u64);
        for s in &self.snapshots {
            put_varint(&mut o, zigzag(s.time_s));
            put_str(&mut o, &s.label);
            put_varint(&mut o, s.first_block as u64);
            put_varint(&mut o, s.entries.len() as u64);
            for e in &s.entries {
                encode_entry(&mut o, e);
            }
        }
        let mut c = Vec::new();
        c.push(self.chunker.algorithm);
        for v in [self.chunker.min, self.chunker.avg, self.chunker.max, self.chunker.whole_file] {
            put_varint(&mut c, v as u64);
        }
        put_varint(&mut o, SECTION_CHUNKER);
        put_varint(&mut o, c.len() as u64);
        o.extend_from_slice(&c);
        o
    }

    /// Decodes and validates every cross-reference, so later code can index without checks.
    pub fn decode(buf: &[u8]) -> Result<Index> {
        let mut p = 0;
        let nb = get_varint(buf, &mut p)? as usize;
        let mut blocks = Vec::with_capacity(nb.min(1 << 20));
        for _ in 0..nb {
            let offset = get_varint(buf, &mut p)?;
            let comp_len = get_varint(buf, &mut p)?;
            let raw_len = get_varint(buf, &mut p)?;
            let codec = get_u8(buf, &mut p)?;
            if codec > CODEC_XZ_ARM64 {
                bail!("unknown codec {codec}");
            }
            if raw_len > 1 << 31 || comp_len > 1 << 32 {
                bail!("block too large");
            }
            blocks.push(BlockEntry { offset, comp_len, raw_len, codec });
        }
        let nc = get_varint(buf, &mut p)? as usize;
        let mut chunks = Vec::with_capacity(nc.min(1 << 24));
        for _ in 0..nc {
            let block = get_varint(buf, &mut p)?;
            let offset = get_varint(buf, &mut p)?;
            let len = get_varint(buf, &mut p)?;
            let hash = get_32(buf, &mut p)?;
            let be = blocks.get(block as usize).context("chunk references missing block")?;
            if offset.checked_add(len).is_none_or(|e| e > be.raw_len) {
                bail!("chunk out of block bounds");
            }
            chunks.push(ChunkEntry { block: block as u32, offset, len, hash });
        }
        let ns = get_varint(buf, &mut p)? as usize;
        let mut snapshots = Vec::with_capacity(ns.min(1 << 16));
        for _ in 0..ns {
            let time_s = unzigzag(get_varint(buf, &mut p)?);
            let label = get_str(buf, &mut p)?;
            let first_block = get_varint(buf, &mut p)? as u32;
            let ne = get_varint(buf, &mut p)? as usize;
            let mut entries = Vec::with_capacity(ne.min(1 << 24));
            for _ in 0..ne {
                entries.push(decode_entry(buf, &mut p, &chunks)?);
            }
            snapshots.push(Snapshot { time_s, label, first_block, entries });
        }
        let mut chunker = Chunker::default();
        while p < buf.len() {
            let tag = get_varint(buf, &mut p)?;
            let len = get_varint(buf, &mut p)? as usize;
            let body = buf.get(p..p.checked_add(len).context("overflow")?).context("truncated section")?;
            p += len;
            if tag == SECTION_CHUNKER {
                let mut q = 0;
                let algorithm = get_u8(body, &mut q)?;
                let mut v = [0u32; 4];
                for x in &mut v {
                    *x = u32::try_from(get_varint(body, &mut q)?).context("bad chunker settings")?;
                }
                chunker = Chunker { algorithm, min: v[0], avg: v[1], max: v[2], whole_file: v[3] };
                if !chunker.valid() {
                    bail!("unsupported chunker settings {chunker:?}");
                }
            }
        }
        Ok(Index { blocks, chunks, snapshots, chunker })
    }
}

/// Paths inside an archive must be relative, normalised and free of `..`.
pub fn safe_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.contains('\0')
        && !(p.len() >= 2 && p.as_bytes()[1] == b':')
        && !p.split('/').any(|c| c == ".." || c == "." || c.is_empty())
}
