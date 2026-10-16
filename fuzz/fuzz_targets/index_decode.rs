#![no_main]
// Archive header and index parsing: arbitrary bytes must never panic or over-allocate.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let _ = snugly::format::Header::decode(data);
    let _ = snugly::format::Index::decode(data);
});
