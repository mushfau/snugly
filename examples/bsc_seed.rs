//! Dev tool: write valid bsc blocks as fuzz seeds (selector byte 4 + block).
fn main() {
    let dir = std::path::Path::new(&std::env::args().nth(1).unwrap()).to_path_buf();
    let samples: Vec<Vec<u8>> = vec![
        (0..3000u32).flat_map(|i| format!("the quick brown fox {}\n", i % 50).into_bytes()).collect(),
        (0..20000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect(),
        b"abracadabra".repeat(400),
        (0..5000u32).flat_map(|i| [(i % 7) as u8, 0, (i % 3) as u8]).collect(),
    ];
    for (i, s) in samples.iter().enumerate() {
        let mut v = vec![snugly::format::CODEC_BSC];
        v.extend(snugly::bsc::compress(s).unwrap());
        std::fs::write(dir.join(format!("bsc{i}")), v).unwrap();
    }
}
