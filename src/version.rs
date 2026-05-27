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

    /// Record a validated commit of `path` carrying `content`.
    ///
    /// 1. Content-address the blob (sha256) and upsert it — a no-op if the hash
    ///    already exists, which is how dedup falls out.
    /// 2. Append a `file_versions` row at the next rev for `path`, linking
    ///    `parent_rev` to the previous head (null for a path's first version).
    ///
    /// Returns the new rev. Runs in one transaction so a crash can't leave a
    /// blob without its version row or a gap in the chain. Same-path callers
    /// are serialised upstream by the mount's per-inode lock (see the
    /// concurrency spike), so the `(path, rev)` unique constraint is a backstop,
    /// not the primary coordination.
    pub fn record(&mut self, path: &str, content: &[u8], author: Option<&str>) -> Result<i32> {
        let hash = sha256_hex(content);
        let size = content.len() as i64;

        let mut tx = self.client.transaction()?;
        tx.execute(
            "insert into blobs (hash, size, content) values ($1, $2, $3) \
             on conflict (hash) do nothing",
            &[&hash, &size, &content],
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
            &[&path, &rev, &hash, &parent, &author, &size],
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

    /// The content of `path` at revision `rev` (for `trove cat <path>@<rev>` /
    /// `diff` / `restore`). `None` if that (path, rev) has no version.
    pub fn cat(&mut self, path: &str, rev: i32) -> Result<Option<Vec<u8>>> {
        let rows = self.client.query(
            "select b.content from file_versions v join blobs b on b.hash = v.blob_hash \
             where v.path = $1 and v.rev = $2",
            &[&path, &rev],
        )?;
        Ok(rows.first().map(|r| r.get::<_, Vec<u8>>(0)))
    }

    /// Blobs still awaiting an embedding, with their content — the embed
    /// worker's sweep (the backstop behind the `blob_needs_embedding` NOTIFY).
    /// Capped by `limit` so the worker batches. Returns `(hash, content)`.
    pub fn pending_embeddings(&mut self, limit: i64) -> Result<Vec<(String, Vec<u8>)>> {
        let rows = self.client.query(
            "select hash, content from blobs where embedding is null \
             order by created_at limit $1",
            &[&limit],
        )?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }
}

/// Lowercase hex sha256 — the blob content-address.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}
