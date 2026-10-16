//! Recovery records: Reed–Solomon parity in a sidecar file (`<archive>.rec`), par2-style.
//!
//! The archive is cut into 128 MiB segments of 64 KiB shards. For each segment we store
//! `ceil(pct% × shards)` Reed–Solomon recovery shards (GF(2^16), reed-solomon-simd) plus a
//! 128-bit BLAKE3 hash of every original and recovery shard. Repair hashes each shard to find
//! the damaged ones, then rebuilds them from any surviving shards — so per segment, up to
//! `recovery shards` damaged 64 KiB pieces (e.g. 103 × 64 KiB ≈ 6.4 MiB at 5%) can be restored,
//! wherever they are: scattered bad sectors, one contiguous burst, a destroyed index, or a
//! truncated tail.
//!
//! Why a sidecar rather than embedded: archives are append-only, so the parity of every full
//! segment written earlier stays valid forever and an update only recomputes the last segment
//! plus the new ones; repair needs nothing from the archive itself (not even its index); and
//! the sidecar can live on a different disk.
//!
//! Sidecar layout:
//! ```text
//! header (64 B): "SNUGREC\x01" | shard_size u32 | seg_shards u32 | pct×100 u32 |
//!                 idx1_off u64 | idx2_off u64 | idx_len u64 | blake3(idx)[16] | pad   (filled in last)
//! index copy 1 (in space reserved up front)
//! recovery shards, segment by segment
//! index copy 2 | footer (48 B): idx1_off u64 | idx2_off u64 | idx_len u64 | blake3(idx)[16] | "SNUGRECE"
//! ```

use crate::create::human;
use crate::format::{get_varint, put_varint};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const MAGIC: &[u8; 8] = b"SNUGREC\x01";
const END: &[u8; 8] = b"SNUGRECE";
const HEADER: u64 = 64;
const FOOTER: u64 = 48;
pub const SHARD: usize = 64 << 10;
pub const SEG_SHARDS: usize = 2048;
const SEG_BYTES: u64 = (SHARD * SEG_SHARDS) as u64;

type H = [u8; 16];

fn h(b: &[u8]) -> H {
    blake3::hash(b).as_bytes()[..16].try_into().unwrap()
}

pub fn sidecar_path(archive: &Path) -> PathBuf {
    let mut s = archive.as_os_str().to_owned();
    s.push(".rec");
    PathBuf::from(s)
}

#[derive(Clone)]
struct Seg {
    rec_off: u64, // where this segment's recovery shards start in the sidecar
    data_len: u64,
    n_rec: usize,
    orig: Vec<H>,
    rec: Vec<H>,
}

struct Sidecar {
    pct100: u32,
    archive_len: u64,
    segs: Vec<Seg>,
}

fn n_rec_for(n_orig: usize, pct100: u32) -> usize {
    ((n_orig as u64 * pct100 as u64).div_ceil(10_000) as usize).max(1)
}

fn encode_index(sc: &Sidecar) -> Vec<u8> {
    let mut o = Vec::new();
    put_varint(&mut o, sc.pct100 as u64);
    put_varint(&mut o, sc.archive_len);
    put_varint(&mut o, sc.segs.len() as u64);
    for s in &sc.segs {
        put_varint(&mut o, s.rec_off);
        put_varint(&mut o, s.data_len);
        put_varint(&mut o, s.n_rec as u64);
        for x in s.orig.iter().chain(&s.rec) {
            o.extend_from_slice(x);
        }
    }
    o
}

/// Exposed for fuzzing: parse a recovery index and discard it.
#[doc(hidden)]
pub fn fuzz_decode_index(b: &[u8]) {
    let _ = decode_index(b);
}

fn decode_index(b: &[u8]) -> Result<Sidecar> {
    let mut p = 0;
    let pct100 = get_varint(b, &mut p)? as u32;
    let archive_len = get_varint(b, &mut p)?;
    let n = get_varint(b, &mut p)? as usize;
    if n as u64 != archive_len.div_ceil(SEG_BYTES) {
        bail!("recovery index doesn't match archive length");
    }
    // Each segment carries at least two 16-byte hashes, so a count larger than the index
    // could possibly hold is a lie — reject it before reserving memory for it.
    if n > b.len() / 32 + 1 {
        bail!("corrupt recovery index (segment count)");
    }
    let mut segs = Vec::with_capacity(n);
    for k in 0..n {
        let rec_off = get_varint(b, &mut p)?;
        let data_len = get_varint(b, &mut p)?;
        let n_rec = get_varint(b, &mut p)? as usize;
        let expect = (archive_len - k as u64 * SEG_BYTES).min(SEG_BYTES);
        if data_len != expect || n_rec == 0 || n_rec > 32768 {
            bail!("corrupt recovery index");
        }
        let n_orig = (data_len as usize).div_ceil(SHARD);
        if (n_orig + n_rec) * 16 > b.len() - p.min(b.len()) {
            bail!("truncated recovery index");
        }
        let mut take = |c: usize| -> Result<Vec<H>> {
            (0..c)
                .map(|_| {
                    let x: H = b.get(p..p + 16).context("truncated recovery index")?.try_into()?;
                    p += 16;
                    Ok(x)
                })
                .collect()
        };
        let orig = take(n_orig)?;
        let rec = take(n_rec)?;
        segs.push(Seg { rec_off, data_len, n_rec, orig, rec });
    }
    Ok(Sidecar { pct100, archive_len, segs })
}

fn read_at(f: &mut File, off: u64, len: usize) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(off))?;
    let mut b = vec![0u8; len];
    f.read_exact(&mut b)?;
    Ok(b)
}

/// Read up to `len` bytes; a truncated archive simply yields fewer bytes.
fn read_upto(f: &mut File, off: u64, len: usize) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(off))?;
    let mut b = Vec::with_capacity(len);
    f.take(len as u64).read_to_end(&mut b)?;
    Ok(b)
}

fn load_sidecar(path: &Path) -> Result<(File, Sidecar)> {
    let mut f = File::open(path).with_context(|| format!("no recovery file {}", path.display()))?;
    let len = f.metadata()?.len();
    let hdr = read_at(&mut f, 0, HEADER as usize).context("recovery file too short")?;
    let u = |i: usize| u32::from_le_bytes(hdr[i..i + 4].try_into().unwrap());
    if &hdr[..8] != MAGIC {
        eprintln!("warning: recovery file header is damaged; using its other copies");
    } else if u(8) as usize != SHARD || u(12) as usize != SEG_SHARDS {
        bail!("unsupported recovery parameters");
    }
    let hdr_ptr = (
        u64::from_le_bytes(hdr[20..28].try_into()?),
        u64::from_le_bytes(hdr[28..36].try_into()?),
        u64::from_le_bytes(hdr[36..44].try_into()?),
        <H>::try_from(&hdr[44..60])?,
    );
    // Footer at the end, or — if the end is damaged — the last intact footer found by scanning.
    let mut candidates = vec![len];
    if len > FOOTER {
        let tail = read_upto(&mut f, len.saturating_sub(1 << 20), 1 << 20)?;
        let base = len.saturating_sub(1 << 20);
        for i in (0..tail.len().saturating_sub(7)).rev() {
            if &tail[i..i + 8] == END {
                candidates.push(base + i as u64 + 8);
            }
        }
    }
    // Pointers to try: the footer at the end, any earlier intact footer, and the copy in the header.
    let mut ptrs: Vec<(u64, u64, u64, H)> = Vec::new();
    for end in candidates {
        if end < HEADER + FOOTER {
            continue;
        }
        let Ok(foot) = read_at(&mut f, end - FOOTER, FOOTER as usize) else { continue };
        if &foot[40..48] != END {
            continue;
        }
        ptrs.push((
            u64::from_le_bytes(foot[0..8].try_into()?),
            u64::from_le_bytes(foot[8..16].try_into()?),
            u64::from_le_bytes(foot[16..24].try_into()?),
            foot[24..40].try_into()?,
        ));
    }
    ptrs.push(hdr_ptr);
    for (o1, o2, il, ih) in ptrs {
        for off in [o1, o2] {
            if off >= HEADER && off.checked_add(il).is_some_and(|e| e <= len) && il > 0 && il < 1 << 32 {
                if let Ok(idx) = read_at(&mut f, off, il as usize) {
                    if h(&idx) == ih {
                        if let Ok(sc) = decode_index(&idx) {
                            return Ok((f, sc));
                        }
                    }
                }
            }
        }
    }
    bail!("recovery file {} is damaged beyond use", path.display())
}

#[allow(dead_code)]
pub struct ProtectStats {
    pub pct100: u32,
    pub segments: usize,
    pub reused: usize,
    pub rec_bytes: u64,
}

/// Create or refresh `<archive>.rec`. Segments that were already full and protected are
/// reused as-is (the archive is append-only), so refreshing after an update is cheap.
pub fn protect(archive: &Path, pct: Option<f64>, threads: usize) -> Result<ProtectStats> {
    let t0 = Instant::now();
    let rec_path = sidecar_path(archive);
    let old = if rec_path.exists() { load_sidecar(&rec_path).ok() } else { None };
    let pct100 = match (pct, &old) {
        (Some(p), _) => (p * 100.0).round() as u32,
        (None, Some((_, o))) => o.pct100,
        (None, None) => 500,
    };
    if !(10..=10_000).contains(&pct100) {
        bail!("recovery percentage must be between 0.1 and 100");
    }
    let mut af = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let len = af.metadata()?.len();
    let nseg = len.div_ceil(SEG_BYTES) as usize;

    let tmp = rec_path.with_extension("rec.tmp");
    let mut out = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
    let mut hdr = Vec::with_capacity(HEADER as usize);
    hdr.extend_from_slice(MAGIC);
    hdr.extend_from_slice(&(SHARD as u32).to_le_bytes());
    hdr.extend_from_slice(&(SEG_SHARDS as u32).to_le_bytes());
    hdr.extend_from_slice(&pct100.to_le_bytes());
    hdr.resize(HEADER as usize, 0);
    out.write_all(&hdr)?;
    // Reserve room for the first index copy right after the header, so the two copies sit at
    // opposite ends of the file and one burst of damage can't take out both.
    let total_orig: u64 = (0..nseg as u64).map(|k| (len - k * SEG_BYTES).min(SEG_BYTES).div_ceil(SHARD as u64)).sum();
    let total_rec: u64 = (0..nseg as u64)
        .map(|k| n_rec_for((len - k * SEG_BYTES).min(SEG_BYTES).div_ceil(SHARD as u64) as usize, pct100) as u64)
        .sum();
    let reserve = 64 + nseg as u64 * 30 + 16 * (total_orig + total_rec);
    out.write_all(&vec![0u8; reserve as usize])?;
    let mut off = HEADER + reserve;

    let mut old = old.filter(|(_, o)| o.pct100 == pct100);
    let mut segs: Vec<Seg> = Vec::with_capacity(nseg);
    let mut reused = 0;
    let mut k = 0;
    while k < nseg {
        // Reuse a full segment that the previous sidecar already protects.
        if let Some((of, o)) = old.as_mut() {
            if let Some(os) = o.segs.get(k).filter(|s| s.data_len == SEG_BYTES && (k as u64 + 1) * SEG_BYTES <= len) {
                // Cheap identity check: first and last piece must still match, otherwise this
                // sidecar belongs to another archive (or the data was damaged) — don't trust it.
                let start = k as u64 * SEG_BYTES;
                let first = read_at(&mut af, start, SHARD)?;
                let last = read_at(&mut af, start + SEG_BYTES - SHARD as u64, SHARD)?;
                if h(&first) != os.orig[0] || h(&last) != os.orig[SEG_SHARDS - 1] {
                    bail!(
                        "{} doesn't match this archive's data at segment {} — the archive is damaged (run `snugly repair`) \
                         or the .rec file belongs to another archive (delete it to start fresh)",
                        rec_path.display(),
                        k + 1
                    );
                }
                let bytes = read_at(of, os.rec_off, os.n_rec * SHARD)?;
                out.write_all(&bytes)?;
                segs.push(Seg { rec_off: off, ..os.clone() });
                off += bytes.len() as u64;
                reused += 1;
                k += 1;
                continue;
            }
        }
        // Compute a batch of segments in parallel (memory ≈ threads × 128 MiB).
        let batch: Vec<usize> = (k..nseg.min(k + threads.max(1))).collect();
        let mut raws = Vec::with_capacity(batch.len());
        for &s in &batch {
            let start = s as u64 * SEG_BYTES;
            raws.push(read_at(&mut af, start, (len - start).min(SEG_BYTES) as usize)?);
        }
        // A segment that was partially protected before must still match in its complete
        // pieces; otherwise refreshing would bake damage into the new parity.
        if let Some((_, o)) = old.as_ref() {
            for (&s, raw) in batch.iter().zip(&raws) {
                if let Some(os) = o.segs.get(s) {
                    let full = (os.data_len as usize) / SHARD; // pieces that were complete then
                    for i in 0..full.min(raw.len() / SHARD) {
                        if h(&raw[i * SHARD..(i + 1) * SHARD]) != os.orig[i] {
                            bail!(
                                "previously protected data in segment {} no longer matches — the archive is damaged \
                                 (run `snugly repair` first) or the .rec file belongs to another archive",
                                s + 1
                            );
                        }
                    }
                }
            }
        }
        let computed: Vec<(Vec<H>, Vec<Vec<u8>>, Vec<H>)> = raws
            .par_iter()
            .map(|raw| {
                let n_orig = raw.len().div_ceil(SHARD);
                let n_rec = n_rec_for(n_orig, pct100);
                let shards: Vec<Vec<u8>> = (0..n_orig)
                    .map(|i| {
                        let mut s = raw[i * SHARD..((i + 1) * SHARD).min(raw.len())].to_vec();
                        s.resize(SHARD, 0);
                        s
                    })
                    .collect();
                let orig: Vec<H> = shards.iter().map(|s| h(s)).collect();
                let rec = reed_solomon_simd::encode(n_orig, n_rec, &shards).map_err(|e| anyhow::anyhow!("reed-solomon: {e}"))?;
                let rh: Vec<H> = rec.iter().map(|s| h(s)).collect();
                Ok((orig, rec, rh))
            })
            .collect::<Result<_>>()?;
        for ((orig, rec, rh), raw) in computed.into_iter().zip(&raws) {
            for r in &rec {
                out.write_all(r)?;
            }
            segs.push(Seg { rec_off: off, data_len: raw.len() as u64, n_rec: rec.len(), orig, rec: rh });
            off += (rec.len() * SHARD) as u64;
        }
        k += batch.len();
    }
    drop(old);

    let sc = Sidecar { pct100, archive_len: len, segs };
    let idx = encode_index(&sc);
    if idx.len() as u64 > reserve {
        bail!("internal error: recovery index larger than reserved");
    }
    let o1 = HEADER;
    let o2 = off;
    out.write_all(&idx)?;
    let mut foot = Vec::with_capacity(FOOTER as usize);
    foot.extend_from_slice(&o1.to_le_bytes());
    foot.extend_from_slice(&o2.to_le_bytes());
    foot.extend_from_slice(&(idx.len() as u64).to_le_bytes());
    foot.extend_from_slice(&h(&idx));
    foot.extend_from_slice(END);
    out.write_all(&foot)?;
    out.flush()?;
    let mut file = out.into_inner().map_err(|e| e.into_error())?;
    file.seek(SeekFrom::Start(20))?;
    file.write_all(&foot[..40])?; // same pointer, at the start of the file
    file.seek(SeekFrom::Start(HEADER))?;
    file.write_all(&idx)?; // first index copy, in the reserved space
    file.sync_all()?;
    fs::rename(&tmp, &rec_path)?;

    let rec_bytes = fs::metadata(&rec_path)?.len();
    eprintln!(
        "recovery: {} protected at {:.1}% -> {} ({} segment(s), {} reused, {} recomputed) | {:.2}s",
        human(len),
        pct100 as f64 / 100.0,
        human(rec_bytes),
        nseg,
        reused,
        nseg - reused,
        t0.elapsed().as_secs_f64()
    );
    Ok(ProtectStats { pct100, segments: nseg, reused, rec_bytes })
}

/// Find damaged 64 KiB pieces and rebuild them. With `check_only`, nothing is written.
pub fn repair(archive: &Path, check_only: bool) -> Result<()> {
    let t0 = Instant::now();
    let rec_path = sidecar_path(archive);
    let (mut rf, sc) = load_sidecar(&rec_path)?;
    let mut af = OpenOptions::new().read(true).write(!check_only).open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let cur_len = af.metadata()?.len();
    if cur_len < sc.archive_len {
        eprintln!("archive is {} shorter than when it was protected (truncated)", human(sc.archive_len - cur_len));
    } else if cur_len > sc.archive_len {
        eprintln!("note: the last {} of the archive were added after protection and aren't covered — run `snugly protect` to cover them", human(cur_len - sc.archive_len));
    }

    let (mut damaged, mut fixed, mut bad_rec) = (0usize, 0usize, 0usize);
    let mut unrecoverable = Vec::new();
    let mut writes: Vec<(u64, Vec<u8>)> = Vec::new();
    for (k, s) in sc.segs.iter().enumerate() {
        let start = k as u64 * SEG_BYTES;
        let raw = read_upto(&mut af, start, s.data_len as usize)?;
        let n_orig = s.orig.len();
        let mut good = Vec::with_capacity(n_orig);
        let mut bad = Vec::new();
        for i in 0..n_orig {
            let lo = i * SHARD;
            let want = (s.data_len as usize - lo).min(SHARD);
            if raw.len() >= lo + want {
                let mut sh = raw[lo..lo + want].to_vec();
                sh.resize(SHARD, 0);
                if h(&sh) == s.orig[i] {
                    good.push((i, sh));
                    continue;
                }
            }
            bad.push(i);
        }
        if bad.is_empty() {
            continue;
        }
        damaged += bad.len();
        let mut rec_good = Vec::with_capacity(s.n_rec);
        for j in 0..s.n_rec {
            if let Ok(r) = read_at(&mut rf, s.rec_off + (j * SHARD) as u64, SHARD) {
                if h(&r) == s.rec[j] {
                    rec_good.push((j, r));
                    continue;
                }
            }
            bad_rec += 1;
        }
        if bad.len() > rec_good.len() {
            unrecoverable.push(format!(
                "segment {} (bytes {}..{}): {} damaged pieces but only {} usable recovery pieces",
                k + 1,
                start,
                start + s.data_len,
                bad.len(),
                rec_good.len()
            ));
            continue;
        }
        let restored = reed_solomon_simd::decode(n_orig, s.n_rec, good, rec_good).map_err(|e| anyhow::anyhow!("reed-solomon: {e}"))?;
        for i in bad {
            let sh = restored.get(&i).context("decoder did not restore a shard")?;
            if h(sh) != s.orig[i] {
                bail!("internal error: restored piece failed its checksum");
            }
            let want = (s.data_len as usize - i * SHARD).min(SHARD);
            writes.push((start + (i * SHARD) as u64, sh[..want].to_vec()));
            fixed += 1;
        }
    }

    if damaged == 0 {
        eprintln!("no damage found in {} ({} protected) — {:.2}s", archive.display(), human(sc.archive_len), t0.elapsed().as_secs_f64());
        return Ok(());
    }
    eprintln!(
        "found {damaged} damaged 64 KiB piece(s){}; {} repairable",
        if bad_rec > 0 { format!(" (+{bad_rec} damaged recovery pieces)") } else { String::new() },
        fixed
    );
    for u in &unrecoverable {
        eprintln!("  cannot repair {u}");
    }
    if check_only {
        eprintln!("check only — nothing written. Run `snugly repair` without --check to fix.");
    } else if !writes.is_empty() {
        if cur_len < sc.archive_len {
            af.set_len(sc.archive_len)?;
        }
        for (off, data) in &writes {
            af.seek(SeekFrom::Start(*off))?;
            af.write_all(data)?;
        }
        af.sync_all()?;
        eprintln!("repaired {fixed} piece(s) in {:.2}s", t0.elapsed().as_secs_f64());
    }
    if !unrecoverable.is_empty() {
        bail!("{} segment(s) have more damage than the recovery data can fix", unrecoverable.len());
    }
    Ok(())
}
