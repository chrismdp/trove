//! Trove's version chain — the history side of the substrate.
//!
//! JuiceFS holds the live tree; this module owns *history*. On every validated
//! commit through the mount's commit barrier, [`VersionStore::record`] appends a
//! row to the per-path version chain and stores the content content-addressed in
//! the `blobs` registry (dedup: identical bytes are stored once). History, diff,
//! and restore read back from here (`trove log` / `diff` / `cat@rev`).
//!
//! The store is **synchronous** on purpose: the FUSE handlers that call it are
//! sync, and the pure-Rust `postgres` crate needs no async runtime — and no
//! libpq — so the core crate stays native-dependency-free.
//!
//! Embeddings are intentionally NOT computed here. `record` leaves
//! `blobs.embedding` null; a separate async pass fills it (an OpenAI round-trip
//! must never sit on the commit barrier). The schema (Supabase migration
//! `…_init_version_chain_and_embeddings.sql`) carries the `vector(3072)` column
//! and its HNSW index ready for that pass and for `trove search`.

use anyhow::{Context, Result};
use postgres::{Client, NoTls};
use sha2::{Digest, Sha256};

/// One entry in a path's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub rev: i32,
    pub blob_hash: String,
    pub parent_rev: Option<i32>,
    pub size: i64,
    pub author: Option<String>,
}

/// A connection to Trove's version/embedding database (Supabase Postgres).
pub struct VersionStore {
    client: Client,
}

impl VersionStore {
    /// Connect to the version DB (e.g. `postgres://postgres:postgres@127.0.0.1:54322/postgres`).
    pub fn connect(url: &str) -> Result<Self> {
        let client = Client::connect(url, NoTls).context("connecting to Trove version DB")?;
        Ok(Self { client })
    }

    /// Record the metadata for a validated commit of `path`. The bytes
    /// themselves are stored content-addressed in R2 by the caller (the
    /// [`crate::recorder::Recorder`]); `blob_hash` is their sha256.
    ///
    /// 1. Upsert the blob row (`hash`, `size`) — a no-op if the hash already
    ///    exists, which is how dedup falls out.
    /// 2. Append a `file_versions` row at the next rev for `path`, linking
    ///    `parent_rev` to the previous head (null for a path's first version).
    ///
    /// Returns the new rev. Runs in one transaction so a crash can't leave a
    /// blob row without its version row or a gap in the chain. Same-path callers
    /// are serialised upstream by the mount's per-inode lock (see the
    /// concurrency spike), so the `(path, rev)` unique constraint is a backstop,
    /// not the primary coordination.
    pub fn record_meta(
        &mut self,
        path: &str,
        blob_hash: &str,
        size: i64,
        author: Option<&str>,
    ) -> Result<i32> {
        let mut tx = self.client.transaction()?;
        tx.execute(
            "insert into blobs (hash, size) values ($1, $2) on conflict (hash) do nothing",
            &[&blob_hash, &size],
        )?;
        let head: i32 = tx
            .query_one(
                "select coalesce(max(rev), 0) from file_versions where path = $1",
                &[&path],
            )?
            .get(0);
        let (rev, parent) = if head == 0 { (1, None) } else { (head + 1, Some(head)) };
        tx.execute(
            "insert into file_versions (path, rev, blob_hash, parent_rev, author, size) \
             values ($1, $2, $3, $4, $5, $6)",
            &[&path, &rev, &blob_hash, &parent, &author, &size],
        )?;
        tx.commit()?;
        Ok(rev)
    }

    /// Full history of `path`, newest revision first. Empty if `path` has none.
    pub fn log(&mut self, path: &str) -> Result<Vec<Version>> {
        let rows = self.client.query(
            "select rev, blob_hash, parent_rev, size, author from file_versions \
             where path = $1 order by rev desc",
            &[&path],
        )?;
        Ok(rows
            .iter()
            .map(|r| Version {
                rev: r.get(0),
                blob_hash: r.get(1),
                parent_rev: r.get(2),
                size: r.get(3),
                author: r.get(4),
            })
            .collect())
    }

    /// The blob hash of `path` at revision `rev` — the R2 key for its bytes
    /// (`trove cat <path>@<rev>` reads PG for this, then R2 for the content).
    /// `None` if that (path, rev) has no version.
    pub fn blob_hash_at(&mut self, path: &str, rev: i32) -> Result<Option<String>> {
        let rows = self.client.query(
            "select blob_hash from file_versions where path = $1 and rev = $2",
            &[&path, &rev],
        )?;
        Ok(rows.first().map(|r| r.get(0)))
    }

    /// Hashes of blobs not yet embedded — i.e. with no `blob_chunks` rows. The
    /// server-side `trove embed` worker's sweep (it reads each blob's content
    /// from its JuiceFS clone, chunks it, and inserts the embeddings). Capped by
    /// `limit` so the worker batches.
    pub fn pending_embedding_hashes(&mut self, limit: i64) -> Result<Vec<String>> {
        let rows = self.client.query(
            "select b.hash from blobs b \
             where not exists (select 1 from blob_chunks c where c.blob_hash = b.hash) \
             order by b.created_at limit $1",
            &[&limit],
        )?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }
}

/// Lowercase hex sha256 — the blob content-address.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}
