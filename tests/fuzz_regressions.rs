//! Inputs found by fuzzing that once crashed snugly or made it allocate gigabytes.
//! Each must now fail cleanly and cheaply.

use std::path::Path;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fuzz_regressions").join(name)).unwrap()
}

#[test]
fn recovery_index_with_absurd_segment_count() {
    // Claimed ~2^60 segments; used to reserve memory for all of them and abort.
    snugly::recovery::fuzz_decode_index(&fixture("recovery_index_huge_count"));
}

#[test]
fn archive_with_jpeg_claiming_huge_dimensions() {
    // A mutated archive whose JPEG data declared enormous dimensions; restoring it used ~4 GB.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.snug");
    std::fs::write(&path, fixture("archive_jpeg_huge_dims.snug")).unwrap();
    let o = snugly::extract::XOpts { only: None, write: false, same_owner: false, cache_blocks: 2, snapshot: None, all_snapshots: true };
    let _ = snugly::extract::extract(&path, dir.path(), &o); // must return (Ok or Err), not abort
}

#[test]
fn spliced_stream_whose_corrections_never_end() {
    // Corrupt preflate corrections used to loop forever, allocating without bound.
    let d = fixture("splice_never_ending_stream");
    let t0 = std::time::Instant::now();
    assert!(snugly::recomp::inverse(d[0] % 4, &d[1..]).is_err());
    assert!(t0.elapsed().as_secs() < 10, "took {:?}", t0.elapsed());
}

#[test]
fn archive_whose_gzip_data_makes_preflate_panic() {
    // preflate panics (index out of bounds) on this data; snugly must report an error, not crash.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.snug");
    std::fs::write(&path, fixture("archive_preflate_panic.snug")).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_snugly")).args(["t", path.to_str().unwrap()]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
    // The archive has several damaged files; files are checked in parallel, so whichever
    // error surfaces first is reported. All of them must be reported as corruption.
    assert!(String::from_utf8_lossy(&out.stderr).contains("corrupt"), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn bsc_block_that_overreads_its_input() {
    // A crafted bsc block made libbsc's range decoder read past the end of its input
    // (found by fuzzing with AddressSanitizer). The patched decoder must reject it cleanly.
    let d = fixture("bsc_range_coder_overread");
    let mut b = d[1..].to_vec();
    if let Some(raw) = snugly::bsc::fuzz_seal(&mut b, 1 << 22) {
        assert!(snugly::codec::decompress(snugly::format::CODEC_BSC, b, raw).is_err());
    }
}

#[test]
fn bsc_block_whose_bwt_is_inconsistent() {
    // libsais's optimised inverse BWT read out of bounds on this block (found by fuzzing with
    // AddressSanitizer); snugly now inverts the BWT itself with bounds checks.
    let d = fixture("bsc_unbwt_wild_read");
    let mut b = d[1..].to_vec();
    if let Some(raw) = snugly::bsc::fuzz_seal(&mut b, 1 << 22) {
        assert!(snugly::codec::decompress(snugly::format::CODEC_BSC, b, raw).is_err());
    }
}

#[test]
fn bsc_block_with_an_escaped_rank_of_zero() {
    // An escaped rank of 0 made libbsc's adaptive decoder index its context tables at -1
    // (found by fuzzing with AddressSanitizer).
    let d = fixture("bsc_rank_zero");
    let mut b = d[1..].to_vec();
    if let Some(raw) = snugly::bsc::fuzz_seal(&mut b, 1 << 22) {
        assert!(snugly::codec::decompress(snugly::format::CODEC_BSC, b, raw).is_err());
    }
}

#[test]
fn bsc_block_with_a_rank_past_the_alphabet() {
    // A decoded rank of 256 or more overflowed libbsc's move-to-front table on the stack
    // (found by fuzzing with AddressSanitizer).
    let d = fixture("bsc_rank_overflow");
    let mut b = d[1..].to_vec();
    if let Some(raw) = snugly::bsc::fuzz_seal(&mut b, 1 << 22) {
        assert!(snugly::codec::decompress(snugly::format::CODEC_BSC, b, raw).is_err());
    }
}

#[test]
fn bsc_block_whose_rank_exponent_runs_away() {
    // With a single-symbol alphabet the rank-exponent loop had no bound and walked off libbsc's
    // model tables (found by fuzzing with AddressSanitizer).
    let d = fixture("bsc_rank_exponent_runaway");
    let mut b = d[1..].to_vec();
    if let Some(raw) = snugly::bsc::fuzz_seal(&mut b, 1 << 22) {
        assert!(snugly::codec::decompress(snugly::format::CODEC_BSC, b, raw).is_err());
    }
}
