# Vendored crates

## preflate-rs 0.7.6 (Apache-2.0, https://github.com/microsoft/preflate-rs)

There is one change, in `src/stream_processor.rs` (`recreate_blocks`). With corrupt correction data, the end-of-stream flag could stay unset forever, so rebuilding a deflate stream looped indefinitely while allocating without limit. The loop is now bounded:
- at most 16 blocks after the input is used up;
- at most input length + 64 blocks in total.

Exceeding either bound returns an error.

The problem was found by fuzzing snugly's `recomp_inverse` target. The fix will be offered upstream.

## libbsc 3.3.12 (Apache-2.0, https://github.com/IlyaGrebnov/libbsc, commit 4178579)

Only the library sources are vendored (no CUDA, no command-line tool). It is built without OpenMP by `build.rs`, with `LIBBSC_NO_SORT_TRANSFORM`, and snugly only writes one kind of block: BWT with the adaptive QLFC coder and no LZP.

libbsc trusts the sizes and symbols inside a compressed block, so a crafted archive could make it read or write out of bounds. Fuzzing with AddressSanitizer found five such bugs, each now a regression test in `tests/fuzz_regressions/`:
1. the range decoder read past the end of its input;
2. libsais's optimised inverse BWT read out of bounds when a block's data and index disagree;
3. an escaped rank of 0 indexed the coder's context tables at -1;
4. a rank of 256 or more overflowed the move-to-front table;
5. with a one-symbol alphabet, the rank-exponent loop had no bound and walked off the model tables.

All changes are marked `snugly:` in the source:
- `coder/common/rangecoder.h`: the decoder knows where its input ends and stops reading there.
- `coder/qlfc/qlfc.cpp` (adaptive decoder): the decoded length must fit the output buffer, a run may not overflow it, and the rank and run-length exponents are capped at the sizes of the model tables they index.
- `coder/coder.cpp`: decoding takes the real input and output sizes; the table of sub-blocks is validated before use; only the adaptive coder is accepted.
- `coder/qlfc/qlfc.cpp` also rejects ranks outside 1–255 and bounds the rank-exponent loop for every alphabet size.
- `libbsc/libbsc.cpp`: the inverse BWT is done by `snugly_unbwt` in `src/bsc.rs` (a bounds-checked PSI walk that decodes libbsc's sampled segments side by side) instead of libsais; impossible header sizes, an index table outside the block, out-of-range BWT indexes, stored blocks shorter than they claim, and any block using LZP are rejected.

snugly's wrapper (`src/bsc.rs`) additionally accepts only the two block modes it writes. The `block_decode` fuzz target fixes up libbsc's checksums on each mutated input (as an attacker could), so the decoders themselves are exercised.
