//! Archive creation and incremental updates, with bounded memory.
//!
//! Memory does not grow with the size of the input:
//! * small and medium files are read in batches (≤32 MiB) and recompressed in parallel;
//! * files over 64 MiB are streamed through content-defined chunking without being loaded;
//! * a block is handed to a worker pool as soon as it fills, through a bounded queue;
//! * a writer thread appends finished blocks in whatever order they finish.
//!
//! Updates (`--update`) append a new snapshot: files whose size and mtime match the previous
//! snapshot are reused without being read, and every chunk is checked against all chunks
//! already in the archive, so only genuinely new data is written. Nothing before the old
//! footer is touched, so an interrupted update leaves the previous snapshot intact.

use crate::archive::{self, ROLE_BLOCK, ROLE_INDEX};
use crate::codec::{classify, compress_block, Kind, Mode};
use crate::crypto::{read_password, Cipher, KdfParams};
use crate::format::*;
use crate::recomp;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const STREAM_THRESHOLD: u64 = 64 << 20;
const RECOMP_LIMIT: u64 = 256 << 20;
const BATCH_BYTES: u64 = 32 << 20;
const BATCH_FILES: usize = 512;
const CDC: (usize, usize, usize) = (16 * 1024, 64 * 1024, 256 * 1024);
const SMALL_FILE: usize = 32 * 1024;

pub struct Opts {
    pub mode: Mode,
    pub block_mib: usize,
    pub recomp: bool,
    pub threads: usize,
    pub encrypt: bool,
    pub update: bool,
    pub rehash: bool,
    pub label: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Magic {
    None,
    Gzip,
    Jpeg,
    Zip,
    Png,
}

struct Item {
    abs: PathBuf,
    rel: String,
    size: u64,
    meta: Meta,
    kind: Kind,
    magic: Magic,
}

impl Item {
    fn streams(&self) -> bool {
        self.size > STREAM_THRESHOLD && !(self.magic != Magic::None && self.size <= RECOMP_LIMIT)
    }
}

pub fn meta_of(md: &fs::Metadata) -> Meta {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Meta { mode: md.mode(), mtime_s: md.mtime(), mtime_ns: md.mtime_nsec() as u32, uid: md.uid(), gid: md.gid() }
    }
    #[cfg(not(unix))]
    {
        let ft = filetime::FileTime::from_last_modification_time(md);
        let mode = if md.is_dir() { 0o755 } else if md.permissions().readonly() { 0o444 } else { 0o644 };
        Meta { mode, mtime_s: ft.unix_seconds(), mtime_ns: ft.nanoseconds(), uid: 0, gid: 0 }
    }
}

fn ext_of(name: &str) -> String {
    Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// Look at the first 8 KiB to decide the block kind and whether a recompressor applies.
fn sniff(abs: &Path, recomp_on: bool) -> Result<(Kind, Magic)> {
    let mut head = Vec::with_capacity(8192);
    File::open(abs)?.take(8192).read_to_end(&mut head)?;
    let name = abs.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if recomp_on && recomp::is_gzip(&head) {
        let mut inner = Vec::with_capacity(8192);
        let _ = flate2::read::GzDecoder::new(&head[..]).take(8192).read_to_end(&mut inner);
        let inner_name = name.trim_end_matches(".gz").trim_end_matches(".tgz");
        return Ok((classify(&ext_of(inner_name), &inner), Magic::Gzip));
    }
    if recomp_on && recomp::is_jpeg(&head) {
        return Ok((Kind::Incompressible, Magic::Jpeg));
    }
    if recomp_on && crate::splice::is_png(&head) {
        return Ok((Kind::Binary, Magic::Png));
    }
    if recomp_on && crate::splice::is_zip(&head) {
        // Office/OpenDocument/EPUB are mostly XML once inflated; JARs, wheels, APKs are mixed.
        let text = matches!(ext_of(&name).as_str(), "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp" | "epub" | "xps" | "kmz");
        return Ok((if text { Kind::Text } else { Kind::Binary }, Magic::Zip));
    }
    Ok((classify(&ext_of(&name), &head), Magic::None))
}

// ------------------------------------------------------------------ block builder

type Job = (u32, Kind, Vec<u8>);

struct Builder {
    block_size: usize,
    seen: HashMap<[u8; 32], u32>,
    chunks: Vec<ChunkEntry>,
    open: HashMap<Kind, (u32, Vec<u8>)>,
    next_block: u32,
    tx: mpsc::SyncSender<Job>,
    dup_bytes: u64,
}

impl Builder {
    fn add_chunk(&mut self, kind: Kind, piece: &[u8]) -> Result<u32> {
        let h = *blake3::hash(piece).as_bytes();
        if let Some(&id) = self.seen.get(&h) {
            self.dup_bytes += piece.len() as u64;
            return Ok(id);
        }
        let full = match self.open.get(&kind) {
            Some((_, d)) => d.len() + piece.len() > self.block_size,
            None => true,
        };
        if full {
            if let Some((id, d)) = self.open.remove(&kind) {
                self.tx.send((id, kind, d)).context("compression workers stopped")?;
            }
            self.open.insert(kind, (self.next_block, Vec::with_capacity(self.block_size)));
            self.next_block += 1;
        }
        let (bid, d) = self.open.get_mut(&kind).unwrap();
        let id = self.chunks.len() as u32;
        self.chunks.push(ChunkEntry { block: *bid, offset: d.len() as u64, len: piece.len() as u64, hash: h });
        d.extend_from_slice(piece);
        self.seen.insert(h, id);
        Ok(id)
    }

    fn add_content(&mut self, kind: Kind, data: &[u8]) -> Result<Vec<u32>> {
        if data.is_empty() {
            return Ok(vec![]);
        }
        if data.len() <= SMALL_FILE {
            return Ok(vec![self.add_chunk(kind, data)?]);
        }
        fastcdc::v2020::FastCDC::new(data, CDC.0, CDC.1, CDC.2)
            .map(|c| self.add_chunk(kind, &data[c.offset..c.offset + c.length]))
            .collect()
    }

    fn finish(mut self) -> Result<(Vec<ChunkEntry>, u64)> {
        let mut open: Vec<_> = self.open.drain().collect();
        open.sort_by_key(|(_, (id, _))| *id);
        for (kind, (id, d)) in open {
            self.tx.send((id, kind, d)).context("compression workers stopped")?;
        }
        Ok((self.chunks, self.dup_bytes))
    }
}

// ------------------------------------------------------------------ per-file preparation

struct Prepared {
    kind: Kind,
    size: u64,
    hash: [u8; 32],
    transform: u8,
    content: Vec<u8>,
}

fn prepare(it: &Item) -> Result<Prepared> {
    let data = fs::read(&it.abs).with_context(|| format!("reading {}", it.abs.display()))?;
    let size = data.len() as u64;
    let hash = *blake3::hash(&data).as_bytes();
    if it.magic != Magic::None {
        if let Some((t, c)) = recomp::forward(&data) {
            return Ok(Prepared { kind: it.kind, size, hash, transform: t, content: c });
        }
        return Ok(Prepared { kind: Kind::Incompressible, size, hash, transform: 0, content: data });
    }
    Ok(Prepared { kind: it.kind, size, hash, transform: 0, content: data })
}

fn file_entry(rel: &str, meta: Meta, size: u64, hash: [u8; 32], transform: u8, chunks: Vec<u32>) -> Entry {
    Entry { path: rel.to_string(), kind: E_FILE, meta, size, hash, transform, chunks, target: String::new() }
}

// ------------------------------------------------------------------ create / update

pub fn create(archive: &Path, input: &Path, o: &Opts) -> Result<()> {
    let t0 = Instant::now();
    let block_size = o.block_mib.clamp(1, 1024) << 20;
    let in_md = fs::metadata(input).with_context(|| format!("reading {}", input.display()))?;
    let base: PathBuf = if in_md.is_dir() { input.to_path_buf() } else { input.parent().unwrap_or(Path::new(".")).to_path_buf() };

    // 0. New archive, or append a snapshot to an existing one.
    let exists = archive.exists();
    if exists && !o.update {
        bail!("{} already exists — use --update to add a new snapshot to it", archive.display());
    }
    let (mut out, start, cipher, mut index) = if exists {
        let op = archive::open(archive)?;
        if o.encrypt && op.cipher.is_none() {
            bail!("this archive is not encrypted; encryption can only be chosen when an archive is created");
        }
        let mut f = OpenOptions::new().write(true).open(archive)?;
        f.set_len(op.end)?; // drop any partial data from an interrupted update
        f.seek(SeekFrom::Start(op.end))?;
        (f, op.end, op.cipher, op.index)
    } else {
        let kdf = if o.encrypt { Some(KdfParams::new_random()) } else { None };
        let cipher = match &kdf {
            Some(k) => {
                let pw = read_password(true)?;
                Some(Arc::new(Cipher::from_password(pw.as_bytes(), k)?))
            }
            None => None,
        };
        let mut f = File::create(archive)?;
        f.write_all(&Header { kdf }.encode())?;
        (f, HEADER_LEN, cipher, Index::default())
    };
    let first_block = index.blocks.len() as u32;
    let archive_abs = fs::canonicalize(archive).ok();

    // Previous snapshot's files, for skipping unchanged ones.
    let prev: HashMap<&str, &Entry> = index
        .snapshots
        .last()
        .map(|s| s.entries.iter().filter(|e| e.kind == E_FILE).map(|e| (e.path.as_str(), e)).collect())
        .unwrap_or_default();

    // 1. Walk.
    let mut entries: Vec<Entry> = Vec::new();
    let mut paths = Vec::new();
    let (mut skipped, mut reused, mut reused_bytes) = (0usize, 0usize, 0u64);
    for e in walkdir::WalkDir::new(input).follow_links(false) {
        let e = e?;
        if e.depth() == 0 && in_md.is_dir() {
            continue;
        }
        if archive_abs.as_deref().is_some_and(|a| fs::canonicalize(e.path()).ok().as_deref() == Some(a)) {
            continue; // never archive the archive itself
        }
        let rel_os = e.path().strip_prefix(&base)?;
        let rel = rel_os.to_string_lossy().replace('\\', "/");
        if rel_os.to_str().is_none() {
            eprintln!("warning: non-UTF-8 name stored lossily: {rel}");
        }
        let md = e.metadata()?;
        let ft = e.file_type();
        let meta = meta_of(&md);
        if ft.is_dir() {
            entries.push(Entry { path: rel, kind: E_DIR, meta, size: 0, hash: [0; 32], transform: 0, chunks: vec![], target: String::new() });
        } else if ft.is_symlink() {
            let target = fs::read_link(e.path())?.to_string_lossy().to_string();
            entries.push(Entry { path: rel, kind: E_SYMLINK, meta, size: 0, hash: [0; 32], transform: 0, chunks: vec![], target });
        } else if ft.is_file() {
            match prev.get(rel.as_str()) {
                Some(pe) if !o.rehash && pe.size == md.len() && pe.meta.mtime_s == meta.mtime_s && pe.meta.mtime_ns == meta.mtime_ns => {
                    reused += 1;
                    reused_bytes += pe.size;
                    entries.push(file_entry(&rel, meta, pe.size, pe.hash, pe.transform, pe.chunks.clone()));
                }
                _ => paths.push((e.path().to_path_buf(), rel, md.len(), meta)),
            }
        } else {
            skipped += 1;
        }
    }
    drop(prev);
    let mut items: Vec<Item> = paths
        .into_par_iter()
        .map(|(abs, rel, size, meta)| {
            let (kind, magic) = sniff(&abs, o.recomp)?;
            Ok(Item { abs, rel, size, meta, kind, magic })
        })
        .collect::<Result<_>>()?;

    // 2. Group: kind, then path within the top-level folder.
    let tail = |r: &str| r.split_once('/').map(|x| x.1.to_string()).unwrap_or_else(|| r.to_string());
    items.sort_by_cached_key(|f| (f.kind, tail(&f.rel), f.rel.clone()));

    // 3. Compression workers (compress, then seal if encrypted) and the writer.
    let threads = o.threads.max(1);
    let (tx, rx) = mpsc::sync_channel::<Job>(threads);
    let rx = Arc::new(Mutex::new(rx));
    let (wtx, wrx) = mpsc::channel::<Result<(u32, u64, u8, Vec<u8>)>>();
    let mode = o.mode;
    let workers: Vec<_> = (0..threads)
        .map(|_| {
            let rx = Arc::clone(&rx);
            let wtx = wtx.clone();
            let cipher = cipher.clone();
            thread::spawn(move || loop {
                let job = rx.lock().unwrap().recv();
                let Ok((id, kind, data)) = job else { break };
                let r = compress_block(&data, kind, mode).and_then(|(c, b)| {
                    let b = match &cipher {
                        Some(k) => k.seal(ROLE_BLOCK, id as u64, &b)?,
                        None => b,
                    };
                    Ok((id, data.len() as u64, c, b))
                });
                drop(data);
                if wtx.send(r).is_err() {
                    break;
                }
            })
        })
        .collect();
    drop(wtx);
    let mut bw = BufWriter::with_capacity(1 << 20, out.try_clone()?);
    let writer = thread::spawn(move || -> Result<(u64, Vec<(u32, BlockEntry)>, [usize; 4])> {
        let mut off = start;
        let mut blocks = Vec::new();
        let mut codec_count = [0usize; 4];
        for r in wrx {
            let (id, raw_len, codec, bytes) = r?;
            bw.write_all(&bytes)?;
            blocks.push((id, BlockEntry { offset: off, comp_len: bytes.len() as u64, raw_len, codec }));
            codec_count[codec as usize] += 1;
            off += bytes.len() as u64;
        }
        bw.flush()?;
        Ok((off, blocks, codec_count))
    });

    // 4. Feed new and changed files through recompression, chunking and dedup.
    let mut b = Builder {
        block_size,
        seen: index.chunks.iter().enumerate().map(|(i, c)| (c.hash, i as u32)).collect(),
        chunks: std::mem::take(&mut index.chunks),
        open: HashMap::new(),
        next_block: first_block,
        tx,
        dup_bytes: 0,
    };
    let (mut read_bytes, mut streamed) = (0u64, 0usize);
    let mut rstats = [(0usize, 0u64, 0u64); 4];
    let feed = (|| -> Result<()> {
        let mut i = 0;
        while i < items.len() {
            if items[i].streams() {
                let it = &items[i];
                let f = File::open(&it.abs).with_context(|| format!("reading {}", it.abs.display()))?;
                let mut hasher = blake3::Hasher::new();
                let (mut refs, mut size) = (Vec::new(), 0u64);
                for c in fastcdc::v2020::StreamCDC::new(BufReader::with_capacity(1 << 20, f), CDC.0, CDC.1, CDC.2) {
                    let c = c.map_err(|e| anyhow::anyhow!("chunking {}: {e:?}", it.abs.display()))?;
                    hasher.update(&c.data);
                    size += c.length as u64;
                    refs.push(b.add_chunk(it.kind, &c.data)?);
                }
                read_bytes += size;
                streamed += 1;
                entries.push(file_entry(&it.rel, it.meta.clone(), size, *hasher.finalize().as_bytes(), 0, refs));
                i += 1;
                continue;
            }
            let (mut j, mut bytes) = (i, 0u64);
            while j < items.len() && j - i < BATCH_FILES && !items[j].streams() && (j == i || bytes + items[j].size <= BATCH_BYTES) {
                bytes += items[j].size;
                j += 1;
            }
            let prepared: Vec<Prepared> = items[i..j].par_iter().map(prepare).collect::<Result<_>>()?;
            for (it, p) in items[i..j].iter().zip(prepared) {
                read_bytes += p.size;
                let rs = &mut rstats[p.transform as usize];
                rs.0 += 1;
                rs.1 += p.size;
                rs.2 += p.content.len() as u64;
                let refs = b.add_content(p.kind, &p.content)?;
                entries.push(file_entry(&it.rel, it.meta.clone(), p.size, p.hash, p.transform, refs));
            }
            i = j;
        }
        Ok(())
    })();
    let fin = match feed {
        Ok(()) => b.finish(),
        Err(e) => {
            drop(b); // closes the job queue so the workers exit
            Err(e)
        }
    };
    for w in workers {
        let _ = w.join();
    }
    let (off, mut new_blocks, codec_count) = writer.join().map_err(|_| anyhow::anyhow!("writer panicked"))??;
    let (chunks, dup_bytes) = fin?;
    new_blocks.sort_by_key(|(id, _)| *id);
    for (k, (id, _)) in new_blocks.iter().enumerate() {
        if *id as usize != first_block as usize + k {
            bail!("internal error: missing block");
        }
    }

    // 5. New index (all snapshots) + footer, appended after the new blocks.
    index.blocks.extend(new_blocks.into_iter().map(|(_, b)| b));
    index.chunks = chunks;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let label = o.label.clone().unwrap_or_else(|| fs::canonicalize(input).unwrap_or(input.to_path_buf()).to_string_lossy().to_string());
    index.snapshots.push(Snapshot { time_s: now, label, first_block, entries });
    let raw_idx = index.encode();
    let comp_idx = zstd::bulk::compress(&raw_idx, 19)?;
    let blob = match &cipher {
        Some(k) => k.seal(ROLE_INDEX, off, &comp_idx)?,
        None => comp_idx,
    };
    out.seek(SeekFrom::Start(off))?;
    let mut tail = Vec::with_capacity(blob.len() + FOOTER_LEN as usize);
    tail.extend_from_slice(&blob);
    tail.extend_from_slice(&off.to_le_bytes());
    tail.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    tail.extend_from_slice(&(raw_idx.len() as u64).to_le_bytes());
    tail.extend_from_slice(END_MAGIC);
    out.write_all(&tail)?;
    out.sync_all()?; // the new snapshot only counts once it's durably on disk

    let total = fs::metadata(archive)?.len();
    let snap = index.snapshots.last().unwrap();
    let nfiles = snap.entries.iter().filter(|e| e.kind == E_FILE).count();
    let total_in: u64 = snap.entries.iter().filter(|e| e.kind == E_FILE).map(|e| e.size).sum();
    let [_, gz, jp, sp] = rstats;
    eprintln!(
        "snapshot #{} {}: {nfiles} files ({}){} | unchanged {reused} ({}) reused without reading | read {} | \
         dedup matched {} | recompressed {} gzip, {} jpeg, {} zip/png | streamed {streamed} | +{} new blocks [store {} zstd {} xz {} xz+bcj {}] | \
         added {} -> archive now {} | {:.2}s",
        index.snapshots.len(),
        if cipher.is_some() { "(encrypted)" } else { "" },
        human(total_in),
        if skipped > 0 { format!(", {skipped} special files skipped") } else { String::new() },
        human(reused_bytes),
        human(read_bytes),
        human(dup_bytes),
        gz.0, jp.0, sp.0,
        index.blocks.len() - first_block as usize,
        codec_count[0], codec_count[1], codec_count[2], codec_count[3],
        human(total - start),
        human(total),
        t0.elapsed().as_secs_f64()
    );
    let _ = (gz.1, gz.2, jp.1, jp.2, sp.1, sp.2);
    Ok(())
}

pub fn human(b: u64) -> String {
    let b = b as f64;
    if b >= 1073741824.0 {
        format!("{:.2} GiB", b / 1073741824.0)
    } else if b >= 1048576.0 {
        format!("{:.2} MiB", b / 1048576.0)
    } else {
        format!("{:.1} KiB", b / 1024.0)
    }
}
