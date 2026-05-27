//! e2e tests for the libjfs wrapper. Each test formats a real JuiceFS volume
//! (SQLite metadata + local-file object store — fast, isolated; R2 is proven
//! separately) and drives it through the safe wrapper. Requires the built
//! `libjfs` + the `juicefs` binary; run with `--features mount`.
#![cfg(feature = "mount")]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use trove::jfs::Fs;

/// Path to the `juicefs` binary (built in the spike); override with JUICEFS_BIN.
fn juicefs_bin() -> String {
    std::env::var("JUICEFS_BIN")
        .unwrap_or_else(|_| "/home/cp/code/trove/spike/juicefs/juicefs".to_string())
}

/// A freshly formatted throwaway volume: unique dir, SQLite meta, file store.
struct TestVol {
    dir: PathBuf,
    name: String,
    meta: String,
}

impl TestVol {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let uniq = format!(
            "{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(format!("trove-jfs-{tag}-{uniq}"));
        std::fs::create_dir_all(dir.join("store")).unwrap();
        let name = format!("vol{}", uniq.replace('-', ""));
        let meta = format!("sqlite3://{}/meta.db", dir.display());

        let out = Command::new(juicefs_bin())
            .args([
                "format",
                "--storage",
                "file",
                "--bucket",
                &format!("{}/store/", dir.display()),
                &meta,
                &name,
            ])
            .output()
            .expect("run juicefs format");
        assert!(
            out.status.success(),
            "juicefs format failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        TestVol { dir, name, meta }
    }

    fn open(&self) -> Fs {
        Fs::init(&self.name, &self.meta, &format!("{}/cache", self.dir.display())).unwrap()
    }
}

impl Drop for TestVol {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn init_opens_a_formatted_volume() {
    let v = TestVol::new("init");
    let _fs = v.open(); // panics if jfs_init fails
}

#[test]
fn write_read_roundtrip_through_storage() {
    let v = TestVol::new("roundtrip");
    let fs = v.open();
    let payload = b"trove jfs wrapper: write -> storage -> read";

    let f = fs.create("/note.md", 0o644).unwrap();
    assert_eq!(f.write_at(payload, 0).unwrap(), payload.len());
    f.fsync().unwrap();
    drop(f); // close

    let f = fs.open("/note.md", 0).unwrap();
    let mut buf = vec![0u8; payload.len()];
    assert_eq!(f.read_at(&mut buf, 0).unwrap(), payload.len());
    assert_eq!(&buf, payload);
}

#[test]
fn mkdir_then_create_inside_it() {
    let v = TestVol::new("mkdir");
    let fs = v.open();
    fs.mkdir("/people", 0o755).unwrap();
    let f = fs.create("/people/rebekah.md", 0o644).unwrap();
    f.write_at(b"---\ntype: person\n---\n", 0).unwrap();
    drop(f);
    let st = fs.stat("/people").unwrap();
    assert!(st.is_dir(), "/people should be a directory");
}

#[test]
fn stat_reports_length() {
    let v = TestVol::new("stat");
    let fs = v.open();
    let body = b"0123456789";
    let f = fs.create("/len.txt", 0o644).unwrap();
    f.write_at(body, 0).unwrap();
    f.fsync().unwrap();
    drop(f);
    let st = fs.stat("/len.txt").unwrap();
    assert_eq!(st.length, body.len() as u64);
    assert!(!st.is_dir());
}

#[test]
fn unlink_removes_a_file() {
    let v = TestVol::new("unlink");
    let fs = v.open();
    let f = fs.create("/tmp.txt", 0o644).unwrap();
    f.write_at(b"bye", 0).unwrap();
    drop(f);
    fs.stat("/tmp.txt").unwrap(); // exists
    fs.unlink("/tmp.txt").unwrap();
    assert!(fs.stat("/tmp.txt").is_err(), "stat should fail after unlink");
}

#[test]
fn open_missing_file_errors() {
    let v = TestVol::new("missing");
    let fs = v.open();
    assert!(fs.open("/does-not-exist.md", 0).is_err());
}

#[test]
fn rename_moves_a_file() {
    let v = TestVol::new("rename");
    let fs = v.open();
    let f = fs.create("/from.md", 0o644).unwrap();
    f.write_at(b"movable", 0).unwrap();
    f.fsync().unwrap();
    drop(f);
    fs.rename("/from.md", "/to.md").unwrap();
    assert!(fs.stat("/from.md").is_err(), "source gone after rename");
    assert_eq!(fs.stat("/to.md").unwrap().length, 7);
}

#[test]
fn readdir_lists_entries_with_types() {
    let v = TestVol::new("readdir");
    let fs = v.open();
    fs.mkdir("/people", 0o755).unwrap();
    let f = fs.create("/a.md", 0o644).unwrap();
    f.write_at(b"x", 0).unwrap();
    drop(f);

    let mut names: Vec<(String, bool)> = fs
        .readdir("/")
        .unwrap()
        .into_iter()
        .map(|d| (d.name.clone(), d.is_dir()))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![("a.md".to_string(), false), ("people".to_string(), true)]
    );
}
