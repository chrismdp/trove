# `version.rs` — the version DB

**~265 lines.** The Postgres side of history and search. The version chain,
the blob registry, the embeddings, and a (synchronous, pure-Rust) client to
talk to all three.

## The tables, in order of dependency

```
blobs           (hash pk, size, created_at)
file_versions   (id, path, rev, blob_hash→blobs, parent_rev, author, size, …)
blob_chunks     (id, blob_hash→blobs, ordinal, heading, start_byte, end_byte,
                 embedding vector(3072), embedding_model)
```

- **`blobs`** — content-addressed registry. One row per unique content. The
  *bytes* live in JuiceFS at `/.trove/versions/<hash>`; this row exists so
  the chain can reference content and `blob_chunks` can hang embeddings off
  it. Dedup is free: same bytes → one row.

- **`file_versions`** — per-path version chain. Append-only. `rev` is
  monotonic per path; `parent_rev` links the chain. A version is `(path,
  rev) → blob_hash`; its bytes are the JuiceFS clone keyed by that hash.

- **`blob_chunks`** — embeddings. One blob → N header-delimited chunks
  (whole-file = N=1). HNSW index for ANN search.

## Why pure-Rust `postgres`?

```rust
use postgres::{Client, NoTls};
```

The `postgres` crate is synchronous and doesn't need libpq. Two consequences:

1. **No async runtime** — the FUSE handlers that call into here are
   synchronous, and bolting tokio in just for one DB call would be a poor
   trade.
2. **No libpq native dep** — the core crate stays native-dep-free. The
   `mount` feature pulls in libjfs; `postgres` does not.

The cost: one DB connection per `VersionStore`, no pooling. For a
single-tenant mount with at most a few writes per second, that's fine. The
hot path is `record_meta`, which executes 2-3 statements inside a single
transaction.

## `record_meta`: one transaction, two writes

```rust
pub fn record_meta(&mut self, path, blob_hash, size, author) -> Result<i32>;
```

Inside a single transaction:

1. `insert into blobs (hash, size) values ($1, $2) on conflict (hash) do
   nothing` — dedup falls out of the unique constraint.
2. `select coalesce(max(rev), 0) from file_versions where path = $1` —
   find the current head.
3. `insert into file_versions (…) values (…)` at `rev = head + 1`, with
   `parent_rev = head` (or null for the first revision).

The transaction makes step 1+3 atomic, so a crash can't leave a blob row
with no version row, or vice versa.

## Why count-from-head?

Wouldn't it be simpler to use `generated always as identity` for `rev`?

No — `rev` is **per-path**, not global. Two files at rev 1, 2, 3 each, not
"file A is rev 1, file B is rev 2". A serial column would give global
identity numbers, useless for history.

The trade is that two simultaneous writes to the same path could compute
the same `head`. The `unique (path, rev)` constraint catches this; the
mount serialises per-inode upstream so it doesn't normally happen in
practice. If it does, one write fails and retries.

## `log`, `blob_hash_at`, `paths`

```rust
pub fn log(&mut self, path: &str) -> Result<Vec<Version>>;
pub fn blob_hash_at(&mut self, path: &str, rev: i32) -> Result<Option<String>>;
pub fn paths(&mut self) -> Result<Vec<String>>;
```

Straight reads:

- **`log`** — `select … from file_versions where path = $1 order by rev
  desc`. The `file_versions_path_rev_desc` index is what makes this O(1)
  for "head of chain" queries.
- **`blob_hash_at`** — `select blob_hash from file_versions where path = $1
  and rev = $2`. Used by `versioning::cat`.
- **`paths`** — `select distinct path from file_versions order by path`.
  The `trove server` `/files` endpoint. Note: this is the set of paths that
  have **ever** had a version, not the current contents of the live tree.

## `replace_chunks`: embedding writes

```rust
pub fn replace_chunks(&mut self, blob_hash, model, rows: &[ChunkInsert]) -> Result<()>;
```

Idempotent re-embedding: delete + insert in one transaction. After this,
the blob has rows, so it's no longer "pending" — a re-run won't re-process
it.

The interesting line is the SQL parameter cast:

```sql
insert into blob_chunks (… embedding …) values (… $6::text::vector …)
```

`$6::text::vector` casts the parameter to `text` first (so the driver
serialises the Rust `String` straight), then to `vector` server-side.
Without the `::text` step, the server would type the parameter as `vector`
directly, which the driver can't construct from a String. This is one of
the trickier bits to debug if you ever swap in a different vector library.

## `search_chunks`: the ANN query

```rust
pub fn search_chunks(&mut self, query_literal: &str, top_k: i64) -> Result<Vec<SearchHit>>;
```

The SQL is worth reading in full:

```sql
select fv.path, c.heading, c.start_byte, c.end_byte,
       c.embedding::halfvec(3072) <=> $1::text::halfvec(3072) as distance
from blob_chunks c
join lateral (
    select path from file_versions where blob_hash = c.blob_hash
    order by rev desc limit 1
) fv on true
where c.embedding is not null
order by c.embedding::halfvec(3072) <=> $1::text::halfvec(3072)
limit $2
```

Three things:

1. **`<=>`** is pgvector cosine distance. Lower = closer. `1 - distance`
   is cosine similarity.
2. **`::halfvec(3072)`** — pgvector's `vector` type indexes up to 2000
   dimensions; `text-embedding-3-large` is 3072. The HNSW index is built
   on the `halfvec` cast, so the query casts both sides to match.
3. **The lateral join** — a blob is content-addressed and can back many
   paths (dedup). We pick the **highest-rev** path that references it
   (the most recent file to hold that content). This means search results
   show the current owner of a duplicated note, not a historical owner.

## `diagnostics`: the doctor's input

```rust
pub fn diagnostics(&mut self) -> Result<(bool, Vec<String>)>;
```

A pair: is pgvector installed, and which of `(blobs, file_versions,
blob_chunks)` are missing? `trove doctor` calls this to report schema
readiness. Independent of the mount path, useful in CI to confirm
migrations have run.

## What the module doesn't do

- **No embedding generation.** That's [`embed.rs`](/docs/embed).
- **No COW clones.** That's [`versioning.rs`](/docs/versioning).
- **No connection pooling.** One `Client` per `VersionStore`. The mount
  has one; `trove embed` and `trove search` each open their own.

Next: [`embed.rs` — the embedding worker →](/docs/embed)
