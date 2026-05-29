//! Tests for the version chain (PG metadata). These exercise the real schema
//! against a running Trove version DB — the local Supabase stack (`supabase
//! start`). Override with TROVE_DB_URL. Each test isolates itself with a unique
//! path prefix, since the DB persists across runs. Blob *bytes* live in R2 and
//! are covered by the recorder/blobstore tests; here we only assert metadata.

use std::sync::atomic::{AtomicU64, Ordering};
use trove::version::{sha256_hex, ChunkInsert, VersionStore};

fn db_url() -> String {
    std::env::var("TROVE_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54322/postgres".to_string())
}

fn store() -> VersionStore {
    VersionStore::connect(&db_url(), None)
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
fn stale_model_hashes_surfaces_blobs_on_old_models_only() {
    let mut s = store();
    // Embed two distinct blobs with distinct models. Sentinel rows (embedding =
    // None) are fine here — the query keys off `embedding_model`, not the vector.
    let current_model = "text-embedding-3-large";
    let old_model = "text-embedding-2-tiny";

    let path_current = unique_path("remodel-current");
    let content_current = format!("current-{}", unique_path("c")).into_bytes();
    let hash_current = sha256_hex(&content_current);
    record(&mut s, &path_current, &content_current, None);
    s.replace_chunks(
        &hash_current,
        current_model,
        &[ChunkInsert { ordinal: 0, heading: None, start_byte: 0, end_byte: content_current.len() as i32, embedding: None }],
    )
    .unwrap();

    let path_stale = unique_path("remodel-stale");
    let content_stale = format!("stale-{}", unique_path("c")).into_bytes();
    let hash_stale = sha256_hex(&content_stale);
    record(&mut s, &path_stale, &content_stale, None);
    s.replace_chunks(
        &hash_stale,
        old_model,
        &[ChunkInsert { ordinal: 0, heading: None, start_byte: 0, end_byte: content_stale.len() as i32, embedding: None }],
    )
    .unwrap();

    let stale = s.stale_model_hashes(current_model, 100_000).unwrap();
    assert!(stale.contains(&hash_stale), "blob embedded with an older model should be flagged stale");
    assert!(!stale.contains(&hash_current), "blob on the current model is not stale");
}

#[test]
fn usage_reports_real_figures_and_growth() {
    // The integration DB is shared with the rest of the suite, which runs in
    // parallel — so we can't make exact delta assertions on counts (a
    // concurrent test will move them). What we CAN assert: a snapshot is
    // self-consistent, sizes are non-negative, and after adding our own
    // content the totals strictly grew. The growth check is robust even when
    // other tests are writing alongside, because every concurrent write
    // also grows the totals (counts/sizes are monotonic in this suite).
    let mut s = store();
    let before = s.usage().unwrap();

    // Add some recognisable content of our own.
    let path_a = unique_path("usage-a");
    let path_b = unique_path("usage-b");
    let content_1 = format!("usage-1-{}", unique_path("c")).into_bytes();
    let content_2 = format!("usage-2-{}", unique_path("c")).into_bytes();
    let hash_1 = sha256_hex(&content_1);
    record(&mut s, &path_a, &content_1, None);
    record(&mut s, &path_a, &content_2, None);
    record(&mut s, &path_b, &content_1, None); // dedups: same hash as content_1

    s.replace_chunks(
        &hash_1,
        "text-embedding-3-large",
        &[ChunkInsert {
            ordinal: 0,
            heading: None,
            start_byte: 0,
            end_byte: content_1.len() as i32,
            embedding: None,
        }],
    )
    .unwrap();

    let after = s.usage().unwrap();

    // Snapshot is internally consistent.
    assert_eq!(after.embedded_blobs + after.pending_blobs, after.blobs_rows);
    assert!(after.distinct_paths <= after.file_versions_rows);

    // Sizes are non-negative.
    assert!(after.database_bytes >= 0);
    assert!(after.blobs_bytes >= 0);
    assert!(after.file_versions_bytes >= 0);
    assert!(after.blob_chunks_bytes >= 0);

    // We added 2 distinct blobs, 3 version rows, 2 distinct paths, and
    // embedded 1 blob — so each must have grown by at least that much.
    assert!(after.blobs_rows - before.blobs_rows >= 2);
    assert!(after.file_versions_rows - before.file_versions_rows >= 3);
    assert!(after.distinct_paths - before.distinct_paths >= 2);
    assert!(after.embedded_blobs > before.embedded_blobs);
    assert!(after.blob_chunks_rows > before.blob_chunks_rows);
}

#[test]
fn log_of_unknown_path_is_empty() {
    let mut s = store();
    assert!(s.log(&unique_path("never-written")).unwrap().is_empty());
}
