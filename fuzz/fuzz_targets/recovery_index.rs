#![no_main]
// Recovery-file index parsing.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    snugly::recovery::fuzz_decode_index(data);
});
