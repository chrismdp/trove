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
    /// When `schema` is set, the session's `search_path` is pointed at it (plus
    /// `public`/`extensions` for the `vector` type), so every unqualified query
    /// in this module resolves against the volume's isolated schema rather than
    /// `public`.
    pub fn connect(url: &str, schema: Option<&str>) -> Result<Self> {
        let mut client = Client::connect(url, NoTls).context("connecting to Trove version DB")?;
        if let Some(s) = schema {
            // `s` is a sanitised identifier (see `config::schema_for`); double
            // any quote defensively before interpolating.
            let ident = s.replace('"', "\"\"");
            client
                .batch_execute(&format!(
                    "set search_path to \"{ident}\", public, extensions"
                ))
                .with_context(|| format!("setting search_path to schema {s}"))?;
        }
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
        let (rev, parent) = if head == 0 {
            (1, None)
        } else {
            (head + 1, Some(head))
        };
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

    /// Every distinct path that has at least one version, alphabetically. The
    /// `trove server` `/files` listing (the live tree's known files, from the
    /// version chain — no libjfs walk needed).
    pub fn paths(&mut self) -> Result<Vec<String>> {
        let rows = self
            .client
            .query("select distinct path from file_versions order by path", &[])?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    /// Doctor support: is pgvector installed, and which of Trove's expected
    /// tables are missing? Returns `(pgvector_present, missing_tables)`. Run
    /// against the connected DB so `trove doctor` can report schema readiness.
    pub fn diagnostics(&mut self) -> Result<(bool, Vec<String>)> {
        let pgvector: bool = self
            .client
            .query_one(
                "select exists(select 1 from pg_extension where extname = 'vector')",
                &[],
            )?
            .get(0);
        let mut missing = Vec::new();
        for table in ["blobs", "file_versions", "blob_chunks"] {
            let present: bool = self
                .client
                .query_one("select to_regclass($1) is not null", &[&table])?
                .get(0);
            if !present {
                missing.push(table.to_string());
            }
        }
        Ok((pgvector, missing))
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

    /// Hashes of blobs whose existing embeddings do NOT include the target
    /// `model` — i.e. they were embedded with a different model (or sentinel-
    /// embedded with a null embedding_model). Used by `trove embed --remodel`
    /// to migrate the vault after the MODEL constant changes. `limit` matches
    /// the batching pattern used by `pending_embedding_hashes`.
    pub fn stale_model_hashes(&mut self, model: &str, limit: i64) -> Result<Vec<String>> {
        let rows = self.client.query(
            "select b.hash from blobs b \
             where exists (select 1 from blob_chunks c where c.blob_hash = b.hash) \
               and not exists ( \
                 select 1 from blob_chunks c \
                 where c.blob_hash = b.hash and c.embedding_model = $1 \
               ) \
             order by b.created_at limit $2",
            &[&model, &limit],
        )?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }
}

/// A point-in-time snapshot of how much room Trove is taking in Postgres —
/// the rows the operator pays for and the bytes Postgres pays for to store
/// them. All `_bytes` are server-reported (`pg_total_relation_size` /
/// `pg_database_size`) so they include indexes, toast, and free space the
/// table is holding — i.e. the bill-shaped figure, not just live tuple bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbUsage {
    pub database_bytes: i64,
    pub blobs_rows: i64,
    pub blobs_bytes: i64,
    pub file_versions_rows: i64,
    pub file_versions_bytes: i64,
    pub blob_chunks_rows: i64,
    pub blob_chunks_bytes: i64,
    /// Distinct `file_versions.path` values — the live tree's "known files"
    /// count (a file with 5 revs counts once).
    pub distinct_paths: i64,
    /// Blobs with at least one row in `blob_chunks` (sentinel rows count —
    /// they mean "we processed it and decided there's nothing to embed").
    pub embedded_blobs: i64,
    /// Blobs with no rows in `blob_chunks` — what `trove embed` will pick up
    /// next. A climbing number with `embedded_blobs` flat means the embedder
    /// isn't running (or `OPENAI_API_KEY` is missing).
    pub pending_blobs: i64,
}

impl VersionStore {
    /// One-shot snapshot of DB usage — sizes, counts, embedding progress.
    /// Runs every query in a single read-only transaction so the snapshot is
    /// internally consistent (no torn reads if a write commits between calls).
    /// Returns a useful error if a Trove table is missing rather than silently
    /// returning 0 — schema readiness is `doctor`'s job, and `usage` assumes
    /// the migration has run.
    pub fn usage(&mut self) -> Result<DbUsage> {
        let mut tx = self.client.build_transaction().read_only(true).start()?;

        // Fail loud if a table is missing — pg_total_relation_size on a
        // non-existent regclass would otherwise panic the query.
        for table in ["blobs", "file_versions", "blob_chunks"] {
            let present: bool = tx
                .query_one("select to_regclass($1) is not null", &[&table])?
                .get(0);
            if !present {
                anyhow::bail!(
                    "schema not migrated — table `{table}` is missing. Run `trove init` in the vault folder."
                );
            }
        }

        let database_bytes: i64 = tx
            .query_one("select pg_database_size(current_database())::bigint", &[])?
            .get(0);

        let row = tx.query_one(
            "select \
                 (select count(*) from blobs)::bigint, \
                 pg_total_relation_size('blobs'::regclass)::bigint, \
                 (select count(*) from file_versions)::bigint, \
                 pg_total_relation_size('file_versions'::regclass)::bigint, \
                 (select count(*) from blob_chunks)::bigint, \
                 pg_total_relation_size('blob_chunks'::regclass)::bigint, \
                 (select count(distinct path) from file_versions)::bigint, \
                 (select count(*) from blobs b \
                    where exists (select 1 from blob_chunks c where c.blob_hash = b.hash))::bigint, \
                 (select count(*) from blobs b \
                    where not exists (select 1 from blob_chunks c where c.blob_hash = b.hash))::bigint",
            &[],
        )?;

        let usage = DbUsage {
            database_bytes,
            blobs_rows: row.get(0),
            blobs_bytes: row.get(1),
            file_versions_rows: row.get(2),
            file_versions_bytes: row.get(3),
            blob_chunks_rows: row.get(4),
            blob_chunks_bytes: row.get(5),
            distinct_paths: row.get(6),
            embedded_blobs: row.get(7),
            pending_blobs: row.get(8),
        };
        tx.commit()?;
        Ok(usage)
    }
}

/// One embedding row to write for a blob. `embedding` is the pgvector text
/// literal (`"[0.1,0.2,…]"`) or `None` for a processed-but-not-embedded blob
/// (empty/binary) — a sentinel so the blob stops showing as "needs embedding".
pub struct ChunkInsert<'a> {
    pub ordinal: i32,
    pub heading: Option<&'a str>,
    pub start_byte: i32,
    pub end_byte: i32,
    pub embedding: Option<String>,
}

impl VersionStore {
    /// Replace all embedding chunks for `blob_hash` (delete + insert in one
    /// transaction) — idempotent re-embedding. After this, the blob is no longer
    /// "pending" (it has rows), so a re-run won't re-process it.
    pub fn replace_chunks(
        &mut self,
        blob_hash: &str,
        model: &str,
        rows: &[ChunkInsert],
    ) -> Result<()> {
        let mut tx = self.client.transaction()?;
        tx.execute(
            "delete from blob_chunks where blob_hash = $1",
            &[&blob_hash],
        )?;
        for r in rows {
            tx.execute(
                // `$6::text::vector`: bind the param as text (so the driver
                // serializes a Rust String/None), then cast text -> vector.
                // `$6::vector` alone makes the server type the param as `vector`,
                // which the driver can't serialize a String into.
                "insert into blob_chunks \
                 (blob_hash, ordinal, heading, start_byte, end_byte, embedding, embedding_model) \
                 values ($1, $2, $3, $4, $5, $6::text::vector, $7)",
                &[
                    &blob_hash,
                    &r.ordinal,
                    &r.heading,
                    &r.start_byte,
                    &r.end_byte,
                    &r.embedding,
                    &model,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// One semantic-search hit: the best-matching chunk of a file, with where it
/// sits in the file (heading + byte range, for deep-linking) and how close it
/// is to the query. `distance` is pgvector cosine distance in `[0, 2]` — lower
/// is nearer; `1 - distance` is cosine similarity.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub path: String,
    pub heading: Option<String>,
    pub start_byte: i32,
    pub end_byte: i32,
    pub distance: f64,
}

impl VersionStore {
    /// Semantic search over embedded chunks: the `top_k` chunks nearest to a
    /// query vector by cosine distance, each resolved to the file that holds it.
    ///
    /// `query_literal` is the query embedding as a pgvector text literal
    /// (`"[0.1,0.2,…]"`), built by [`crate::embed::embed_query_literal`]. The
    /// ORDER BY casts both sides to `halfvec(3072)` to match the
    /// `blob_chunks_embedding_hnsw` index expression, so the ANN index is used.
    ///
    /// A blob is content-addressed and can back several paths (dedup); the
    /// lateral picks the highest-rev path that references it — i.e. the most
    /// recent file to hold that exact content. Sentinel rows (null embedding,
    /// from binary/blank blobs) are skipped.
    pub fn search_chunks(&mut self, query_literal: &str, top_k: i64) -> Result<Vec<SearchHit>> {
        let rows = self.client.query(
            "select fv.path, c.heading, c.start_byte, c.end_byte, \
                    c.embedding::halfvec(3072) <=> $1::text::halfvec(3072) as distance \
             from blob_chunks c \
             join lateral ( \
                 select path from file_versions where blob_hash = c.blob_hash \
                 order by rev desc limit 1 \
             ) fv on true \
             where c.embedding is not null \
             order by c.embedding::halfvec(3072) <=> $1::text::halfvec(3072) \
             limit $2",
            &[&query_literal, &top_k],
        )?;
        Ok(rows
            .iter()
            .map(|r| SearchHit {
                path: r.get(0),
                heading: r.get(1),
                start_byte: r.get(2),
                end_byte: r.get(3),
                distance: r.get(4),
            })
            .collect())
    }
}

/// Lowercase hex sha256 — the blob content-address.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}
