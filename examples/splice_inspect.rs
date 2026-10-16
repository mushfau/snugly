//! Dev tool: describe a spliced-transform payload (first byte = transform id, as in fuzz inputs).
use snugly::format::get_varint;
fn main() {
    let d = std::fs::read(std::env::args().nth(1).unwrap()).unwrap();
    let t = &d[1..];
    let n = t.len();
    let mlen = u32::from_le_bytes(t[n - 4..].try_into().unwrap()) as usize;
    let Some(mstart) = n.checked_sub(4 + mlen) else { println!("bad meta len"); return };
    let meta = &t[mstart..n - 4];
    let mut q = 0;
    let total = get_varint(meta, &mut q).unwrap();
    let ns = get_varint(meta, &mut q).unwrap();
    println!("total={total} streams={ns} meta={} plain_area={}", meta.len(), mstart);
    for i in 0..ns.min(20) {
        let pl = get_varint(meta, &mut q).unwrap();
        let mut lens = vec![];
        for _ in 0..3 { let l = get_varint(meta, &mut q).unwrap(); lens.push(l); q += l as usize; }
        println!("  stream {i}: plain {pl} corr {} prefix {} suffix {}", lens[0], lens[1], lens[2]);
    }
}
