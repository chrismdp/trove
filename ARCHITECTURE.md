# Trove architecture

**A filesystem that talks back.** Trove is an agent-native knowledge substrate:
agents and humans read/write ordinary files, and the filesystem itself enforces
schemas on write, keeps full version history, and builds a semantic search
index — with zero per-agent integration. The thesis is that coordination,
validation and visibility belong *at the filesystem layer*, not bolted onto each
agent via MCP/SDK glue.

This document describes the system as built. Design rationale and the (long)
decision history live in the vault project note `Trove - V1 Substrate Build` and
in `git log`; this is the "how it works now" reference.

## The one storage substrate

Everything is stored in **one JuiceFS volume** backed by **Cloudflare R2**
(object storage for file data, chunked) plus **one Postgres** (Supabase) for
metadata. The same Postgres holds *both* JuiceFS's own metadata (`jfs_*` tables)
*and* Trove's tables (`blobs`, `file_versions`, `blob_chunks`). There is **no
second storage system** — Trove never writes R2 directly.

Why this matters: JuiceFS stores files as opaque, chunked, possibly
compressed/encrypted objects keyed by chunk-id, with the path→chunk map in
Postgres. You cannot read a file out of R2 by name — only libjfs can reassemble
it. Trove embeds libjfs in-process (Rust FFI) so the `trove` binary *is* the
JuiceFS client; nobody runs or sees "juicefs".

## The write pipeline

A single `write()` + `close()` through the mount flows:

```
kernel write → FUSE → [validation gate] → commit:
    1. write the new content to the live tree (libjfs)
    2. COW-clone it to /.trove/versions/<sha256>   (version snapshot)
    3. append a row to file_versions                (the chain)
    4. push (hash, content) to the embed thread     (if mount --embed)
→ background: chunk by header → OpenAI → blob_chunks  (vectors)
```

- **Validation gate.** A governed file (its path matches a schema glob in
  `.types/`) is buffered whole; at the commit barrier (`flush`/`fsync`) it's
  validated against its JSON Schema. Invalid → `EINVAL`, nothing persists, a
  `<path>.errors` sidecar explains why. Valid → commits. This is the "talks
  back" property. POSIX-*shaped*, not POSIX-complete: the gate is the one
  deliberate deviation; everything else delegates to libjfs.
- **Versioning = copy-on-write, zero duplication.** After a valid write, Trove
  `jfs_clone`s the file to `/.trove/versions/<hash>`. A JuiceFS clone is
  metadata-only — it shares the underlying data blocks and bumps a refcount, so
  **no bytes are copied**. Overwrite the live file later and COW preserves the
  clone's old blocks. History is therefore free of byte duplication: N versions
  = N clones sharing whatever blocks they have in common.
- **Best-effort, no WAL.** The clone and the chain row ride the *same* backend
  the live write just succeeded against (one JuiceFS volume, one Postgres), so
  there's no independent store to be "eventually consistent" with. Versioning is
  a light retry; failure is logged and never fails the file write (the live tree
  is the source of truth).
- **Embedding self-triggers on commit** (when `mount --embed`). The mount spawns
  one background thread at startup; `commit()` hands it `(hash, content)` over an
  in-process channel — embedding runs off the write path, straight from the write
  buffer (no libjfs re-read), and the send is non-blocking. No cron, no daemon to
  babysit, no per-commit subprocess.

## Reads & history

- `cat(path, rev)` resolves the blob hash from `file_versions`, then reads
  `/.trove/versions/<hash>` back through libjfs. Every revision's exact bytes are
  recoverable (the restore data path).
- The live tree is the working copy; `/.trove/versions/` is the immutable
  history. Diffs are computed on demand from two revisions' content.

## Components (`src/`)

| Module | Role |
|---|---|
| `frontmatter.rs`, `types.rs`, `validate.rs` | Parse YAML frontmatter; load the `.types/` JSON-Schema registry (glob-selected, Cursor-rules style); validate. The native-dep-free core (`trove check`). |
| `jfs.rs` | Safe Rust wrapper over libjfs's C ABI (open/read/write/clone/stat/readdir/rename/symlink/…). The in-process JuiceFS client. `mount` feature. |
| `mount.rs` | The FUSE filesystem (`fuser`). Routes opens to Read / PassThrough (binary/ungoverned, streams straight through) / Write (governed, buffered + gated). Owns the commit barrier. |
| `versioning.rs` | `record_version` (COW clone + chain row, best-effort) and `cat` (historical read). |
| `version.rs` | `VersionStore` — Postgres metadata: `record_meta`, `log`, `blob_hash_at`, `pending_embedding_hashes`, `replace_chunks`. Pure-Rust `postgres` crate (no async, no libpq), so it's in the core build. |
| `embed.rs` | The embed worker: `chunk_markdown` (split at ATX headings), OpenAI `text-embedding-3-large` (`ureq`), `embed_content` (buffer path) / `embed_blob` (clone path), `run_once`/`run_watch`, `spawn_embedder` (the on-commit background thread). |

The `mount` cargo feature gates everything needing native deps (libjfs FFI, TLS
for OpenAI). `trove check` builds with no native dependencies.

## Database schema

One migration, `supabase/migrations/…_init_version_chain_and_embeddings.sql`:

- **`blobs`** `(hash pk, size, created_at)` — content-addressed registry. One row
  per unique content; the *bytes* live in JuiceFS (the COW clone), not here.
- **`file_versions`** `(id, path, rev, blob_hash→blobs, parent_rev, author, size,
  created_at, unique(path,rev))` — the per-path version chain. The bit JuiceFS
  doesn't track.
- **`blob_chunks`** `(id, blob_hash→blobs, ordinal, heading, start_byte,
  end_byte, embedding vector(3072), embedding_model, unique(blob_hash,ordinal))`
  — embeddings. One blob → N header-delimited chunks (whole-file = N=1). HNSW
  index on `embedding::halfvec(3072)` (pgvector's `vector` hnsw caps at 2000
  dims; the halfvec cast indexes 3072). "Needs embedding" = a blob with no rows
  here.

JuiceFS's `--meta` points at this same Postgres.

## Running it

```bash
# 1. Format a volume (once). Metadata + Trove tables share one Postgres.
juicefs format --storage s3 --bucket https://<bucket>.<acct>.r2.cloudflarestorage.com \
    --access-key $R2_ACCESS_KEY_ID --secret-key $R2_SECRET_ACCESS_KEY \
    postgres://…supabase… myvol

# 2. Mount with versioning + self-triggering embedding.
trove mount /mnt/trove --volume myvol --meta postgres://…supabase… \
    --types ~/vault --versions-db postgres://…supabase… --embed
#   --types <dir>      enable the validation gate (.types/ registry)
#   --versions-db <url> enable version capture (same Postgres as --meta)
#   --embed            embed each committed file on write (needs OPENAI_API_KEY)

# 3. Validate a store without mounting.
trove check ~/vault [--quiet]

# 4. Backfill embeddings manually (one pass) or as a poller.
trove embed --volume myvol --meta … --versions-db … [--watch 30]
```

## Test coverage (e2e)

Run with `cargo test --features mount` (needs the built libjfs + `juicefs`
binary + `/dev/fuse`, a local Supabase stack, R2/OpenAI creds in env). Default
`cargo test` runs the native-dep-free core.

| Test binary | Proves (end-to-end, against the real stack) |
|---|---|
| `tests/check.rs` | Schema-on-write validation over real stores. |
| `tests/jfs.rs` | libjfs FFI round-trips; `jfs_clone` is a true COW snapshot (overwrite-after-clone keeps old bytes); **concurrency** — parallel ops on distinct files are safe + data-correct, concurrent readers consistent. |
| `tests/version.rs` | The version chain in Postgres: monotonic revs, parent links, dedup, `blob_hash_at`, pending-embedding query. |
| `tests/versioning.rs` | History accumulates across edits and **every revision's exact bytes are recoverable via `cat`** (COW didn't clobber old versions); identical content dedups to one clone. |
| `tests/mount.rs` | Full FUSE stack through the kernel: read/write, the validation gate (reject + `.errors` sidecar, commit valid), a committed write becomes a version (clone + chain row), and **a committed write self-triggers embedding** via the background thread. |
| `tests/embed.rs` | Real OpenAI embed of a blob → one 3072-dim vector per header chunk in `blob_chunks`, correct headings. |
| `src/embed.rs` units | The markdown chunker (heading splits, preamble, byte ranges, edge cases). |

## Known limitations / not yet built

- **`trove search` is not built.** Vectors are produced and indexed, but there's
  no query command yet (cosine over `blob_chunks` → join to path + heading). This
  is the next payoff.
- **Only schema-governed files are versioned/embedded.** Ungoverned and binary
  files stream through (`PassThrough`) and bypass the commit-versioning branch.
  For a knowledge substrate you probably want *all text* files versioned; that's
  a routing change (buffer + version text even when ungoverned; pass-through only
  true binaries) and an open decision.
- **`trove log/diff/cat@rev/restore` CLI** — the primitives exist
  (`versioning::cat`, `VersionStore::log/blob_hash_at`); the user-facing commands
  aren't wired yet.
- **Fire-on-commit is an in-process channel**, not a DB `NOTIFY`. Fine for a
  single mount; a multi-mount fleet would want the embed worker decoupled.
- **Images/PDFs deferred.** The plan when added: vision-model → caption + OCR →
  embed as text, staying in the one vector space.
- **`/.trove/` is visible** in the mounted tree (cosmetic; hide from `readdir`).
- **Multi-tenant ACLs** are out of scope (the project is single-tenant per the
  strategy pivot).
