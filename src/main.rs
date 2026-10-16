//! snugly — a prototype archiver combining the strongest ideas of ZIP, 7z and RAR:
//!
//! 1. Recompression: gzip streams are inflated (preflate) and JPEGs re-encoded (lepton),
//!    both bit-exact reversible.
//! 2. Content-defined chunking + BLAKE3 dedup across all files and all snapshots.
//! 3. Files grouped by kind and by path within the top-level folder.
//! 4. Bounded solid blocks: solid ratios, parallel compression, fast single-file extraction.
//! 5. Per-block codec choice (zstd / LZMA2 / LZMA2+BCJ), sampled in max mode.
//! 6. Seekable index, per-file BLAKE3, Unix metadata (permissions, mtime, symlinks, dirs).
//! 7. Streaming in both directions: memory stays bounded regardless of input size.
//! 8. Optional encryption: Argon2id + XChaCha20-Poly1305 over every block and the index.
//! 9. Incremental snapshots: updates append only new data; unchanged files aren't even read.
//! 10. Recovery records: Reed–Solomon parity in `<archive>.rec` repairs damaged pieces.


use anyhow::Result;
use snugly::{codec, create, extract, recovery};
use clap::{Parser, Subcommand};
use codec::Mode;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "snugly", version, about = "snugly — packs your files snugly: recompressing, deduplicating, encrypting, incremental, repairable archives")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

fn default_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).min(4)
}

/// Threads for decoding/recompressing files in parallel. Each thread can hold a whole
/// recompressed file plus its decoder state, so this — not archive size — sets peak memory.
fn default_workers() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).min(8)
}

fn with_pool<T: Send>(threads: usize, f: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    rayon::ThreadPoolBuilder::new().num_threads(threads.max(1)).build()?.install(f)
}

#[derive(Subcommand)]
enum Cmd {
    /// Create an archive, or add a snapshot to one with --update
    C {
        archive: PathBuf,
        input: PathBuf,
        #[arg(short, long, value_enum, default_value = "balanced")]
        mode: Mode,
        /// Solid block size in MiB
        #[arg(short, long, default_value_t = 128)]
        block_mib: usize,
        /// Disable recompression of JPEG, gzip, ZIP/Office and PNG files
        #[arg(long)]
        no_recomp: bool,
        /// Blocks compressed in parallel (each needs roughly 1-1.5 GB in max mode)
        #[arg(short = 't', long, default_value_t = default_threads())]
        threads: usize,
        /// Encrypt with a password (prompted, or SNUGLY_PASSWORD). Only when creating.
        #[arg(short, long)]
        encrypt: bool,
        /// Add a new snapshot to an existing archive, storing only new data
        #[arg(short, long)]
        update: bool,
        /// With --update: re-read every file instead of trusting unchanged size + mtime
        #[arg(long)]
        rehash: bool,
        /// Label for this snapshot (default: the source path)
        #[arg(long)]
        label: Option<String>,
        /// Write recovery data (<archive>.rec) able to repair this % of damage per 128 MiB.
        /// An existing .rec file is refreshed automatically on --update.
        #[arg(short, long, value_name = "PERCENT")]
        recovery: Option<f64>,
    },
    /// Create or refresh recovery data (<archive>.rec) for an archive
    Protect {
        archive: PathBuf,
        /// Percent of each 128 MiB segment that can be rebuilt (default 5, or the existing file's)
        #[arg(short, long, value_name = "PERCENT")]
        recovery: Option<f64>,
        #[arg(short = 't', long, default_value_t = default_threads())]
        threads: usize,
    },
    /// Find and fix damaged parts of an archive using its .rec file
    Repair {
        archive: PathBuf,
        /// Only report damage; don't write anything
        #[arg(long)]
        check: bool,
    },
    /// Extract a snapshot (default: latest), optionally one file or folder
    X {
        archive: PathBuf,
        outdir: PathBuf,
        /// Snapshot number from `snugly s` (default: latest)
        #[arg(short, long)]
        snapshot: Option<usize>,
        /// Extract only this path or folder (as shown by `snugly l`)
        #[arg(long)]
        only: Option<String>,
        /// Restore file owners (uid/gid) as well — normally only useful as root
        #[arg(long)]
        same_owner: bool,
        /// Decoded blocks kept in memory at once
        #[arg(long, default_value_t = 4)]
        cache_blocks: usize,
        /// Files decoded in parallel (each may need tens of MB for recompressed photos)
        #[arg(short = 't', long, default_value_t = default_workers())]
        threads: usize,
    },
    /// List the contents of a snapshot (default: latest)
    L {
        archive: PathBuf,
        #[arg(short, long)]
        snapshot: Option<usize>,
    },
    /// List snapshots
    S { archive: PathBuf },
    /// Verify every file in every snapshot (decrypt, decompress, check BLAKE3)
    T {
        archive: PathBuf,
        #[arg(long, default_value_t = 4)]
        cache_blocks: usize,
        /// Files decoded in parallel
        #[arg(short = 't', long, default_value_t = default_workers())]
        threads: usize,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::C { archive, input, mode, block_mib, no_recomp, threads, encrypt, update, rehash, label, recovery } => {
            let rec = recovery::sidecar_path(&archive);
            if !archive.exists() && rec.exists() {
                std::fs::remove_file(&rec)?; // stale recovery file from an archive that no longer exists
            }
            with_pool(default_workers(), || {
                create::create(&archive, &input, &create::Opts { mode, block_mib, recomp: !no_recomp, threads, encrypt, update, rehash, label })
            })?;
            if recovery.is_some() || rec.exists() {
                recovery::protect(&archive, recovery, threads)?;
            }
            Ok(())
        }
        Cmd::Protect { archive, recovery, threads } => recovery::protect(&archive, recovery, threads).map(|_| ()),
        Cmd::Repair { archive, check } => recovery::repair(&archive, check),
        Cmd::X { archive, outdir, snapshot, only, same_owner, cache_blocks, threads } => with_pool(threads, || {
            extract::extract(
                &archive,
                &outdir,
                &extract::XOpts { only: only.as_deref(), write: true, same_owner, cache_blocks, snapshot, all_snapshots: false },
            )
        }),
        Cmd::L { archive, snapshot } => extract::list(&archive, snapshot),
        Cmd::S { archive } => extract::snapshots(&archive),
        Cmd::T { archive, cache_blocks, threads } => with_pool(threads, || {
            extract::extract(
                &archive,
                std::path::Path::new("."),
                &extract::XOpts { only: None, write: false, same_owner: false, cache_blocks, snapshot: None, all_snapshots: true },
            )
        }),
    }
}
