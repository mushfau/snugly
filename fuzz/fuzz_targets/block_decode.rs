#![no_main]
// Decoding a block with each codec on hostile data. For bsc the harness first fixes up
// libbsc's checksums, as a crafted archive could, so its BWT and entropy decoders are reached.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    let codec = sel % 5;
    if codec == snugly::format::CODEC_BSC {
        let mut b = rest.to_vec();
        if let Some(raw) = snugly::bsc::fuzz_seal(&mut b, 1 << 22) {
            let _ = snugly::codec::decompress(codec, b, raw);
        }
    } else if rest.len() >= 3 {
        let raw = (rest[0] as usize | (rest[1] as usize) << 8 | (rest[2] as usize) << 16) % (1 << 22);
        let _ = snugly::codec::decompress(codec, rest[3..].to_vec(), raw);
    }
});
