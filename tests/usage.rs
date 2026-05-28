//! Integration test for `trove usage` against a live Trove version DB +
//! a freshly formatted JuiceFS volume (SQLite meta + local file store, the
//! same pattern `tests/jfs.rs` uses). Requires the local Supabase stack
//! (`supabase start`) for the DB half, and a built libjfs + `juicefs`
//! binary for the volume half.
#![cfg(feature = "mount")]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use trove::commands::usage;
use trove::jfs::Fs;
use trove::version::VersionStore;

fn db_url() -> String {
    std::env::var("TROVE_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54322/postgres".to_string())
}

fn juicefs_bin() -> String {
    std::env::var("JUICEFS_BIN")
        .unwrap_or_else(|_| "/home/cp/code/trove/spike/juicefs/juicefs".to_string())
}

/// Freshly formatted throwaway volume. Mirrors `tests/jfs.rs::TestVol`.
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
        let dir = std::env::temp_dir().join(format!("trove-usage-{tag}-{uniq}"));
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
fn usage_report_is_producible_and_self_consistent() {
    let Ok(mut versions) = VersionStore::connect(&db_url()) else {
        eprintln!("skipping: no DB reachable at {}", db_url());
        return;
    };
    let v = TestVol::new("usage");
    let fs = v.open();

    let report = usage::run(&fs, &mut versions).expect("usage::run");

    // No negative byte counts come back from Postgres / statvfs.
    assert!(report.db.database_bytes >= 0);
    assert!(report.db.blobs_bytes >= 0);
    assert!(report.db.file_versions_bytes >= 0);
    assert!(report.db.blob_chunks_bytes >= 0);

    // statvfs sanity: total >= available.
    assert!(
        report.volume_total_bytes >= report.volume_available_bytes,
        "volume_total ({}) should be >= volume_available ({})",
        report.volume_total_bytes,
        report.volume_available_bytes
    );

    // If the shared integration DB has any embedded chunks at all, the
    // embedded-blob count must be positive (the two queries should agree).
    if report.db.blob_chunks_rows > 0 {
        assert!(
            report.db.embedded_blobs > 0,
            "blob_chunks has rows ({}) but embedded_blobs is 0",
            report.db.blob_chunks_rows
        );
    }

    // print() must not panic on a real snapshot — exercise it once.
    usage::print(&report);
}
