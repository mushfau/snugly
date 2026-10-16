//! Dev tool: time bsc compress / decompress on a file (first 128 MiB).
fn main() {
    let d = std::fs::read(std::env::args().nth(1).unwrap()).unwrap();
    let d = &d[..d.len().min(128 << 20)];
    let t = std::time::Instant::now();
    let c = snugly::bsc::compress(d).unwrap();
    let ct = t.elapsed().as_secs_f64();
    let t = std::time::Instant::now();
    let back = snugly::bsc::decompress(&c, d.len()).unwrap();
    let dt = t.elapsed().as_secs_f64();
    assert_eq!(back, d);
    println!("{} -> {} bytes, compress {ct:.2}s ({:.1} MB/s), decompress {dt:.2}s ({:.1} MB/s)", d.len(), c.len(), d.len() as f64 / 1e6 / ct, d.len() as f64 / 1e6 / dt);
}
