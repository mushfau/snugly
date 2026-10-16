//! Dev tool: report what preflate makes of raw deflate streams.
fn main() {
    for f in std::env::args().skip(1) {
        let d = std::fs::read(&f).unwrap();
        let cfg = preflate_rs::PreflateConfig::default();
        match preflate_rs::preflate_whole_deflate_stream(&d, &cfg) {
            Ok((r, p)) => println!("{f}: deflate {} -> plain {} corrections {} consumed {}", d.len(), p.text().len(), r.corrections.len(), r.compressed_size),
            Err(e) => println!("{f}: error {e:?}"),
        }
    }
}
