#![no_main]
// Undoing gzip / JPEG / ZIP+PNG recompression on hostile data (it comes from the archive).
use libfuzzer_sys::fuzz_target;
// snugly turns panics from third-party decoders into errors (catch_unwind). libFuzzer's hook
// would abort on them anyway, so install a quiet hook: panics that escape snugly still crash.
static QUIET: std::sync::Once = std::sync::Once::new();

fuzz_target!(|data: &[u8]| {
    QUIET.call_once(|| std::panic::set_hook(Box::new(|_| {})));
    if let Some((&t, rest)) = data.split_first() {
        let _ = snugly::recomp::inverse(t % 4, rest);
    }
});
