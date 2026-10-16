//! Optional performance counters for `snugly c --stats`: where the time goes, stage by stage,
//! and which kinds of file the input is made of. Counters are always collected (an `Instant`
//! costs a few nanoseconds); they are only printed when asked for.
//!
//! CPU-side stages are summed across threads, so they can add up to more than the wall time.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

pub struct Counter {
    pub ns: AtomicU64,
    pub n: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
}

impl Counter {
    const fn new() -> Self {
        Counter { ns: AtomicU64::new(0), n: AtomicU64::new(0), bytes_in: AtomicU64::new(0), bytes_out: AtomicU64::new(0) }
    }
    pub fn add(&self, since: Instant, bytes_in: u64, bytes_out: u64) {
        self.ns.fetch_add(since.elapsed().as_nanos() as u64, Relaxed);
        self.n.fetch_add(1, Relaxed);
        self.bytes_in.fetch_add(bytes_in, Relaxed);
        self.bytes_out.fetch_add(bytes_out, Relaxed);
    }
    /// Like `add`, minus time already counted elsewhere (waiting on a queue).
    pub fn add_excluding(&self, since: Instant, excluded_ns: u64, bytes_in: u64) {
        let ns = (since.elapsed().as_nanos() as u64).saturating_sub(excluded_ns);
        self.ns.fetch_add(ns, Relaxed);
        self.n.fetch_add(1, Relaxed);
        self.bytes_in.fetch_add(bytes_in, Relaxed);
    }
    fn get(&self) -> (f64, u64, u64, u64) {
        (self.ns.load(Relaxed) as f64 / 1e9, self.n.load(Relaxed), self.bytes_in.load(Relaxed), self.bytes_out.load(Relaxed))
    }
}

/// Input file groups for the byte census.
pub const GROUPS: [&str; 12] =
    ["JPEG", "HEIC/AVIF", "video", "PNG", "other images", "audio", "PDF", "Office/ZIP/JAR", "compressed archives", "executables", "text/source", "other"];

pub fn group_of(ext: &str, kind: crate::codec::Kind, magic_jpeg: bool, magic_png: bool, magic_zip: bool) -> usize {
    use crate::codec::Kind;
    if magic_jpeg {
        return 0;
    }
    if magic_png {
        return 3;
    }
    match ext {
        "jpg" | "jpeg" => 0,
        "heic" | "heif" | "avif" => 1,
        "mov" | "mp4" | "m4v" | "mkv" | "webm" | "avi" | "3gp" => 2,
        "png" => 3,
        "gif" | "webp" | "tif" | "tiff" | "bmp" | "dng" | "cr2" | "nef" | "arw" | "raw" | "jxl" | "psd" | "svg" => 4,
        "mp3" | "m4a" | "aac" | "wav" | "flac" | "ogg" | "opus" | "aiff" | "caf" => 5,
        "pdf" => 6,
        _ if magic_zip => 7,
        "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "lz4" | "br" | "dmg" => 8,
        _ => match kind {
            Kind::Executable => 9,
            Kind::Text => 10,
            _ => 11,
        },
    }
}

pub struct Stats {
    pub walk: Counter,
    pub read: Counter,
    pub file_hash: Counter,
    /// Successful recompression by transform (1 gzip, 2 jpeg, 3 zip/png).
    pub recomp: [Counter; 4],
    /// Attempts that failed verification and fell back to storing the original.
    pub recomp_failed: Counter,
    pub chunk: Counter,
    pub queue_wait: Counter,
    pub probe: Counter,
    /// Block compression by kind (text, binary, executable, incompressible).
    pub compress: [Counter; 4],
    pub seal: Counter,
    pub write: Counter,
    pub index: Counter,
    pub groups: [Counter; 12],
}

/// One line per compressed block: (id, kind, raw bytes, compressed bytes, zstd workers, seconds).
pub type BlockLine = (u32, usize, u64, u64, u32, f64);
pub static BLOCKS: std::sync::Mutex<Vec<BlockLine>> = std::sync::Mutex::new(Vec::new());

pub static S: Stats = Stats {
    walk: Counter::new(),
    read: Counter::new(),
    file_hash: Counter::new(),
    recomp: [Counter::new(), Counter::new(), Counter::new(), Counter::new()],
    recomp_failed: Counter::new(),
    chunk: Counter::new(),
    queue_wait: Counter::new(),
    probe: Counter::new(),
    compress: [Counter::new(), Counter::new(), Counter::new(), Counter::new()],
    seal: Counter::new(),
    write: Counter::new(),
    index: Counter::new(),
    groups: [
        Counter::new(), Counter::new(), Counter::new(), Counter::new(), Counter::new(), Counter::new(),
        Counter::new(), Counter::new(), Counter::new(), Counter::new(), Counter::new(), Counter::new(),
    ],
};

fn mb(b: u64) -> f64 {
    b as f64 / 1e6
}

pub fn print(wall: f64, blocks: usize, inner: u32, workers: usize) {
    eprintln!();
    eprintln!("stage timing (up to {blocks} blocks at a time, up to {inner} zstd workers per block, {workers} file workers; CPU stages are summed over threads)");
    eprintln!("  {:<28} {:>9} {:>8} {:>11} {:>11} {:>10}", "stage", "seconds", "count", "in MB", "out MB", "MB/s");
    let row = |name: &str, c: &Counter| {
        let (s, n, i, o) = c.get();
        if n == 0 {
            return;
        }
        let rate = if s > 0.0 && i > 0 { format!("{:.1}", mb(i) / s) } else { String::new() };
        let out = if o > 0 { format!("{:.2}", mb(o)) } else { String::new() };
        let inn = if i > 0 { format!("{:.2}", mb(i)) } else { String::new() };
        eprintln!("  {name:<28} {s:>9.2} {n:>8} {inn:>11} {out:>11} {rate:>10}");
    };
    row("scan folders + sniff", &S.walk);
    row("read files", &S.read);
    row("hash files (BLAKE3)", &S.file_hash);
    row("recompress gzip", &S.recomp[1]);
    row("recompress JPEG", &S.recomp[2]);
    row("recompress ZIP/Office/PNG", &S.recomp[3]);
    row("recompress failed (kept)", &S.recomp_failed);
    row("chunk + dedup", &S.chunk);
    row("waiting for compressors", &S.queue_wait);
    row("entropy probe", &S.probe);
    row("compress text blocks", &S.compress[0]);
    row("compress binary blocks", &S.compress[1]);
    row("compress executable blocks", &S.compress[2]);
    row("compress media blocks", &S.compress[3]);
    row("encrypt", &S.seal);
    row("write", &S.write);
    row("index + footer", &S.index);
    eprintln!("  {:<28} {wall:>9.2}", "total (wall clock)");

    let mut blocks = BLOCKS.lock().unwrap().clone();
    blocks.sort_by_key(|b| b.0);
    if !blocks.is_empty() {
        const KINDS: [&str; 4] = ["text", "binary", "executable", "media"];
        eprintln!();
        eprintln!("blocks");
        eprintln!("  {:>5} {:<11} {:>10} {:>10} {:>8} {:>9}", "id", "kind", "raw MB", "out MB", "workers", "seconds");
        for (id, k, raw, comp, w, s) in blocks {
            eprintln!("  {id:>5} {:<11} {:>10.2} {:>10.2} {w:>8} {s:>9.2}", KINDS[k], mb(raw), mb(comp));
        }
    }

    let total: u64 = S.groups.iter().map(|c| c.bytes_in.load(Relaxed)).sum();
    if total == 0 {
        return;
    }
    eprintln!();
    eprintln!("input by file type (files read this run)");
    eprintln!("  {:<22} {:>8} {:>11} {:>7} {:>16}", "type", "files", "MB", "share", "after recompress");
    for (i, c) in S.groups.iter().enumerate() {
        let (_, n, b, o) = c.get();
        if n == 0 {
            continue;
        }
        eprintln!(
            "  {:<22} {n:>8} {:>11.2} {:>6.1}% {:>15.1}%",
            GROUPS[i],
            mb(b),
            b as f64 * 100.0 / total as f64,
            if b > 0 { o as f64 * 100.0 / b as f64 } else { 100.0 }
        );
    }
}
