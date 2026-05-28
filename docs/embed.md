# `embed.rs` — the embedding worker

**~490 lines.** Turn blobs into vectors. The server-side worker that
chunks markdown by headings, embeds each chunk via OpenAI, and writes the
result to `blob_chunks`.

This module has three personas:

1. **The chunker** — pure markdown → chunks, no IO. Unit-tested in the file.
2. **The HTTP client** — `ureq` to OpenAI's embeddings endpoint.
3. **The orchestrator** — `run_once` / `run_watch` / `spawn_embedder`. The
   three ways to actually do work.

## Why a worker, not inline?

A commit can't sit on a network round-trip to OpenAI. Even at the
~100-300ms typical, that's three orders of magnitude slower than a local
write. Two reasons matter:

- **Latency**: agents waiting on `close()` would feel sluggish.
- **Failure mode**: OpenAI down would mean writes failing, not just search
  going stale.

So embedding happens off the write path:

- **`spawn_embedder`** — start a background thread at mount time.
  `commit()` pushes `(hash, content)` over an `mpsc::Sender`; the thread
  picks them up, embeds, writes to Postgres. Non-blocking send.
- **`run_once`** — a single sweep of `pending_embedding_hashes`. For
  backfilling after a `mount` ran with `--no-embed`, or for catching up
  after the thread died.
- **`run_watch`** — a loop of `run_once` with a `LISTEN`-style poll.
  Useful if you ever want to decouple the embed worker from the mount
  (separate processes; not the v0.1 default).

## The chunker: two strategies

```rust
pub fn chunk_markdown(content: &str) -> Vec<Chunk>;     // split at every heading
pub fn chunk_paragraphs(content: &str) -> Vec<Chunk>;   // paragraph-clustered, size-bounded
```

Toggled by `TROVE_CHUNK_STRATEGY=heading|paragraph`. Default is paragraph.

### `chunk_markdown` (heading)

Splits at every ATX heading (`#`..`######`). Text before the first heading
is the preamble (chunk 0, no heading). Each heading starts a new chunk
running to the next heading. Empty chunks dropped. Byte ranges preserved.

Pros: matches how humans navigate a doc.
Cons: a doc with 30 H3s makes 30 tiny chunks; a doc with one H1 and 5000
lines is one giant chunk that exceeds the model's input cap.

### `chunk_paragraphs` (default)

Splits on blank lines (paragraph blocks), then:

- **Clusters** consecutive paragraphs until a chunk reaches
  `MIN_CHUNK_CHARS = 400` (~100 tokens).
- **Never exceeds** `MAX_CHUNK_CHARS = 4000` (~1000 tokens). A single
  over-max paragraph is hard-split at a char/newline boundary.
- **Keeps a lone heading attached** to the paragraph that follows it
  (never a heading-only chunk while content remains).
- **Byte ranges partition** the document exactly. Concatenating
  `&content[s..e]` for each chunk reconstructs the input.

Better hit rates for prose, more tractable for the model. The heading
strategy is kept for users with strongly structured documents (lots of
H3-sized atomic notes).

## The token math

```rust
const MIN_CHUNK_CHARS: usize = 400;   // ~100 tokens
const MAX_CHUNK_CHARS: usize = 4000;  // ~1000 tokens
```

We approximate at the well-known **~4 chars per token** for English prose.
`text-embedding-3-large` accepts up to ~8191 tokens; capping at ~1000
leaves headroom and produces more focussed vectors.

No tokenizer dependency. If you ever swap models, recheck the constant.

## Heading detection

```rust
fn heading_text(line: &str) -> Option<String> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ') {
        Some(line[hashes..].trim().to_string())
    } else {
        None
    }
}
```

ATX headings only (`# title`, `## title`, …). Setext (`title\n=====`) is
not supported — Trove's source-of-truth is agent-written markdown, and
agents almost universally use ATX.

A "heading-like" line that doesn't have whitespace after the hashes (e.g.
`#tag`) is not a heading.

## The OpenAI call

```rust
const MODEL: &str = "text-embedding-3-large";
```

`ureq` POSTs to `/v1/embeddings` with `{"input": [chunk_text, …], "model":
MODEL}`. Response is parsed into a `Vec<Vec<f32>>` of 3072-dimensional
vectors. Errors surface as `anyhow` errors — the worker logs and skips
the blob (it stays "pending" for the next sweep).

Batching: up to `BATCH = 64` chunks per call. OpenAI bills per input
token regardless, so batching is a latency optimisation, not a cost one.

## Pgvector text literals

A vector goes into Postgres as a **text literal** like `"[0.1,0.2,…]"`,
which is then cast to `vector` server-side (see [`version.rs`](/docs/version)
for the `$N::text::vector` trick). Rust-side:

```rust
fn vector_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 { s.push(','); }
        s.push_str(&format!("{x:.6}"));
    }
    s.push(']');
    s
}
```

Six decimal places is enough precision for cosine search; fewer would
quantise too much, more bloats the wire.

## Sentinel embeddings: skipping binaries

A blob that's empty or non-UTF-8 doesn't deserve a vector, but it still
needs to *not* show as "pending" forever. The worker writes a **sentinel
row**: one `blob_chunks` row with `embedding = NULL` and the model name.

The pending query is "blobs with no chunks at all", so a sentinel removes
the blob from the queue. The search query filters `where c.embedding is
not null`, so sentinels never match.

## `spawn_embedder` — the in-process pattern

```rust
pub fn spawn_embedder(versions_url: &str, api_key: String)
    -> Result<Sender<(String, Vec<u8>)>>;
```

What it does:

1. Open a fresh `VersionStore` (the worker owns its own DB connection —
   no sharing with the mount).
2. `mpsc::channel`. Return the `Sender`; keep the `Receiver` in the thread.
3. Spawn a `thread::spawn` that drains the channel forever:
   - `recv` → `embed_content(hash, content, …)`
   - on error, log and continue
4. The mount's `commit()` calls `tx.send((hash, content)).unwrap_or(())` —
   non-blocking, drop on full (the channel is unbounded; "drop on full"
   would require a bounded channel — not currently).

The thread lives for the lifetime of the mount. There's no clean shutdown;
when the mount exits, the receiver is dropped and the thread terminates.

## What the worker doesn't do

- **No retries** beyond a single OpenAI call. A transient 429/500 is logged
  and the blob stays pending for the next sweep.
- **No incremental embeddings.** Changing one paragraph re-embeds the
  whole blob. The chunker is fast and OpenAI is the cost; partial
  re-embedding isn't worth the complexity.

## Model migrations

`trove embed --remodel` re-embeds every blob whose existing chunks are on
a different `embedding_model` than the one this binary is compiled against
(the `MODEL` constant in `embed.rs`). Use it after bumping the constant
and rebuilding — no manual `delete from blob_chunks` needed.

- **Idempotent.** Once every blob is on the current model the query
  returns nothing, so re-running is a no-op. Safe to wire into a script.
- **Doesn't change the constant for you.** Bump `MODEL` in `embed.rs`,
  rebuild, *then* run `trove embed --remodel`. Otherwise it'll happily
  re-embed back to the old model.
- **Mutually exclusive with `--watch`.** Remodel is a one-shot migration,
  not a steady-state loop. Passing both is rejected with an error.
- **Cost warning.** A vault with 10k chunks at `text-embedding-3-large`
  costs roughly $1.30 to re-embed; budget accordingly before running on a
  big vault.

## Tests

- **`src/embed.rs` unit tests** — the chunker (both strategies), heading
  detection, byte-range correctness, oversized paragraph splitting.
- **`tests/embed.rs`** — real OpenAI round-trip: embed a blob, verify
  3072-dim vectors land in `blob_chunks` with correct headings.

The unit tests run in milliseconds and catch every chunker regression.
The integration test costs ~1¢ per run.

Next: [The write pipeline →](/docs/write-pipeline)
