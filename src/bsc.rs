//! Bindings to the vendored libbsc: Burrows–Wheeler block sorting with an adaptive QLFC coder
//! (LZP preprocessing is off; its decoder isn't hardened against hostile input). On text it
//! beats zstd -19 by 2–17% in size while compressing about 10x faster; decompression runs at
//! roughly 15–20 MB/s per thread.
//!
//! The vendored copy is patched so every decoder read and write is bounds-checked (see
//! vendor/libbsc/README.md); fuzzing with AddressSanitizer found the original overflows.

use anyhow::{bail, Result};
use std::os::raw::{c_int, c_uchar};
use std::sync::Once;

const FEATURE_FASTMODE: c_int = 1;
const BLOCKSORTER_BWT: c_int = 1;
const CODER_QLFC_ADAPTIVE: c_int = 2;
const LZP_HASH: c_int = 0;
const LZP_MIN: c_int = 0;
const HEADER: usize = 28;

unsafe extern "C" {
    fn bsc_init(features: c_int) -> c_int;
    fn bsc_compress(input: *const c_uchar, output: *mut c_uchar, n: c_int, lzp_hash: c_int, lzp_min: c_int, sorter: c_int, coder: c_int, features: c_int) -> c_int;
    fn bsc_block_info(header: *const c_uchar, header_size: c_int, block_size: *mut c_int, data_size: *mut c_int, features: c_int) -> c_int;
    fn bsc_decompress(input: *const c_uchar, input_size: c_int, output: *mut c_uchar, output_size: c_int, features: c_int) -> c_int;
}

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| unsafe {
        bsc_init(FEATURE_FASTMODE);
    });
}

/// Largest input libbsc accepts in one call.
const MAX_INPUT: usize = (i32::MAX as usize) - HEADER - 64;

pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    init();
    if data.is_empty() || data.len() > MAX_INPUT {
        bail!("bsc: unsupported block size {}", data.len());
    }
    let mut out = vec![0u8; data.len() + HEADER];
    let r = unsafe {
        bsc_compress(data.as_ptr(), out.as_mut_ptr(), data.len() as c_int, LZP_HASH, LZP_MIN, BLOCKSORTER_BWT, CODER_QLFC_ADAPTIVE, FEATURE_FASTMODE)
    };
    if r < 0 {
        bail!("bsc compression failed ({r})");
    }
    out.truncate(r as usize);
    Ok(out)
}

/// Decompress a block that must expand to exactly `raw_len` bytes. libbsc checks the header and
/// an Adler-32 of the data, so corruption is reported instead of producing wrong output.
pub fn decompress(data: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    init();
    if data.len() < HEADER || data.len() > i32::MAX as usize || raw_len > MAX_INPUT {
        bail!("bsc: bad block");
    }
    let (mut block, mut size) = (0 as c_int, 0 as c_int);
    let r = unsafe { bsc_block_info(data.as_ptr(), HEADER as c_int, &mut block, &mut size, FEATURE_FASTMODE) };
    if r != 0 || block as usize != data.len() || size < 0 || size as usize != raw_len {
        bail!("bsc: corrupt block header");
    }
    // Only the two modes snugly writes: stored (0) or BWT + adaptive QLFC without LZP. The
    // rest of libbsc's decoders are never reached from an archive.
    let mode = u32::from_le_bytes(data[8..12].try_into().unwrap());
    if mode != 0 && mode != (CODER_QLFC_ADAPTIVE as u32) << 5 | BLOCKSORTER_BWT as u32 {
        bail!("bsc: unsupported block mode {mode:#x}");
    }
    let mut out = vec![0u8; raw_len];
    let r = unsafe { bsc_decompress(data.as_ptr(), data.len() as c_int, out.as_mut_ptr(), raw_len as c_int, FEATURE_FASTMODE) };
    if r != 0 {
        bail!("bsc: corrupt block ({r})");
    }
    Ok(out)
}

fn adler32(d: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in d.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

/// For fuzzing only: make a mutated block pass libbsc's header and payload checksums, as a
/// crafted archive could, so the decoder itself is exercised. Returns the claimed raw length
/// (capped at `max_raw`), or None if the input is too short.
#[doc(hidden)]
pub fn fuzz_seal(buf: &mut [u8], max_raw: usize) -> Option<usize> {
    if buf.len() < HEADER {
        return None;
    }
    let len = buf.len() as u32;
    buf[0..4].copy_from_slice(&len.to_le_bytes());
    let raw = (u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize % (max_raw + 1)) as u32;
    buf[4..8].copy_from_slice(&raw.to_le_bytes());
    let payload = adler32(&buf[HEADER..]);
    buf[20..24].copy_from_slice(&payload.to_le_bytes());
    let head = adler32(&buf[..24]);
    buf[24..28].copy_from_slice(&head.to_le_bytes());
    Some(raw as usize)
}

/// Inverse BWT, called by the patched libbsc in place of libsais's optimised decoder, which
/// read out of bounds on inconsistent input (found by fuzzing). `t` holds the BWT without its
/// end marker, which sits at row `index`; on success `t` holds the original data.
///
/// Rows are walked forward through the PSI mapping with every index checked, so any input
/// yields either some output (which libbsc's Adler-32 then rejects if wrong) or an error, never
/// a bad access. When the block carries libbsc's sampled indexes (the rows where each 1/8th or
/// so of the data starts), all segments are walked in the same loop: the memory reads of one
/// segment overlap the others', which is what makes this fast.
#[unsafe(no_mangle)]
pub extern "C" fn snugly_unbwt(t: *mut c_uchar, n: c_int, index: c_int, num_indexes: c_int, indexes: *const c_int) -> c_int {
    const BAD_PARAMETER: c_int = -1;
    const NOT_ENOUGH_MEMORY: c_int = -2;
    const DATA_CORRUPT: c_int = -6;
    if t.is_null() || n < 0 || index <= 0 || index > n || !(0..=255).contains(&num_indexes) {
        return BAD_PARAMETER;
    }
    if n <= 1 {
        return 0;
    }
    let n = n as usize;
    let index = index as usize;
    // SAFETY: libbsc passes its output buffer, which holds `n` bytes, and its index table,
    // which holds `num_indexes` entries.
    let t = unsafe { std::slice::from_raw_parts_mut(t, n) };
    let sampled: &[c_int] = if num_indexes > 0 && !indexes.is_null() {
        unsafe { std::slice::from_raw_parts(indexes, num_indexes as usize) }
    } else {
        &[]
    };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), c_int> {
        // Last column: t[row] before the marker row, t[row-1] after it.
        let mut count = [0usize; 256];
        for &c in t.iter() {
            count[c as usize] += 1;
        }
        let mut next = [0usize; 256];
        let mut sum = 1; // row 0 of the first column is the end marker
        for c in 0..256 {
            next[c] = sum;
            sum += count[c];
        }
        // psi[k] = the row whose rotation starts one position after row k's; first[k] = the
        // first character of row k.
        let mut psi: Vec<u32> = Vec::new();
        psi.try_reserve_exact(n + 1).map_err(|_| NOT_ENOUGH_MEMORY)?;
        psi.resize(n + 1, 0);
        let mut first: Vec<u8> = Vec::new();
        first.try_reserve_exact(n + 1).map_err(|_| NOT_ENOUGH_MEMORY)?;
        first.resize(n + 1, 0);
        for row in 0..=n {
            if row == index {
                continue;
            }
            let c = if row < index { t[row] } else { t[row - 1] } as usize;
            let k = next[c];
            psi[k] = row as u32;
            first[k] = c as u8;
            next[c] += 1;
        }
        // Segment starts: the original data begins at row `index`. libbsc samples further rows
        // every `step` positions, with `step` a power of two derived from n.
        let mut step = n / 8;
        for s in [1, 2, 4, 8, 16] {
            step |= step >> s;
        }
        step = (step >> 1) + 1;
        let mut starts: Vec<usize> = vec![index];
        if !sampled.is_empty() && sampled.len() == (n - 1) / step {
            for &x in sampled {
                let row = x as usize + 1;
                if x < 0 || row > n {
                    return Err(DATA_CORRUPT);
                }
                starts.push(row);
            }
        } else {
            step = n;
        }
        let mut out: Vec<u8> = Vec::new();
        out.try_reserve_exact(n).map_err(|_| NOT_ENOUGH_MEMORY)?;
        out.resize(n, 0);
        let mut rows = starts;
        // Segment s covers positions s*step .. min((s+1)*step, n).
        for pos in 0..step {
            for (s, row) in rows.iter_mut().enumerate() {
                let p = s * step + pos;
                if p >= n {
                    break;
                }
                let r = *row;
                if r > n {
                    return Err(DATA_CORRUPT);
                }
                // The rotation starting at position p sits at row r and begins with t[p].
                out[p] = first[r];
                *row = psi[r] as usize;
            }
        }
        t.copy_from_slice(&out);
        Ok(())
    }));
    match r {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => e,
        Err(_) => DATA_CORRUPT,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip_and_checksums_match_libbsc() {
        let text: Vec<u8> = (0..200_000u32).flat_map(|i| format!("line {} of some text\n", i % 977).into_bytes()).collect();
        let c = super::compress(&text).unwrap();
        assert!(c.len() < text.len() / 5);
        assert_eq!(super::decompress(&c, text.len()).unwrap(), text);
        // Our adler32 must agree with libbsc's, or the fuzz harness would test nothing.
        let mut c2 = c.clone();
        assert_eq!(super::fuzz_seal(&mut c2, usize::MAX >> 1), Some(text.len()));
        assert_eq!(c2, c);
        // A corrupted payload is rejected, not decoded into wrong output.
        let mut bad = c.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0x55;
        assert!(super::decompress(&bad, text.len()).is_err());
        assert!(super::decompress(&c, text.len() - 1).is_err());
    }

    #[test]
    fn rust_inverse_bwt_matches_libbsc_on_many_inputs() {
        // Every round trip goes through snugly_unbwt; libbsc's Adler-32 confirms the output.
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for &n in &[2usize, 3, 7, 64, 1000, 65_536, 300_000] {
            for alphabet in [1u64, 2, 4, 26, 256] {
                let d: Vec<u8> = (0..n).map(|_| (rnd() % alphabet) as u8 + b'a' * (alphabet < 200) as u8).collect();
                let c = super::compress(&d).unwrap();
                assert_eq!(super::decompress(&c, n).unwrap(), d, "n={n} alphabet={alphabet}");
            }
        }
    }
}
