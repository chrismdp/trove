# Self-triggering embeddings

How a single `close()` produces a searchable vector in Postgres — without
the OpenAI round-trip ever sitting on the write path.

## The shape

```
mount startup:
    if --no-embed not set and versions_db resolved:
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut vs = VersionStore::connect(&url)?;
            while let Ok((hash, content)) = rx.recv() {
                let _ = embed_content(&api_key, &mut vs, &hash, &content);
            }
        });
        embed_tx = Some(tx);

commit() success path:
    if let Some(tx) = &embed_tx {
        let _ = tx.send((hash, buf.clone()));   // non-blocking, fire-and-forget
    }
    return Ok(())  // write returns immediately

(meanwhile, in the embed thread)
    recv (hash, content)
    chunks = chunk_paragraphs(&content)     // or chunk_markdown
    vectors = openai_embed(chunks)          // batched, up to 64 chunks per call
    replace_chunks(blob_hash, MODEL, rows)   // delete + insert in one tx
```

## The three pieces, in order

### 1. The send is fire-and-forget

```rust
let _ = tx.send((sha256_hex(&buf), buf.clone()));
```

Three things to notice:

- **`let _ = …`** — we don't care if the receiver dropped. If the embed
  thread crashed, the send is silently lost; a future `trove embed`
  backfill picks it up.
- **`buf.clone()`** — yes, we copy the bytes. The write path already
  validated and committed the original; the clone is a few KiB for a
  typical note. Avoiding the clone would require lifetime-binding the
  send to the buffer's owner, which is more complexity than it saves.
- **No `await`, no callback** — the `commit()` function returns the next
  line. The OpenAI call happens entirely on the other thread.

### 2. The thread owns one DB connection

```rust
let mut vs = VersionStore::connect(&versions_url)?;
```

The embed thread opens its **own** `VersionStore` rather than sharing the
mount's. Two reasons:

- `VersionStore` holds a sync `postgres::Client`. Sharing it would
  require a `Mutex` and serialise the mount and embed paths on the same
  connection.
- One process can hold many connections cheaply; Postgres is happy with
  it.

So writes contend on `fs` (libjfs), not on Postgres.

### 3. `replace_chunks` is idempotent

```rust
pub fn replace_chunks(blob_hash, model, rows) -> Result<()> {
    transaction {
        delete from blob_chunks where blob_hash = $1;
        for r in rows { insert ... }
    }
}
```

A re-run on the same blob deletes the old rows and inserts new ones. So:

- Failed embedding (network error): blob stays "pending", next sweep
  retries.
- Re-chunking strategy change (`TROVE_CHUNK_STRATEGY=heading` ↔
  `paragraph`): `delete from blob_chunks; trove embed` re-builds with
  the new strategy.
- Same content saved twice: same hash → already has chunks → no insert
  (we check `is_blob_chunked` before calling `embed_content`).

## Why we use a hash, not a `(path, rev)` key

The chain says `(path, rev) → blob_hash`. Embeddings hang off
`blob_hash`. So:

- **Two paths with identical content share one set of chunks.** Search
  returns *one* hit (the lateral join in `search_chunks` picks the
  highest-rev path that owns the blob).
- **Editing a file makes a new hash → a new embed job.** The old hash's
  chunks stay in place (in case some other path still references them).
- **Restoring a previous rev** doesn't re-embed (same hash, already
  chunked).

## The pending-set query

```sql
select b.hash from blobs b
where not exists (select 1 from blob_chunks c where c.blob_hash = b.hash)
order by b.created_at
limit $1
```

A blob is "needs embedding" if and only if it has **zero** rows in
`blob_chunks`. This is the source of truth for `trove embed --watch`.

A sentinel row (one chunk, `embedding = NULL`) takes a binary blob out
of the pending set without ever calling OpenAI. The search query
filters out null embeddings, so sentinels can't match.

## What happens if a write batch races the embed thread?

Say five files are committed in rapid succession. The mount queues five
`(hash, content)` pairs. The embed thread processes them one at a time:

- Each `recv` is a separate OpenAI call.
- Each `replace_chunks` is a separate transaction.
- The mount keeps accepting writes during all of it.

There's no batching of multiple files into one OpenAI call. Could be a
future optimisation; v0.1 keeps the code simple.

## What happens if the mount restarts mid-queue?

The channel is in-memory. Anything queued but not yet sent to OpenAI is
**lost**. The blob is in `blobs` (the chain wrote that row), but has no
chunks. The next `trove embed` (run once or via watch) picks it up by
the "no chunks yet" query and embeds it.

**Net effect**: at-most-once delivery from the mount's send, plus
at-least-once delivery from the backfill sweep. The blob ends up
embedded; the question is just *how soon*.

## Sentinel embeddings: skipping bytes that don't deserve a vector

Three cases write a sentinel:

1. **Empty content** — the file was created and committed with zero
   bytes. Nothing to embed.
2. **Binary content** — the `version_pass_through` path for a binary
   sends `(hash, vec![])`. `embed_content` sees the empty vec and
   writes a sentinel.
3. **Non-UTF-8 "text"** — a sniffed-as-text file that turns out to be
   binary. Caught by an `std::str::from_utf8` check; falls through to
   the sentinel path.

The sentinel is a single row with `(blob_hash, ordinal=0, heading=NULL,
start_byte=0, end_byte=0, embedding=NULL)`. The blob now has at least
one row → no longer pending. Search ignores it via `where c.embedding is
not null`.

## The `--watch` mode

```bash
trove embed --watch 30
```

A loop: `run_once`; if any work was done, immediately loop; else sleep
30s. Useful if you ever decouple the embed worker from the mount
process. The default mount has the worker in-process, so `--watch` is
for catch-up duty.

## What the pipeline is not

- **Not transactional with the file write.** If embedding fails, the
  file is still committed and the chain row is still there. Search is
  best-effort.
- **Not real-time guaranteed.** "Self-triggering" means "within
  milliseconds of the write" in the normal case, not "before the
  syscall returns".
- **Not idempotent across model versions.** Switching models means
  manually clearing `blob_chunks` and re-embedding.

Next: [Running it end-to-end →](/docs/running)
