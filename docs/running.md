# Running it end-to-end

A working setup with all four substrates: validation, FUSE mount, COW
versions, embeddings.

## Prerequisites

- **Linux** with FUSE 3 (`apt install fuse3 libfuse3-dev`).
- **Postgres** with `pgvector` (the migration `create extension if not
  exists vector;` will do it). A local Supabase is fine; any Postgres
  ≥14 with pgvector works.
- **R2** (or any S3-compatible store). For local dev, MinIO works.
- **libjfs** built from a pinned JuiceFS source. See [Contributing](/docs/contributing).
- **OPENAI_API_KEY** (or `--no-embed`).

## Step 1: configuration

```bash
trove install
```

This walks you through `~/.config/trove/config.toml`. Non-secret settings
(volume name, meta URL, cache path, R2 bucket name) live here so you
don't have to pass flags every time. **Secrets stay in the environment**
— never the config file.

You'll be asked for:

- `versions_db` — the Postgres URL (e.g.
  `postgres://postgres:postgres@127.0.0.1:54322/postgres`)
- `volume` — the JuiceFS volume name
- `meta` — usually the same as `versions_db`
- `cache` — local block-cache directory (default `/tmp/trove-cache`)
- `r2_bucket` — for `trove doctor`'s reference

## Steps 2 + 3: migrations and `juicefs format`

**`trove install` now does this for you** — it applies the embedded SQL
migration (`blobs`, `file_versions`, `blob_chunks`, pgvector, HNSW
index) and formats the JuiceFS volume on your bucket in the same run.
Safety flags:

- `--reuse` — accept an existing populated Trove DB / formatted volume.
  Use when re-running install against a backend you intend to keep.
- `--reinstall` — DROP existing Trove tables and reformat the volume.
  Destructive — every step prompts for an explicit `destroy`
  confirmation, and re-formatting against a new bucket (which would
  orphan the chunks under the old one) requires this flag.

The manual `psql -f` and `juicefs format` steps below are only needed
if `trove install` fails partway through:

```bash
# Migration (manual fallback).
psql "$VERSIONS_DB" -f supabase/migrations/<timestamp>_init_version_chain_and_embeddings.sql

# Volume format (manual fallback).
juicefs format \
    --storage s3 \
    --bucket   "https://<bucket>.<acct>.r2.cloudflarestorage.com" \
    --access-key  "$R2_ACCESS_KEY_ID" \
    --secret-key  "$R2_SECRET_ACCESS_KEY" \
    "$VERSIONS_DB" \
    myvol
```

JuiceFS metadata (`jfs_*`) and Trove's tables coexist peacefully in the
same Postgres.

## Step 4: preflight

```bash
trove doctor
```

```
trove doctor
  ✓ OPENAI_API_KEY     set (needed for embed + search)
  ✓ R2 credentials     R2_ACCESS_KEY_ID + R2_SECRET_ACCESS_KEY set
  ✓ versions DB        reachable
  ✓ pgvector           extension installed
  ✓ schema tables      blobs, file_versions, blob_chunks present
  ✓ JuiceFS backend    libjfs + volume "myvol" + object store OK

✓ all checks passed
```

If anything fails here, **fix it before mounting**. A green doctor is the
contract that the mount can succeed.

## Step 5: mount

```bash
mkdir -p /mnt/trove
trove mount /mnt/trove --types ~/vault
```

That's the full command. Everything else (volume, meta, versions_db,
cache) comes from `~/.config/trove/config.toml`. Embedding is **on by
default** (set `--no-embed` to skip).

You'll see:

```
trove: mounting at /mnt/trove (validating via /home/you/vault; versioning on; embed on)
```

The process is foreground; Ctrl-C unmounts. (Or detach with `nohup` /
`systemd` — see the systemd example below.)

## Step 6: write things

In another shell:

```bash
ls /mnt/trove                                  # empty volume
mkdir -p /mnt/trove/people
cat > /mnt/trove/people/alice.md <<EOF
---
type: person
name: Alice
dob: "1990-01-15"
---

Alice is a person.
EOF
```

If the file matches the schema, the write succeeds silently. If it
doesn't:

```bash
echo "garbage" > /mnt/trove/people/bob.md
# bash: echo: write error: Invalid argument
cat /mnt/trove/people/bob.md.errors
# (root): "type" is a required property
```

Two things to note: bash's `>` redirect checks `close()` and surfaces
the kernel's `EINVAL`, but a tool that ignores `close()`'s return value
will silently appear to succeed even though nothing persisted. **The
`.errors` sidecar is the reliable signal** — it's written before
`EINVAL` is returned, regardless of how the writing tool handles close.

## Step 7: history

```bash
trove log /people/alice.md
# trove: /people/alice.md (1 revision(s))
#   rev 1  142 bytes  —  a3f97c4b1e9d

# edit the file (any change), then:
trove log /people/alice.md
# trove: /people/alice.md (2 revision(s))
#   rev 2  168 bytes  —  d8e4a2…
#   rev 1  142 bytes  —  a3f97c…

trove cat /people/alice.md --rev 1     # original content
trove diff /people/alice.md 1 2        # unified diff
trove restore /people/alice.md 1       # writes back rev 1 as rev 3
```

`trove restore` is itself a versioned event — the restore lands as a
new rev, never overwriting silently. The timeline is always append-only.

## Step 8: search

```bash
trove search "Alice's birthday"
# trove: 1 result(s) for "Alice's birthday"
#   0.832  /people/alice.md
```

Cosine similarity (higher is closer). If you just wrote the file, the
embed thread may take a second or two; if a result is missing, run
`trove embed` to force a sync sweep.

## Step 9: the localhost viewer (optional)

```bash
trove server --port 38080
# trove server: http://127.0.0.1:38080
```

Open in a browser. File list, search box, raw content viewer. Bound to
**localhost only**; front with nginx for external access. No auth in
v0.1 (single-tenant).

## A systemd unit for the mount

```ini
# /etc/systemd/system/trove-mount.service
[Unit]
Description=Trove FUSE mount
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/trove mount /mnt/trove --types /home/cp/vault
Restart=on-failure
RestartSec=5s
User=cp
Environment="OPENAI_API_KEY=sk-..."
Environment="R2_ACCESS_KEY_ID=..."
Environment="R2_SECRET_ACCESS_KEY=..."

[Install]
WantedBy=default.target
```

`systemctl --user enable --now trove-mount.service`. The mount
auto-restarts on crash. Embeddings restart with it (the thread is
in-process).

See [`trove usage`](/docs/usage) to check storage growth at a glance — DB
size, embedding backlog, and the bucket's bill-shaped figure.

For a nightly local mirror of every file and every historical revision, see
[`trove backup`](/docs/backup).

## Tuning knobs

| Variable / flag | What it does |
|---|---|
| `TROVE_CHUNK_STRATEGY` | `heading` (split at every `#`/`##`/…) or `paragraph` (default, size-bounded clusters) |
| `--no-embed` | Disable on-commit embedding for this mount |
| `--types <dir>` | Where the `.types/` registry lives |
| `--versions-db <url>` | Override the Postgres URL (precedence: flag > env > config) |
| `TROVE_CACHE` | Local block-cache directory |
| `LIBJFS_DIR` | Where the `libjfs-amd64.so` lives (used by `build.rs`) |

Next: [Troubleshooting →](/docs/troubleshooting)
