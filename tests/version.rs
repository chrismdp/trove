//! Tests for the version chain. These exercise the real schema against a
//! running Trove version DB — the local Supabase stack (`supabase start` in
//! the repo). Override the connection with TROVE_DB_URL. Each test isolates
//! itself with a unique path prefix, since the DB persists across runs.

use std::sync::atomic::{AtomicU64, Ordering};
use trove::version::{sha256_hex, VersionStore};

fn db_url() -> String {
    std::env::var("TROVE_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54322/postgres".to_string())
}

fn store() -> VersionStore {
    VersionStore::connect(&db_url())
        .expect("connect to Trove version DB — is the local supabase stack up? (`supabase start`)")
}

/// A path nobody else in this run will touch.
fn unique_path(tag: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("/test/{tag}-{}-{n}-{nanos}.md", std::process::id())
}

#[test]
fn record_builds_a_monotonic_chain_with_parents() {
    let mut s = store();
    let path = unique_path("chain");

    assert_eq!(s.record(&path, b"one", Some("agent-a")).unwrap(), 1);
    assert_eq!(s.record(&path, b"two", Some("agent-a")).unwrap(), 2);
    assert_eq!(s.record(&path, b"three", None).unwrap(), 3);

    let log = s.log(&path).unwrap();
    // Newest first.
    let revs: Vec<i32> = log.iter().map(|v| v.rev).collect();
    assert_eq!(revs, vec![3, 2, 1]);
    let parents: Vec<Option<i32>> = log.iter().map(|v| v.parent_rev).collect();
    assert_eq!(parents, vec![Some(2), Some(1), None]);
    // Sizes and author flow through.
    assert_eq!(log[0].size, 5); // "three"
    assert_eq!(log[2].author.as_deref(), Some("agent-a"));
    assert_eq!(log[0].author, None);
    // Blob hash is the content address.
    assert_eq!(log[2].blob_hash, sha256_hex(b"one"));
}

#[test]
fn identical_content_dedups_the_blob() {
    let mut s = store();
    let a = unique_path("dedup-a");
    let b = unique_path("dedup-b");
    let content = b"identical bytes under two different paths";

    s.record(&a, content, None).unwrap();
    s.record(&b, content, None).unwrap();

    // Both versions point at the same content-addressed blob.
    let la = s.log(&a).unwrap();
    let lb = s.log(&b).unwrap();
    assert_eq!(la[0].blob_hash, sha256_hex(content));
    assert_eq!(la[0].blob_hash, lb[0].blob_hash);
}

#[test]
fn log_of_unknown_path_is_empty() {
    let mut s = store();
    assert!(s.log(&unique_path("never-written")).unwrap().is_empty());
}
