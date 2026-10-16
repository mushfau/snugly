//! Dev tool: write `[transform id] + transformed bytes` for each input file (fuzz seeds).
fn main() {
    let mut args = std::env::args().skip(1);
    let out = std::path::PathBuf::from(args.next().expect("usage: transform_seed OUTDIR FILES..."));
    for (i, f) in args.enumerate() {
        let d = std::fs::read(&f).unwrap();
        if let Some((t, c)) = snugly::recomp::forward(&d) {
            let mut v = vec![t];
            v.extend_from_slice(&c);
            std::fs::write(out.join(format!("t{i}")), v).unwrap();
            println!("{f}: transform {t}, {} -> {} bytes", d.len(), c.len());
        }
    }
}
