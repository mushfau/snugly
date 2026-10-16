//! End-to-end tests: every test runs the real `snugly` binary against generated folders and
//! compares what comes back — contents, permissions, timestamps, symlinks, empty folders.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PW: &str = "correct horse battery staple";

fn snugly(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_snugly")).args(args).env_remove("SNUGLY_PASSWORD").output().expect("run snugly")
}
fn snugly_pw(pw: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_snugly")).args(args).env("SNUGLY_PASSWORD", pw).output().expect("run snugly")
}
fn ok(o: Output) -> Output {
    assert!(o.status.success(), "snugly failed: {}\n{}", o.status, String::from_utf8_lossy(&o.stderr));
    o
}
fn fails(o: &Output) -> String {
    assert!(!o.status.success(), "expected failure but snugly succeeded");
    let err = String::from_utf8_lossy(&o.stderr).to_string();
    assert!(!err.contains("panicked"), "snugly panicked instead of failing cleanly:\n{err}");
    err
}
fn p(x: &Path) -> &str {
    x.to_str().unwrap()
}

/// Deterministic pseudo-random bytes (no rand dependency).
fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn set_mtime(path: &Path, secs: i64, nanos: u32) {
    let t = filetime::FileTime::from_unix_time(secs, nanos);
    filetime::set_symlink_file_times(path, t, t).unwrap();
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A minimal ZIP (deflate entries, zlib-style encoder — like Office and most tools).
fn make_zip(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    use flate2::{write::DeflateEncoder, Compression, Crc};
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let mut crc = Crc::new();
        crc.update(data);
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(6));
        enc.write_all(data).unwrap();
        let comp = enc.finish().unwrap();
        let off = out.len() as u32;
        let hdr = |sig: &[u8], central: bool| {
            let mut h = sig.to_vec();
            if central {
                h.extend_from_slice(&20u16.to_le_bytes());
            }
            h.extend_from_slice(&20u16.to_le_bytes()); // version needed
            h.extend_from_slice(&0u16.to_le_bytes()); // flags
            h.extend_from_slice(&8u16.to_le_bytes()); // deflate
            h.extend_from_slice(&[0, 0, 0x21, 0]); // time, date
            h.extend_from_slice(&crc.sum().to_le_bytes());
            h.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            h.extend_from_slice(&(data.len() as u32).to_le_bytes());
            h.extend_from_slice(&(name.len() as u16).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes()); // extra
            if central {
                h.extend_from_slice(&[0; 6]); // comment len, disk number, internal attributes
                h.extend_from_slice(&[0; 4]); // external attributes
                h.extend_from_slice(&off.to_le_bytes());
            }
            h.extend_from_slice(name.as_bytes());
            h
        };
        out.extend(hdr(b"PK\x03\x04", false));
        out.extend_from_slice(&comp);
        central.extend(hdr(b"PK\x01\x02", true));
    }
    let cd_off = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// A minimal valid PNG (RGB, zlib-compressed scanlines, split over two IDAT chunks).
fn make_png(w: u32, h: u32) -> Vec<u8> {
    use flate2::{write::ZlibEncoder, Compression, Crc};
    let mut raw = Vec::new();
    for y in 0..h {
        raw.push(0); // filter: none
        for x in 0..w {
            raw.extend_from_slice(&[(x * 7 + y) as u8, (y * 3) as u8, ((x ^ y) * 5) as u8]);
        }
    }
    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    z.write_all(&raw).unwrap();
    let z = z.finish().unwrap();
    let chunk = |out: &mut Vec<u8>, ty: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut crc = Crc::new();
        crc.update(ty);
        crc.update(data);
        out.extend_from_slice(ty);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc.sum().to_be_bytes());
    };
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    let mid = z.len() / 2;
    chunk(&mut out, b"IDAT", &z[..mid]);
    chunk(&mut out, b"IDAT", &z[mid..]);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A folder exercising content types and metadata edge cases.
fn fixture(root: &Path) {
    fs::create_dir_all(root.join("docs/deep/er")).unwrap();
    fs::create_dir_all(root.join("empty")).unwrap();
    fs::create_dir_all(root.join("ünïcode 日本")).unwrap();
    let text: String = (0..4000).map(|i| format!("line {i}: the quick brown fox {}\n", i * 7 % 13)).collect();
    fs::write(root.join("docs/notes.txt"), &text).unwrap();
    fs::write(root.join("docs/deep/er/copy.txt"), &text).unwrap(); // duplicate → dedup
    fs::write(root.join("random.bin"), noise(1, 300_000)).unwrap();
    fs::write(root.join("empty.txt"), b"").unwrap();
    fs::write(root.join("ünïcode 日本/ファイル.txt"), "x".repeat(100)).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(text.as_bytes()).unwrap();
    fs::write(root.join("log.txt.gz"), gz.finish().unwrap()).unwrap();
    fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample.jpg"), root.join("photo.jpg")).unwrap();
    fs::write(root.join("image.png"), make_png(120, 90)).unwrap();
    let xml: String = (0..800).map(|i| format!("<w:p><w:r><w:t>paragraph {i}</w:t></w:r></w:p>")).collect();
    fs::write(root.join("report.docx"), make_zip(&[("[Content_Types].xml", b"<Types/>".repeat(30)), ("word/document.xml", xml.into_bytes())])).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("docs/notes.txt", root.join("link-rel")).unwrap();
        std::os::unix::fs::symlink("/does/not/exist", root.join("link-dangling")).unwrap();
        fs::write(root.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        chmod(&root.join("run.sh"), 0o755);
        fs::write(root.join("secret.txt"), "private").unwrap();
        chmod(&root.join("secret.txt"), 0o600);
        fs::write(root.join("readonly.txt"), "ro").unwrap();
        chmod(&root.join("readonly.txt"), 0o444);
    }
    set_mtime(&root.join("docs/notes.txt"), 631_231_445, 123_456_789); // 1990, with nanoseconds
    set_mtime(&root.join("empty.txt"), -152_409_233, 0); // 1965, before the epoch
    set_mtime(&root.join("empty"), 946_666_799, 0);
}

#[derive(Debug, PartialEq)]
enum Node {
    File { mode: u32, mtime: (i64, u32), hash: [u8; 32] },
    Dir { mode: u32, mtime: (i64, u32) },
    Link { target: PathBuf },
}

fn tree(root: &Path) -> BTreeMap<String, Node> {
    let mut m = BTreeMap::new();
    for e in walkdir::WalkDir::new(root).min_depth(1) {
        let e = e.unwrap();
        let rel = e.path().strip_prefix(root).unwrap().to_string_lossy().to_string();
        let md = fs::symlink_metadata(e.path()).unwrap();
        let ft = filetime::FileTime::from_last_modification_time(&md);
        let mtime = (ft.unix_seconds(), ft.nanoseconds());
        #[cfg(unix)]
        let mode = std::os::unix::fs::PermissionsExt::mode(&md.permissions()) & 0o7777;
        #[cfg(not(unix))]
        let mode = 0;
        let node = if md.file_type().is_symlink() {
            Node::Link { target: fs::read_link(e.path()).unwrap() }
        } else if md.is_dir() {
            Node::Dir { mode, mtime }
        } else {
            Node::File { mode, mtime, hash: *blake3::hash(&fs::read(e.path()).unwrap()).as_bytes() }
        };
        m.insert(rel, node);
    }
    m
}

fn assert_same(a: &Path, b: &Path) {
    let (ta, tb) = (tree(a), tree(b));
    assert_eq!(ta.keys().collect::<Vec<_>>(), tb.keys().collect::<Vec<_>>(), "different file lists");
    for (k, v) in &ta {
        assert_eq!(v, &tb[k], "difference at {k}");
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    src: PathBuf,
}
fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let src = root.join("src");
    fixture(&src);
    Env { _tmp: tmp, root, src }
}

// ------------------------------------------------------------------ tests

#[test]
fn roundtrip_every_mode_preserves_everything() {
    let e = env();
    for mode in ["fast", "balanced", "max"] {
        let a = e.root.join(format!("{mode}.snug"));
        let out = e.root.join(format!("out-{mode}"));
        ok(snugly(&["c", p(&a), p(&e.src), "-m", mode]));
        ok(snugly(&["x", p(&a), p(&out)]));
        assert_same(&e.src, &out);
        ok(snugly(&["t", p(&a)]));
    }
}

#[test]
fn recompression_is_used_and_lossless() {
    let e = env();
    let a = e.root.join("a.snug");
    ok(snugly(&["c", p(&a), p(&e.src)]));
    let list = String::from_utf8(ok(snugly(&["l", p(&a)])).stdout).unwrap();
    for (file, tag) in [("log.txt.gz", "[gz]"), ("photo.jpg", "[jpg]"), ("image.png", "[zip/png]"), ("report.docx", "[zip/png]")] {
        let line = list.lines().find(|l| l.ends_with(&format!("{file} {tag}"))).unwrap_or_else(|| panic!("{file} not recompressed:\n{list}"));
        assert!(!line.is_empty());
    }
    let out = e.root.join("out");
    ok(snugly(&["x", p(&a), p(&out)]));
    assert_same(&e.src, &out);
    // and with recompression off, everything still round-trips
    let b = e.root.join("b.snug");
    ok(snugly(&["c", p(&b), p(&e.src), "--no-recomp"]));
    assert!(fs::metadata(&a).unwrap().len() < fs::metadata(&b).unwrap().len(), "recompression should make the archive smaller");
}

#[test]
fn extract_single_file_and_folder() {
    let e = env();
    let a = e.root.join("a.snug");
    ok(snugly(&["c", p(&a), p(&e.src)]));
    let out = e.root.join("one");
    ok(snugly(&["x", p(&a), p(&out), "--only", "docs/deep/er/copy.txt"]));
    assert_eq!(fs::read(out.join("docs/deep/er/copy.txt")).unwrap(), fs::read(e.src.join("docs/deep/er/copy.txt")).unwrap());
    assert!(!out.join("random.bin").exists());
    let out2 = e.root.join("folder");
    ok(snugly(&["x", p(&a), p(&out2), "--only", "docs"]));
    assert!(out2.join("docs/notes.txt").exists() && out2.join("docs/deep/er/copy.txt").exists());
    assert!(!out2.join("photo.jpg").exists());
}

#[test]
fn refuses_to_overwrite_without_update() {
    let e = env();
    let a = e.root.join("a.snug");
    ok(snugly(&["c", p(&a), p(&e.src)]));
    assert!(fails(&snugly(&["c", p(&a), p(&e.src)])).contains("--update"));
}

#[test]
fn encryption_hides_everything_and_rejects_wrong_password_and_tampering() {
    let e = env();
    let a = e.root.join("enc.snug");
    ok(snugly_pw(PW, &["c", p(&a), p(&e.src), "-e"]));
    let bytes = fs::read(&a).unwrap();
    for needle in [&b"notes.txt"[..], b"quick brown fox", b"\x28\xb5\x2f\xfd"] {
        assert!(!bytes.windows(needle.len()).any(|w| w == needle), "plaintext visible: {:?}", String::from_utf8_lossy(needle));
    }
    let out = e.root.join("out");
    ok(snugly_pw(PW, &["x", p(&a), p(&out)]));
    assert_same(&e.src, &out);

    let bad = e.root.join("bad");
    fails(&snugly_pw("wrong password", &["x", p(&a), p(&bad)]));
    assert!(!bad.join("docs").exists(), "nothing may be written with a wrong password");

    let mut t = bytes.clone();
    let mid = t.len() / 2;
    t[mid] ^= 1;
    let tampered = e.root.join("tampered.snug");
    fs::write(&tampered, t).unwrap();
    fails(&snugly_pw(PW, &["t", p(&tampered)]));
}

#[test]
fn snapshots_store_only_changes_and_restore_each_state() {
    let e = env();
    let a = e.root.join("snap.snug");
    let state1 = e.root.join("state1");
    ok(snugly(&["c", p(&a), p(&e.src), "--label", "one"]));
    ok(snugly(&["x", p(&a), p(&state1)]));
    let after1 = fs::metadata(&a).unwrap().len();

    fs::write(e.src.join("docs/notes.txt"), "changed\n").unwrap();
    fs::remove_file(e.src.join("random.bin")).unwrap();
    fs::rename(e.src.join("docs/deep"), e.src.join("docs/moved")).unwrap();
    fs::write(e.src.join("new.txt"), "new file\n").unwrap();
    ok(snugly(&["c", p(&a), p(&e.src), "-u", "--label", "two"]));
    let after2 = fs::metadata(&a).unwrap().len();
    assert!(after2 - after1 < after1 / 4, "an update with small changes should add little ({after1} -> {after2})");

    let s = String::from_utf8(ok(snugly(&["s", p(&a)])).stdout).unwrap();
    assert!(s.contains(" one") && s.contains(" two"), "{s}");
    let r1 = e.root.join("r1");
    let r2 = e.root.join("r2");
    ok(snugly(&["x", p(&a), p(&r1), "-s", "1"]));
    ok(snugly(&["x", p(&a), p(&r2)]));
    assert_same(&state1, &r1);
    assert_same(&e.src, &r2);
    ok(snugly(&["t", p(&a)]));

    ok(snugly(&["c", p(&a), p(&e.src), "-u"]));
    assert!(fs::metadata(&a).unwrap().len() - after2 < 64 * 1024, "an update with no changes should add only an index");
}

#[test]
fn interrupted_update_falls_back_and_recovers() {
    let e = env();
    let a = e.root.join("a.snug");
    ok(snugly(&["c", p(&a), p(&e.src)]));
    let good = fs::read(&a).unwrap();
    // simulate a crash mid-update: partial garbage after the last footer
    let mut f = fs::OpenOptions::new().append(true).open(&a).unwrap();
    f.write_all(&noise(9, 500_000)).unwrap();
    drop(f);
    let out = e.root.join("out");
    let o = ok(snugly(&["x", p(&a), p(&out)]));
    assert!(String::from_utf8_lossy(&o.stderr).contains("interrupted"));
    assert_same(&e.src, &out);
    ok(snugly(&["c", p(&a), p(&e.src), "-u"])); // reclaims the garbage
    assert!(fs::read(&a).unwrap().starts_with(&good));
    ok(snugly(&["t", p(&a)]));
}

#[test]
fn recovery_data_repairs_damage() {
    let e = env();
    let a = e.root.join("a.snug");
    ok(snugly(&["c", p(&a), p(&e.src), "-r", "50"]));
    let good = fs::read(&a).unwrap();
    let mut bad = good.clone();
    for off in [100, bad.len() / 3, bad.len() - 10] {
        bad[off] ^= 0xff;
    }
    fs::write(&a, &bad).unwrap();
    fails(&snugly(&["t", p(&a)]));
    ok(snugly(&["repair", p(&a)]));
    assert_eq!(fs::read(&a).unwrap(), good, "repair must restore the exact bytes");
    ok(snugly(&["t", p(&a)]));
    let o = ok(snugly(&["repair", p(&a), "--check"]));
    assert!(String::from_utf8_lossy(&o.stderr).contains("no damage"));
}

#[test]
fn hostile_paths_are_rejected() {
    use snugly::format::*;
    let dir = tempfile::tempdir().unwrap();
    let payload = b"pwned\n".to_vec();
    for bad in ["../escape.txt", "/tmp/abs.txt", "a/../../b.txt", "C:/x.txt", "a//b"] {
        let mut file = Header { kdf: None }.encode();
        let off = file.len() as u64;
        file.extend_from_slice(&payload);
        let entry = Entry {
            path: bad.into(),
            kind: E_FILE,
            meta: Meta { mode: 0o644, ..Default::default() },
            size: payload.len() as u64,
            hash: *blake3::hash(&payload).as_bytes(),
            transform: 0,
            chunks: vec![0],
            target: String::new(),
        };
        let idx = Index {
            blocks: vec![BlockEntry { offset: off, comp_len: payload.len() as u64, raw_len: payload.len() as u64, codec: CODEC_STORE }],
            chunks: vec![ChunkEntry { block: 0, offset: 0, len: payload.len() as u64, hash: *blake3::hash(&payload).as_bytes() }],
            snapshots: vec![Snapshot { time_s: 0, label: "evil".into(), first_block: 0, entries: vec![entry] }],
        };
        let raw = idx.encode();
        let comp = zstd::bulk::compress(&raw, 3).unwrap();
        let ioff = file.len() as u64;
        file.extend_from_slice(&comp);
        file.extend_from_slice(&ioff.to_le_bytes());
        file.extend_from_slice(&(comp.len() as u64).to_le_bytes());
        file.extend_from_slice(&(raw.len() as u64).to_le_bytes());
        file.extend_from_slice(END_MAGIC);
        let a = dir.path().join("evil.snug");
        fs::write(&a, &file).unwrap();
        let out = dir.path().join("out/inner");
        let err = fails(&snugly(&["x", p(&a), p(&out)]));
        assert!(err.contains("unsafe path"), "{bad}: {err}");
    }
    assert!(!dir.path().join("escape.txt").exists() && !dir.path().join("out/escape.txt").exists());
}

#[test]
fn corrupted_archives_fail_cleanly_never_panic() {
    let e = env();
    let a = e.root.join("a.snug");
    ok(snugly(&["c", p(&a), p(&e.src)]));
    let good = fs::read(&a).unwrap();
    let c = e.root.join("c.snug");
    let rnd = noise(42, 4096);
    for i in 0..120 {
        let mut bad = good.clone();
        match i % 4 {
            0 => bad[(rnd[i] as usize * 7919 + i * 104_729) % good.len()] ^= 1 << (i % 8),
            1 => bad.truncate(good.len() * (i % 97 + 1) / 100),
            2 => {
                let s = (i * 7_777) % (good.len() - 64);
                bad[s..s + 64].copy_from_slice(&rnd[..64]);
            }
            _ => {
                let s = good.len() - 1 - (i * 13) % 600; // hammer the index/footer
                bad[s] = rnd[i];
            }
        }
        fs::write(&c, &bad).unwrap();
        let o = snugly(&["t", p(&c)]);
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(!err.contains("panicked") && o.status.code() != Some(101), "case {i} panicked:\n{err}");
    }
}
