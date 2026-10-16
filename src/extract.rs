//! Extraction, listing, snapshot listing and verification — with bounded memory.
//!
//! Files are ordered by the blocks they need and extracted in groups whose blocks fit in a
//! small cache (default 4 blocks). Each group's missing blocks are read, decrypted (if
//! needed) and decoded in parallel, then the group's files are written in parallel. A file
//! needing more blocks than the cache holds is streamed on its own.
//!
//! Safety: every path is checked (relative, no `..`); an existing symlink at a destination is
//! replaced rather than followed; archive symlinks are created only after all files are
//! written, so nothing is ever written through a symlink from the archive.

use crate::archive::{self, ROLE_BLOCK};
use crate::codec::decompress;
use crate::create::human;
use crate::crypto::Cipher;
use crate::format::*;
use crate::recomp;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

struct BlockCache<'a> {
    file: File,
    idx: &'a Index,
    cipher: Option<&'a Cipher>,
    map: HashMap<u32, Arc<Vec<u8>>>,
    lru: VecDeque<u32>,
    cap: usize,
}

/// Positional read, safe to call from many threads on one file handle.
fn read_block(file: &File, off: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    #[cfg(unix)]
    std::os::unix::fs::FileExt::read_exact_at(file, &mut buf, off)?;
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < len {
            let n = std::os::windows::fs::FileExt::seek_read(file, &mut buf[done..], off + done as u64)?;
            if n == 0 {
                bail!("unexpected end of archive");
            }
            done += n;
        }
    }
    Ok(buf)
}

impl<'a> BlockCache<'a> {
    /// Make every block in `need` resident, evicting least-recently-used blocks that aren't
    /// needed first. Each missing block is read, decrypted and decoded by one thread in one
    /// go, so at most one compressed copy per thread exists at any moment.
    fn ensure(&mut self, need: &BTreeSet<u32>) -> Result<()> {
        let missing: Vec<u32> = need.iter().copied().filter(|b| !self.map.contains_key(b)).collect();
        let room = self.cap.max(need.len());
        while self.map.len() + missing.len() > room {
            let Some(pos) = self.lru.iter().position(|b| !need.contains(b)) else { break };
            let victim = self.lru.remove(pos).unwrap();
            self.map.remove(&victim);
        }
        let (idx, cipher, file) = (self.idx, self.cipher, &self.file);
        let decoded: Vec<(u32, Vec<u8>)> = missing
            .into_par_iter()
            .map(|b| {
                let be = &idx.blocks[b as usize];
                let buf = read_block(file, be.offset, be.comp_len as usize)?;
                let plain = match cipher {
                    Some(c) => c.open(ROLE_BLOCK, b as u64, &buf).with_context(|| format!("block {b} failed authentication (tampered or damaged)"))?,
                    None => buf,
                };
                Ok((b, decompress(be.codec, plain, be.raw_len as usize).with_context(|| format!("block {b}"))?))
            })
            .collect::<Result<_>>()?;
        for (b, d) in decoded {
            self.map.insert(b, Arc::new(d));
        }
        for &b in need {
            if let Some(p) = self.lru.iter().position(|x| *x == b) {
                self.lru.remove(p);
            }
            self.lru.push_back(b);
        }
        Ok(())
    }

    fn slice(&self, c: &ChunkEntry) -> &[u8] {
        &self.map[&c.block][c.offset as usize..(c.offset + c.len) as usize]
    }
}

fn blocks_of(idx: &Index, e: &Entry) -> BTreeSet<u32> {
    e.chunks.iter().map(|&c| idx.chunks[c as usize].block).collect()
}

fn clear_dst(dst: &Path) -> Result<()> {
    if let Ok(md) = fs::symlink_metadata(dst) {
        if md.file_type().is_symlink() || md.is_file() {
            fs::remove_file(dst)?;
        } else if md.is_dir() {
            bail!("{} exists and is a directory", dst.display());
        }
    }
    Ok(())
}

pub fn apply_meta(path: &Path, m: &Meta, symlink: bool, same_owner: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if same_owner {
            let _ = std::os::unix::fs::lchown(path, Some(m.uid), Some(m.gid));
        }
        if !symlink {
            fs::set_permissions(path, fs::Permissions::from_mode(m.mode & 0o7777))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = same_owner;
        if !symlink && m.mode & 0o200 == 0 {
            let mut p = fs::metadata(path)?.permissions();
            p.set_readonly(true);
            fs::set_permissions(path, p)?;
        }
    }
    let ft = filetime::FileTime::from_unix_time(m.mtime_s, m.mtime_ns);
    if symlink {
        filetime::set_symlink_file_times(path, ft, ft)?;
    } else {
        filetime::set_file_mtime(path, ft)?;
    }
    Ok(())
}

/// Reassemble one file from chunks (via `get`), verify it, and optionally write it.
fn emit_with(e: &Entry, outdir: &Path, write: bool, same_owner: bool, mut next: impl FnMut(&ChunkEntry) -> Result<Vec<u8>>, idx: &Index) -> Result<()> {
    let dst = outdir.join(&e.path);
    let mut hasher = blake3::Hasher::new();
    let mut size = 0u64;
    if e.transform != recomp::T_NONE {
        let mut content = Vec::new();
        for &c in &e.chunks {
            content.extend_from_slice(&next(&idx.chunks[c as usize])?);
        }
        let data = recomp::inverse(e.transform, &content).with_context(|| format!("undoing transform: {}", e.path))?;
        hasher.update(&data);
        size = data.len() as u64;
        if size == e.size && *hasher.finalize().as_bytes() == e.hash && write {
            clear_dst(&dst)?;
            fs::write(&dst, &data)?;
        }
    } else {
        let mut w = if write {
            clear_dst(&dst)?;
            Some(BufWriter::with_capacity(1 << 20, File::create(&dst)?))
        } else {
            None
        };
        for &c in &e.chunks {
            let s = next(&idx.chunks[c as usize])?;
            hasher.update(&s);
            size += s.len() as u64;
            if let Some(w) = w.as_mut() {
                w.write_all(&s)?;
            }
        }
        if let Some(mut w) = w {
            w.flush()?;
        }
    }
    if size != e.size || *hasher.finalize().as_bytes() != e.hash {
        if write {
            let _ = fs::remove_file(&dst);
        }
        bail!("checksum mismatch: {}", e.path);
    }
    if write {
        apply_meta(&dst, &e.meta, false, same_owner)?;
    }
    Ok(())
}

pub struct XOpts<'a> {
    pub only: Option<&'a str>,
    pub write: bool,
    pub same_owner: bool,
    pub cache_blocks: usize,
    /// 1-based snapshot number; None = latest. Ignored when `all_snapshots` is set.
    pub snapshot: Option<usize>,
    pub all_snapshots: bool,
}

fn pick_snapshot(idx: &Index, n: Option<usize>) -> Result<usize> {
    if idx.snapshots.is_empty() {
        bail!("archive has no snapshots");
    }
    match n {
        None => Ok(idx.snapshots.len() - 1),
        Some(k) if k >= 1 && k <= idx.snapshots.len() => Ok(k - 1),
        Some(k) => bail!("no snapshot #{k} (archive has {})", idx.snapshots.len()),
    }
}

pub fn extract(archive: &Path, outdir: &Path, o: &XOpts) -> Result<()> {
    let t0 = Instant::now();
    let op = archive::open(archive)?;
    let idx = &op.index;

    // Which entries: one snapshot, or (for verification) every distinct file in all snapshots.
    let sel: Vec<&Entry> = if o.all_snapshots {
        let mut seen = HashSet::new();
        idx.snapshots
            .iter()
            .flat_map(|s| s.entries.iter())
            .filter(|e| e.kind == E_FILE && seen.insert((e.hash, e.chunks.clone())))
            .collect()
    } else {
        let s = &idx.snapshots[pick_snapshot(idx, o.snapshot)?];
        s.entries
            .iter()
            .filter(|e| match o.only {
                None => true,
                Some(p) => {
                    let p = p.trim_end_matches('/');
                    e.path == p || e.path.starts_with(&format!("{p}/"))
                }
            })
            .collect()
    };
    for e in &sel {
        if !safe_path(&e.path) {
            bail!("unsafe path in archive: {:?}", e.path);
        }
    }
    if sel.is_empty() {
        bail!("no matching entries");
    }

    if o.write {
        fs::create_dir_all(outdir)?;
        for e in sel.iter().filter(|e| e.kind == E_DIR) {
            let d = outdir.join(&e.path);
            if fs::symlink_metadata(&d).is_ok_and(|m| m.file_type().is_symlink()) {
                fs::remove_file(&d)?;
            }
            fs::create_dir_all(&d)?;
        }
        for e in sel.iter().filter(|e| e.kind != E_DIR) {
            if let Some(p) = outdir.join(&e.path).parent() {
                fs::create_dir_all(p)?;
            }
        }
    }

    let mut files: Vec<(&Entry, BTreeSet<u32>)> = sel.iter().filter(|e| e.kind == E_FILE).map(|e| (*e, blocks_of(idx, e))).collect();
    files.sort_by_key(|(_, bs)| bs.iter().next().copied().unwrap_or(0));
    let cap = o.cache_blocks.max(1);
    let mut cache = BlockCache { file: op.file.try_clone()?, idx, cipher: op.cipher.as_deref(), map: HashMap::new(), lru: VecDeque::new(), cap };
    let mut decoded_blocks = BTreeSet::new();
    let mut i = 0;
    while i < files.len() {
        if files[i].1.len() > cap {
            // Too many blocks for the cache: stream this file chunk by chunk.
            decoded_blocks.extend(files[i].1.iter().copied());
            let e = files[i].0;
            let cache_ref = &mut cache;
            emit_with(
                e,
                outdir,
                o.write,
                o.same_owner,
                |ce| {
                    cache_ref.ensure(&BTreeSet::from([ce.block]))?;
                    Ok(cache_ref.slice(ce).to_vec())
                },
                idx,
            )?;
            i += 1;
            continue;
        }
        let mut set = files[i].1.clone();
        let mut j = i + 1;
        while j < files.len() && j - i < 4096 && files[j].1.len() <= cap {
            let u: BTreeSet<u32> = set.union(&files[j].1).copied().collect();
            if u.len() > cap {
                break;
            }
            set = u;
            j += 1;
        }
        cache.ensure(&set)?;
        decoded_blocks.extend(set.iter().copied());
        let c = &cache;
        files[i..j]
            .par_iter()
            .try_for_each(|(e, _)| emit_with(e, outdir, o.write, o.same_owner, |ce| Ok(c.slice(ce).to_vec()), idx))?;
        i = j;
    }
    drop(cache);

    let mut nlinks = 0;
    if o.write {
        for e in sel.iter().filter(|e| e.kind == E_SYMLINK) {
            let dst = outdir.join(&e.path);
            clear_dst(&dst)?;
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&e.target, &dst)?;
                apply_meta(&dst, &e.meta, true, o.same_owner)?;
                nlinks += 1;
            }
            #[cfg(not(unix))]
            eprintln!("warning: skipping symlink {} -> {}", e.path, e.target);
        }
        let mut dirs: Vec<&&Entry> = sel.iter().filter(|e| e.kind == E_DIR).collect();
        dirs.sort_by_key(|e| std::cmp::Reverse(e.path.matches('/').count()));
        for e in dirs {
            apply_meta(&outdir.join(&e.path), &e.meta, false, o.same_owner)?;
        }
    }

    let what = if o.all_snapshots {
        format!("all {} snapshot(s)", idx.snapshots.len())
    } else {
        format!("snapshot #{}", pick_snapshot(idx, o.snapshot)? + 1)
    };
    eprintln!(
        "{} {} file(s){} from {what}{}, decoded {}/{} blocks, all BLAKE3 hashes OK, {:.2}s",
        if o.write { "extracted" } else { "verified" },
        files.len(),
        if nlinks > 0 { format!(" + {nlinks} symlink(s)") } else { String::new() },
        if op.cipher.is_some() { " (decrypted)" } else { "" },
        decoded_blocks.len(),
        idx.blocks.len(),
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

pub fn list(archive: &Path, snapshot: Option<usize>) -> Result<()> {
    let op = archive::open(archive)?;
    let s = &op.index.snapshots[pick_snapshot(&op.index, snapshot)?];
    for e in &s.entries {
        let t = match e.kind {
            E_DIR => 'd',
            E_SYMLINK => 'l',
            _ => '-',
        };
        let mut perm = String::new();
        for (i, ch) in "rwxrwxrwx".chars().enumerate() {
            perm.push(if e.meta.mode & (1 << (8 - i)) != 0 { ch } else { '-' });
        }
        let tr = match e.transform {
            recomp::T_GZIP => " [gz]",
            recomp::T_JPEG => " [jpg]",
            recomp::T_SPLICE => " [zip/png]",
            _ => "",
        };
        let link = if e.kind == E_SYMLINK { format!(" -> {}", e.target) } else { String::new() };
        println!("{t}{perm} {:>12} {} {}{link}{tr}", e.size, fmt_time(e.meta.mtime_s), e.path);
    }
    Ok(())
}

pub fn snapshots(archive: &Path) -> Result<()> {
    let op = archive::open(archive)?;
    let idx = &op.index;
    println!(
        "{}archive {} — {} snapshot(s), {} on disk",
        if op.cipher.is_some() { "encrypted " } else { "" },
        archive.display(),
        idx.snapshots.len(),
        human(fs::metadata(archive)?.len())
    );
    println!("{:>3}  {:<19}  {:>8}  {:>11}  {:>11}  source", "#", "created (UTC)", "files", "total size", "added");
    for (n, s) in idx.snapshots.iter().enumerate() {
        let end = idx.snapshots.get(n + 1).map(|x| x.first_block).unwrap_or(idx.blocks.len() as u32);
        let added: u64 = idx.blocks[s.first_block as usize..end as usize].iter().map(|b| b.comp_len).sum();
        let files = s.entries.iter().filter(|e| e.kind == E_FILE).count();
        let total: u64 = s.entries.iter().filter(|e| e.kind == E_FILE).map(|e| e.size).sum();
        println!("{:>3}  {:<19}  {:>8}  {:>11}  {:>11}  {}", n + 1, fmt_time(s.time_s), files, human(total), human(added), s.label);
    }
    Ok(())
}

/// "YYYY-MM-DD HH:MM:SS" in UTC, without pulling in a date library.
pub fn fmt_time(t: i64) -> String {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}
