//! `trove demo-seed` — plant a small, fixed corpus so `trove search` returns
//! clean, reproducible results (for demos, screenshots, the essay).
//!
//! Deliberately uses the *real* embedding path (`record_meta` + `embed_content`)
//! so a seeded store is indistinguishable from one written through the mount —
//! just without needing libjfs or a FUSE mount. Re-running is safe: the blob is
//! content-addressed (dedup) and `embed_content` replaces chunks idempotently;
//! a re-run only appends a new (identical-content) revision to each demo path.

use crate::embed::embed_content;
use crate::version::{sha256_hex, VersionStore};
use anyhow::{Context, Result};

/// A handful of single-topic documents, each with headed sections so chunking
/// produces several distinctly-embeddable blocks. Topics are deliberately far
/// apart so a query disambiguates cleanly.
pub const DOCS: &[(&str, &str)] = &[
    (
        "/demo/sourdough.md",
        "---\ntype: note\n---\n# Sourdough\n\n## Starter\nKeep a rye starter at room temperature and feed it equal weights of flour and water once a day until it doubles reliably.\n\n## Proving\nA long cold prove in the fridge overnight develops sour flavour and an open crumb; bake into a preheated cast-iron pot for oven spring.\n",
    ),
    (
        "/demo/postgres-indexes.md",
        "---\ntype: note\n---\n# Postgres Indexes\n\n## B-tree vs HNSW\nA B-tree index serves equality and range queries on scalar columns; for high-dimensional vector similarity you want an approximate index like HNSW from pgvector.\n\n## When to add one\nIndexes speed reads but slow writes and cost disk — add them for the queries you actually run hot, not speculatively.\n",
    ),
    (
        "/demo/running-injuries.md",
        "---\ntype: note\n---\n# Running Injuries\n\n## Knee pain\nRunner's knee usually comes from ramping mileage too fast; back off volume, strengthen the glutes, and check your cadence is high enough to avoid overstriding.\n\n## Recovery\nRest, ice, and a gradual return-to-run plan beat pushing through pain, which turns a niggle into months out.\n",
    ),
    (
        "/demo/jazz-history.md",
        "---\ntype: note\n---\n# Jazz History\n\n## Bebop\nIn the 1940s Charlie Parker and Dizzy Gillespie sped tempos and stacked complex harmony, turning jazz from dance music into a music for listening.\n\n## Modal jazz\nMiles Davis's Kind of Blue traded fast chord changes for slow-moving modes, giving soloists space to explore.\n",
    ),
    (
        "/demo/rust-ownership.md",
        "---\ntype: note\n---\n# Rust Ownership\n\n## Borrowing\nEach value has a single owner; you can lend out shared references or one mutable reference, and the borrow checker proves no data races at compile time.\n\n## Lifetimes\nLifetimes annotate how long a reference is valid so the compiler can reject dangling pointers without a garbage collector.\n",
    ),
];

/// Seed every demo doc: content-address it, record a version, and embed it.
/// Returns the number of documents seeded.
pub fn seed(versions: &mut VersionStore, api_key: &str) -> Result<usize> {
    for (path, content) in DOCS {
        let bytes = content.as_bytes();
        let hash = sha256_hex(bytes);
        versions
            .record_meta(path, &hash, bytes.len() as i64, Some("demo"))
            .with_context(|| format!("recording demo version for {path}"))?;
        embed_content(versions, api_key, &hash, bytes)
            .with_context(|| format!("embedding demo doc {path}"))?;
    }
    Ok(DOCS.len())
}
