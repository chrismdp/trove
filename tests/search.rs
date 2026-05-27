//! Tests for `trove search`'s ranking core (`VersionStore::search_chunks`).
//!
//! These exercise the real pgvector query against a running Trove version DB
//! (local Supabase stack, `supabase start`; override with TROVE_DB_URL). They
//! need NO OpenAI and NO libjfs: we hand-craft embedding vectors and assert the
//! SQL ranks nearest-first and resolves each chunk back to its file. The
//! query-embedding leg (OpenAI) is covered separately by the embed e2e.
//!
//! The DB persists across runs, so every test uses unique paths/content and
//! near-basis crafted vectors, which dominate the dense real embeddings that may
//! also be present for any query that concentrates its mass in one dimension.

use std::sync::atomic::{AtomicU64, Ordering};
use trove::version::{sha256_hex, ChunkInsert, VersionStore};

const MODEL: &str = "text-embedding-3-large";
const DIMS: usize = 3072;

fn db_url() -> String {
    std::env::var("TROVE_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54322/postgres".to_string())
}

fn store() -> VersionStore {
    VersionStore::connect(&db_url())
        .expect("connect to Trove version DB — is the local supabase stack up? (`supabase start`)")
}

fn uniq() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}-{n}-{nanos}", std::process::id())
}

/// A 3072-dim pgvector literal with `lead` as its first components, zeros after.
fn vec_literal(lead: &[f32]) -> String {
    let mut v = vec![0f32; DIMS];
    v[..lead.len()].copy_from_slice(lead);
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

/// Register a blob + a v1 file_versions row for `path`, then attach one chunk
/// embedded at `embedding`. Returns the blob hash.
fn seed(s: &mut VersionStore, path: &str, content: &str, heading: &str, embedding: &str) -> String {
    let hash = sha256_hex(content.as_bytes());
    s.record_meta(path, &hash, content.len() as i64, None).unwrap();
    s.replace_chunks(
        &hash,
        MODEL,
        &[ChunkInsert {
            ordinal: 0,
            heading: Some(heading),
            start_byte: 0,
            end_byte: content.len() as i32,
            embedding: Some(embedding.to_string()),
        }],
    )
    .unwrap();
    hash
}

#[test]
fn ranks_nearest_chunk_first_and_resolves_the_path() {
    let mut s = store();
    let tag = uniq();
    let path_near = format!("/test/search-near-{tag}.md");
    let path_far = format!("/test/search-far-{tag}.md");

    // Two near-orthogonal chunks: NEAR points along dim 0, FAR along dim 1.
    seed(&mut s, &path_near, &format!("cats {tag}"), "Cats", &vec_literal(&[1.0, 0.0]));
    seed(&mut s, &path_far, &format!("boats {tag}"), "Boats", &vec_literal(&[0.0, 1.0]));

    // Query mostly along dim 0 -> NEAR should win.
    let hits = s.search_chunks(&vec_literal(&[0.9, 0.1]), 20).unwrap();

    let pos = |p: &str| hits.iter().position(|h| h.path == p);
    let (np, fp) = (pos(&path_near), pos(&path_far));
    assert!(np.is_some(), "near file should appear in results");
    assert!(fp.is_some(), "far file should appear in results");
    assert!(np < fp, "the chunk nearer the query must rank first");

    // The hit carries the resolving file's heading + a sane similarity.
    let near = &hits[np.unwrap()];
    assert_eq!(near.heading.as_deref(), Some("Cats"));
    let near_sim = 1.0 - near.distance;
    let far_sim = 1.0 - hits[fp.unwrap()].distance;
    assert!(near_sim > far_sim, "nearer chunk has higher cosine similarity");
    assert!(near_sim > 0.9, "query aligned with NEAR should score high, got {near_sim}");
}

#[test]
fn skips_unembedded_sentinel_chunks() {
    let mut s = store();
    let tag = uniq();
    let path = format!("/test/search-sentinel-{tag}.md");
    let content = format!("binary blob {tag}");
    let hash = sha256_hex(content.as_bytes());
    s.record_meta(&path, &hash, content.len() as i64, None).unwrap();
    // A null-embedding sentinel (what binary/blank blobs get).
    s.replace_chunks(
        &hash,
        MODEL,
        &[ChunkInsert { ordinal: 0, heading: None, start_byte: 0, end_byte: 0, embedding: None }],
    )
    .unwrap();

    let hits = s.search_chunks(&vec_literal(&[1.0]), 1000).unwrap();
    assert!(
        !hits.iter().any(|h| h.path == path),
        "a sentinel chunk (null embedding) must never appear in search results"
    );
}
