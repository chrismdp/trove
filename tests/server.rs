//! Tests for `trove server`'s routing against a real volume + the version DB.
//! Drives `server::route` directly (pure over the URL — no socket), so /files
//! and /file are deterministic with no OpenAI. The semantic /search path is
//! covered by tests/search.rs (search_chunks) + manual smoke; here we only
//! assert /search with no query is a clean 400.
#![cfg(feature = "mount")]

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use trove::commands::server::route;
use trove::jfs::Fs;
use trove::version::VersionStore;
use trove::versioning::record_version;

fn juicefs_bin() -> String {
    std::env::var("JUICEFS_BIN")
        .unwrap_or_else(|_| "/home/cp/code/trove/spike/juicefs/juicefs".to_string())
}
fn db_url() -> String {
    std::env::var("TROVE_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54322/postgres".to_string())
}
fn uniq() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    )
}
fn fresh_fs() -> Fs {
    let dir = std::env::temp_dir().join(format!("trove-srv-{}", uniq()));
    std::fs::create_dir_all(dir.join("store")).unwrap();
    let name = format!("vol{}", uniq().replace('-', ""));
    let meta = format!("sqlite3://{}/meta.db", dir.display());
    let out = Command::new(juicefs_bin())
        .args(["format", "--storage", "file", "--bucket", &format!("{}/store/", dir.display()), &meta, &name])
        .output()
        .expect("run juicefs format");
    assert!(out.status.success(), "format failed: {}", String::from_utf8_lossy(&out.stderr));
    Fs::init(&name, &meta, &format!("{}/cache", dir.display())).unwrap()
}

#[test]
fn serves_index_files_and_file_content() {
    let fs = fresh_fs();
    let mut vs = VersionStore::connect(&db_url(), None).expect("version DB up? (`supabase start`)");

    // A file in the live tree + a version row (so /files lists it).
    let path = format!("/srv-{}.md", uniq());
    let body = "# Served\nhello from trove server\n";
    fs.write_all(&path, body.as_bytes(), 0o644).unwrap();
    record_version(&fs, &mut vs, &path, body.as_bytes(), None).unwrap();

    // / — HTML index.
    let (status, ct, html) = route(&fs, &mut vs, "", "/");
    assert_eq!(status, 200);
    assert!(ct.starts_with("text/html"));
    assert!(String::from_utf8_lossy(&html).contains("a filesystem that talks back"));

    // /files — JSON containing our path.
    let (status, ct, files) = route(&fs, &mut vs, "", "/files");
    assert_eq!(status, 200);
    assert_eq!(ct, "application/json");
    assert!(String::from_utf8_lossy(&files).contains(&path), "files JSON should list the path");

    // /file/<path> — the live bytes.
    let (status, _ct, content) = route(&fs, &mut vs, "", &format!("/file{path}"));
    assert_eq!(status, 200);
    assert_eq!(content, body.as_bytes());

    // /file of a missing path — 404.
    let (status, _, _) = route(&fs, &mut vs, "", "/file/does/not/exist.md");
    assert_eq!(status, 404);

    // /search with no query — 400, no OpenAI call.
    let (status, _, _) = route(&fs, &mut vs, "", "/search");
    assert_eq!(status, 400);

    // unknown route — 404.
    let (status, _, _) = route(&fs, &mut vs, "", "/nope");
    assert_eq!(status, 404);
}
