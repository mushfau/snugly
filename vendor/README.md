# Vendored crates

## preflate-rs 0.7.6 (Apache-2.0, https://github.com/microsoft/preflate-rs)

There is one change, in `src/stream_processor.rs` (`recreate_blocks`). With corrupt correction data, the end-of-stream flag could stay unset forever, so rebuilding a deflate stream looped indefinitely while allocating without limit. The loop is now bounded:
- at most 16 blocks after the input is used up;
- at most input length + 64 blocks in total.

Exceeding either bound returns an error.

The problem was found by fuzzing snugly's `recomp_inverse` target. The fix will be offered upstream.
