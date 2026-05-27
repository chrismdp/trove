//! Tests for the version chain (PG metadata). These exercise the real schema
//! against a running Trove version DB — the local Supabase stack (`supabase
//! start`). Override with TROVE_DB_URL. Each test isolates itself with a unique
//! path prefix, since the DB persists across runs. Blob *bytes* live in R2 and
//! are covered by the recorder/blobstore tests; here we only assert metadata.

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

/// Record a content blob's metadata, content-addressing it the way the recorder
/// does (hash + size from the bytes).
fn record(s: &mut VersionStore, path: &str, content: &[u8], author: Option<&str>) -> i32 {
    s.record_meta(path, &sha256_hex(content), content.len() as i64, author)
        .unwrap()
}

#[test]
fn record_builds_a_monotonic_chain_with_parents() {
    let mut s = store();
    let path = unique_path("chain");

    assert_eq!(record(&mut s, &path, b"one", Some("agent-a")), 1);
    assert_eq!(record(&mut s, &path, b"two", Some("agent-a")), 2);
    assert_eq!(record(&mut s, &path, b"three", None), 3);

    let log = s.log(&path).unwrap();
    assert_eq!(log.iter().map(|v| v.rev).collect::<Vec<_>>(), vec![3, 2, 1]);
    assert_eq!(
        log.iter().map(|v| v.parent_rev).collect::<Vec<_>>(),
        vec![Some(2), Some(1), None]
    );
    assert_eq!(log[0].size, 5); // "three"
    assert_eq!(log[2].author.as_deref(), Some("agent-a"));
    assert_eq!(log[0].author, None);
    assert_eq!(log[2].blob_hash, sha256_hex(b"one"));
}

#[test]
fn identical_content_dedups_the_blob() {
    let mut s = store();
    let a = unique_path("dedup-a");
    let b = unique_path("dedup-b");
    let content = b"identical bytes under two different paths";

    record(&mut s, &a, content, None);
    record(&mut s, &b, content, None);

    // Both versions point at the same content-addressed blob.
    let (la, lb) = (s.log(&a).unwrap(), s.log(&b).unwrap());
    assert_eq!(la[0].blob_hash, sha256_hex(content));
    assert_eq!(la[0].blob_hash, lb[0].blob_hash);
}

#[test]
fn blob_hash_at_resolves_each_revision() {
    let mut s = store();
    let path = unique_path("hashat");
    record(&mut s, &path, b"first", None);
    record(&mut s, &path, b"second", None);

    assert_eq!(s.blob_hash_at(&path, 1).unwrap().as_deref(), Some(&*sha256_hex(b"first")));
    assert_eq!(s.blob_hash_at(&path, 2).unwrap().as_deref(), Some(&*sha256_hex(b"second")));
    assert_eq!(s.blob_hash_at(&path, 99).unwrap(), None);
}

#[test]
fn pending_embedding_hashes_surfaces_unembedded_blobs() {
    let mut s = store();
    let path = unique_path("embed");
    // Unique content so this blob's hash is distinguishable in the shared DB.
    let content = format!("embed-me-{}", unique_path("c")).into_bytes();
    let hash = sha256_hex(&content);
    record(&mut s, &path, &content, None);

    let pending = s.pending_embedding_hashes(10_000).unwrap();
    assert!(pending.contains(&hash), "new blob should be pending embedding");
}

#[test]
fn log_of_unknown_path_is_empty() {
    let mut s = store();
    assert!(s.log(&unique_path("never-written")).unwrap().is_empty());
}
