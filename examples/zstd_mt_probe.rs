//! Dev tool: ratio/speed of zstd multithreading inside one block.
//! usage: zstd_mt_probe <file> <level> [workers:jobMiB:overlapLog ...]
use std::time::Instant;
use zstd::zstd_safe::CParameter;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).unwrap();
    let data = &data[..data.len().min(128 << 20)];
    let level: i32 = a[2].parse().unwrap();
    for spec in &a[3..] {
        let v: Vec<u32> = spec.split(':').map(|x| x.parse().unwrap()).collect();
        let mut c = zstd::bulk::Compressor::new(level).unwrap();
        let wlog = (64 - (data.len().max(1 << 16) as u64 - 1).leading_zeros()).clamp(17, 27);
        c.set_parameter(CParameter::WindowLog(wlog)).unwrap();
        c.set_parameter(CParameter::EnableLongDistanceMatching(true)).unwrap();
        if v[0] > 0 {
            c.set_parameter(CParameter::NbWorkers(v[0])).unwrap();
            if v[1] > 0 { c.set_parameter(CParameter::JobSize(v[1] << 20)).unwrap(); }
            if v[2] > 0 { c.set_parameter(CParameter::OverlapSizeLog(v[2])).unwrap(); }
        }
        let t = Instant::now();
        let out = c.compress(data).unwrap();
        let s = t.elapsed().as_secs_f64();
        println!("{spec:>12}  {:>11} bytes ({:.3}%)  {s:6.2}s  {:.1} MB/s", out.len(), out.len() as f64 * 100.0 / data.len() as f64, data.len() as f64 / 1e6 / s);
    }
}
