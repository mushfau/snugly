# Benchmarks

Sizes are in MB (10^6 bytes) and times are wall-clock seconds, unless noted. Every snugly archive below was extracted again and compared byte for byte with the original, with each file's BLAKE3 hash checked as well.

These numbers were measured on pre-release builds while snugly was being developed, so results from the current build may differ slightly.

**Machines:**
- **Mac:** Apple M5 Pro, 24 GB RAM, macOS.
- **VM:** 2 vCPU Linux virtual machine.

**Tools compared:** zip, 7-Zip (LZMA2 9e, solid, "Ultra"), tar.xz and tar.zst. RAR was not tested, because its compressor is proprietary.

## Photos

### Photo originals, 1.66 GB (Mac)

| Tool | Size | Create |
|---|---|---|
| zip -9 | 1,596.6 | 25 s |
| 7-Zip Ultra | 1,532.3 | ~50 min |
| **snugly balanced** | **1,456.9** | **13 s** |

Running an update on that archive with nothing changed took 0.13 s.

### Mixed photo library sample, 122 MB (VM)

The sample was 100 original JPEGs, 64 JPEG previews, 20 HEIC images, 10 MOV videos and 6 PNGs. HEIC and MOV make up 82.7 MB of it.

| Tool | Size | Ratio | Create | Extract |
|---|---|---|---|---|
| zip -9 | 118.70 | 97.1% | 3.7 | 0.8 |
| 7-Zip Ultra ¹ | 116.37 | 95.2% | 88 | 2.4 |
| tar.xz -9e | 116.38 | 95.2% | 91 | 2.2 |
| tar.zst -19 --long | 116.54 | 95.4% | 35 | 0.16 |
| **snugly balanced** | **110.39** | 90.3% | **10** | 3.9 |
| **snugly max** | **110.02** | 90.0% | 90 | 4.2 |

- 162 of the 164 JPEGs were recompressed losslessly, 25% smaller. The other two were stored as they were.
- HEIC and MOV are already compressed with HEVC/H.264, so no tool gains much on them. Max mode still runs LZMA on them, which wins 1–4%.

### JPEGs only, 11.6 MB, 138 files (VM)

The test set was real JPEGs at several quality levels, both baseline and progressive.

| Tool | Size | Create | Extract |
|---|---|---|---|
| zip -9 | 10.49 | 0.4 | 0.15 |
| 7-Zip Ultra ¹ | 9.32 | 4.3 | 0.69 |
| tar.xz -9e | 9.33 | 4.3 | 0.46 |
| tar.zst -19 --long | 9.31 | 3.4 | 0.05 |
| **snugly (any mode)** | **7.71** | 3.4 | 1.2 |

## Documents, ZIPs and PNGs, 58 MB, 242 files (VM)

The test set had 31 Office files (docx, xlsx, pptx, odt, ods), 55 JARs, 22 ZIPs and Python wheels, and 134 PNGs.

| Type | snugly max, without recompression | snugly max | Saved |
|---|---|---|---|
| Office | 0.88 | 0.52 | 41% |
| JAR | 10.34 | 5.02 | 51% |
| ZIP / wheel | 14.99 | 6.94 | 54% |
| PNG | 8.40 | 7.31 | 13% |
| **Whole set** | — | **21.0** (max) / 22.5 (balanced) | **39% smaller than 7-Zip Ultra (34.7)** |

## Source code and binaries (VM)

**mixed** (194 MB, 6,280 files): the Python 3.12 standard library, Rust crate sources, 1,102 .gz documentation files and about 160 Linux executables.

| Tool | Size | Create | Extract |
|---|---|---|---|
| zip -9 | 73.46 | 26.9 | 1.6 |
| 7-Zip Ultra ¹ | 37.54 | 135 | 6.6 |
| tar.xz -9e | 37.67 | 143 | 2.8 |
| tar.zst -19 --long | 39.87 | 73 | 0.37 |
| snugly fast | 48.93 | 5.3 | 1.4 |
| snugly balanced | 39.55 | 57 | 2.3 |
| **snugly max** | **37.14** | 94 | 3.4 |

**backup** (183 MB, 3,763 files): the Python 3.11, 3.12 and 3.13 standard libraries side by side, so most of the files are near-duplicates.

| Tool | Size | Create | Extract |
|---|---|---|---|
| zip -9 | 60.38 | 51.0 | 2.5 |
| 7-Zip Ultra ¹ | **26.62** | 145 | 4.3 |
| tar.xz -9e | 28.98 | 150 | 2.5 |
| tar.zst -19 --long | 30.54 | 65 | 0.32 |
| snugly fast | 39.45 | 1.4 | 0.59 |
| snugly balanced | 29.44 | 67 | 0.83 |
| snugly max | 26.65 | 122 | 2.2 |

On plain text and code, snugly max ties 7-Zip Ultra on size and creates the archive 15–30% faster. The bigger wins come from data that can be recompressed. Extracting a single file is 6–15× faster than with 7-Zip, because snugly only decodes the blocks that file uses.

¹ The 7z archives on the VM were made with py7zr, which uses liblzma, the same LZMA2 9e stream as 7-Zip. Its extract times include Python overhead.

## Incremental snapshots (VM)

The day 1 snapshot was 6,280 files (185 MB of code, documentation and binaries). For day 2:
- 40 files were edited and 25 deleted;
- 100 files were touched without changing their content;
- 60 files were added, a folder was renamed, and a 20 MB log was added.

| | Time | Data added | Archive total |
|---|---|---|---|
| Day 1, first snapshot | 65 s | 37.8 MB | 37.8 MB |
| Day 2 update | **21 s** | **5.3 MB** | 43.1 MB |
| Day 3 update, nothing changed | **1.0 s** | 0.47 MB (index only) | 43.6 MB |
| *A fresh archive of day 2, for comparison* | 74 s | 44.1 MB | — |

- 6,123 of the 6,316 files were reused without being read.
- The renamed folder and the touched files added nothing.
- Every snapshot restored identically, including permissions and timestamps.
- Restoring one file from snapshot 1 decoded 1 of 7 blocks.
- An update killed with `kill -9` partway through left the archive readable at the previous snapshot. The next update cleaned up the partial data.

## Encryption (VM)

- **Overhead:** about 200 bytes on a 39.7 MB archive.
- **No plaintext visible:** no file contents, file names or compression headers appear in the archive.
- **Wrong password:** rejected in 0.25 s, before anything is written.
- **Tampering:** a single flipped byte in any block or in the index is detected.

## Repair (VM)

A 629 MB archive was protected with `-r 5`, which made a 32 MB `.rec` file in 1.4 s. Each kind of damage was applied to a fresh copy of the archive. After `snugly repair`, the copy was verified and compared with the original.

| Damage | Result |
|---|---|
| 40 scattered 4 KiB "bad sectors" | repaired, byte-identical |
| 5 MiB contiguous burst | repaired, byte-identical |
| Last 2 MiB destroyed (archive unopenable) | repaired, byte-identical |
| Last 3 MiB cut off | repaired, byte-identical |
| Archive damage, plus damage to the `.rec` file itself | repaired, byte-identical |
| Encrypted archive with damaged header and footer | repaired; decrypts and verifies |
| 20 MiB burst in one 128 MiB segment (beyond 5%) | refused with a clear message; nothing written |

- **Speed:** a repair took 1.4–4 s. `--check` on a healthy archive took 0.9 s.
- **After an update:** refreshing the `.rec` file took 0.55 s, because only the last segment is recomputed.
- **Size:** the `.rec` file is about the chosen percentage of the archive, with a 64 KiB minimum.

## Memory (VM, balanced mode, 2 threads)

| Input | Create (peak) | Extract (peak) |
|---|---|---|
| 360 MB | 689 MB | 371 MB |
| 2.05 GB | 1,130 MB | 798 MB |

Memory levels off once the pipeline is full. It doesn't keep growing with input size.

Extracting an archive of 120 unique JPEGs peaked at 513 MB with 8 decode threads and 320 MB with 4.

## Reliability testing

- **Automated tests:** the end-to-end suite covers:
  - round-trips in every mode, with metadata;
  - recompression of gzip, JPEG, PNG and docx;
  - encryption, snapshots, interrupted updates and repair;
  - hostile paths;
  - 120 deliberately corrupted archives, which must all fail cleanly.
- **Fuzzing:** there are four fuzz targets: index parsing, the recompression decoders, whole-archive verification and recovery-file parsing. They found four bugs, now fixed and kept as regression tests:
  - an oversized memory reservation from a crafted recovery file;
  - unlimited JPEG dimensions;
  - an infinite loop in preflate-rs on corrupt data (patched in `vendor/`);
  - a decoder panic, which was already handled safely.
- **After the fixes:** the targets ran for 33 million, 126 million, 344,000 and 571,000 inputs with no new crash, hang or runaway memory use.
