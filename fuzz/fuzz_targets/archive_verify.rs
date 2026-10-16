#![no_main]
// Whole-archive path: open (footer scan, index), decode blocks, rebuild and hash every file.
use libfuzzer_sys::fuzz_target;
use std::io::Write;
// snugly turns panics from third-party decoders into errors (catch_unwind). libFuzzer's hook
// would abort on them anyway, so install a quiet hook: panics that escape snugly still crash.
static QUIET: std::sync::Once = std::sync::Once::new();

fuzz_target!(|data: &[u8]| {
    QUIET.call_once(|| std::panic::set_hook(Box::new(|_| {})));
    let mut data = data.to_vec();
    if data.len() > 4 {
        data[4] &= !1; // keep it unencrypted: Argon2 per input would make fuzzing crawl
    }
    let dir = std::env::temp_dir().join(format!("snugly-fuzz-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("in.snug");
    std::fs::File::create(&path).unwrap().write_all(&data).unwrap();
    let o = snugly::extract::XOpts { only: None, write: false, same_owner: false, cache_blocks: 2, snapshot: None, all_snapshots: true };
    let _ = snugly::extract::extract(&path, &dir, &o);
});
