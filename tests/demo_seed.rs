//! Demo corpus seeder — moved out of the shipped binary (no `trove demo-seed`
//! cruft in production; Chris, 2026-05-28). This is an **ignored** test: run it
//! by hand to plant a fixed, reproducible corpus into the live DB for playing
//! with `trove search` or grabbing essay screenshots:
//!
//!   source ~/.secret_env   # OPENAI_API_KEY + the supabase stack up
//!   cargo test --features mount --test demo_seed -- --ignored --nocapture
//!
//! It uses the real embedding path (record_meta + embed_content), so the seeded
//! store is indistinguishable from one written through the mount — just without
//! needing libjfs. Re-running is safe (content-addressed blobs dedup; chunks are
//! replaced idempotently). The fast suites seed their own deterministic vectors,
//! so nothing here runs by default (no OpenAI cost on a normal `cargo test`).
#![cfg(feature = "mount")]

use trove::embed::embed_content;
use trove::version::{sha256_hex, VersionStore};

/// Single-topic docs with headed sections, topics far apart so search
/// disambiguates cleanly.
const DOCS: &[(&str, &str)] = &[
    ("/demo/sourdough.md", "---\ntype: note\n---\n# Sourdough\n\n## Starter\nKeep a rye starter at room temperature and feed it equal weights of flour and water once a day until it doubles reliably.\n\n## Proving\nA long cold prove in the fridge overnight develops sour flavour and an open crumb; bake into a preheated cast-iron pot for oven spring.\n"),
    ("/demo/postgres-indexes.md", "---\ntype: note\n---\n# Postgres Indexes\n\n## B-tree vs HNSW\nA B-tree index serves equality and range queries on scalar columns; for high-dimensional vector similarity you want an approximate index like HNSW from pgvector.\n\n## When to add one\nIndexes speed reads but slow writes and cost disk — add them for the queries you actually run hot, not speculatively.\n"),
    ("/demo/running-injuries.md", "---\ntype: note\n---\n# Running Injuries\n\n## Knee pain\nRunner's knee usually comes from ramping mileage too fast; back off volume, strengthen the glutes, and check your cadence is high enough to avoid overstriding.\n\n## Recovery\nRest, ice, and a gradual return-to-run plan beat pushing through pain, which turns a niggle into months out.\n"),
    ("/demo/jazz-history.md", "---\ntype: note\n---\n# Jazz History\n\n## Bebop\nIn the 1940s Charlie Parker and Dizzy Gillespie sped tempos and stacked complex harmony, turning jazz from dance music into a music for listening.\n\n## Modal jazz\nMiles Davis's Kind of Blue traded fast chord changes for slow-moving modes, giving soloists space to explore.\n"),
    ("/demo/rust-ownership.md", "---\ntype: note\n---\n# Rust Ownership\n\n## Borrowing\nEach value has a single owner; you can lend out shared references or one mutable reference, and the borrow checker proves no data races at compile time.\n\n## Lifetimes\nLifetimes annotate how long a reference is valid so the compiler can reject dangling pointers without a garbage collector.\n"),
];

fn db_url() -> String {
    std::env::var("TROVE_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54322/postgres".to_string())
}

#[test]
#[ignore = "seeds the live DB with real OpenAI embeddings — run on demand"]
fn seed_live_demo_corpus() {
    let api_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY (source ~/.secret_env)");
    let mut vs = VersionStore::connect(&db_url(), None).expect("version DB up? (`supabase start`)");
    for (path, content) in DOCS {
        let bytes = content.as_bytes();
        let hash = sha256_hex(bytes);
        vs.record_meta(path, &hash, bytes.len() as i64, Some("demo")).unwrap();
        embed_content(&mut vs, &api_key, &hash, bytes).unwrap();
        println!("seeded {path}");
    }
    println!("seeded {} demo docs under /demo/", DOCS.len());
}
