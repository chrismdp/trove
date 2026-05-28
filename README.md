# trove

**A filesystem that talks back.** Coordination, validation and visibility pushed
down to the filesystem layer, so any agent — or human — gets typed, validated,
schema-checked shared state with zero per-agent integration. You don't need MCP
to coordinate agents; you need a filesystem that talks back.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/chrismdp/trove/main/install.sh | sh
```

Linux + macOS, amd64 + arm64. Pulls the latest pre-built binary (and the
matching `libjfs` shared library) from the GitHub Release, verifies the
sha256, installs to `~/.local/share/trove/<version>/`, and symlinks
`trove` into `~/.local/bin/`.

To pin a specific version:

```sh
curl -fsSL https://raw.githubusercontent.com/chrismdp/trove/main/install.sh | sh -s -- --version v0.2.0
```

Or build from source (needs a recent Rust toolchain and Go ≥ 1.22 to
build `libjfs`, plus running `./libjfs/build.sh` before `cargo install`):

```sh
./libjfs/build.sh
cargo install --git https://github.com/chrismdp/trove --features mount
```

## Status — v0.1 (single-tenant)

Built and tested end-to-end: **schema-on-write validation**, a **FUSE mount**
over JuiceFS (R2 + Postgres), **copy-on-write version history** (via `jfs_clone`,
zero byte duplication), and **semantic-search embeddings** that self-trigger on
write. One write flows: validate → COW-clone a version → record the chain →
embed. See **[ARCHITECTURE.md](ARCHITECTURE.md)** for the full design, schema,
how to run it, and the e2e test map.

Not yet built: the `trove search` query command (vectors are produced + indexed,
just no query surface yet) and the `trove log/diff/cat/restore` history CLI.

### `trove check <store>`

Walks a store, and for every markdown file whose frontmatter declares a `type`,
validates it against the matching JSON Schema in `<store>/.types/<type>.json`.
Untyped files and types with no schema are skipped; parse failures (broken YAML,
unclosed fences, non-UTF-8) are reported regardless of schema. Non-zero exit on
any failure, so it drops straight into a pre-commit hook or CI.

```
trove check ~/vault          # validate a whole store
trove check ~/vault --quiet  # failures + summary only
```

The type registry (`.types/*.json`) is **data in the store, not code** — editing
a schema is how you migrate; writes self-heal lazily as records are touched.

## Commands

- **`trove check <store>`** — schema-on-write validation. ✅
- **`trove mount <mnt> --volume … --meta … [--types …] [--versions-db …] [--embed]`**
  — the FUSE projection. The validation gate runs on the write path (a
  schema-violating `fsync` returns `EINVAL` + a `.errors` sidecar); `--versions-db`
  turns on COW version history; `--embed` self-triggers embedding on each commit. ✅
- **`trove embed --volume … --meta … --versions-db … [--watch SECS]`** — backfill /
  poll embeddings for any un-embedded blobs. ✅
- **`trove search`** — semantic query over the embeddings. _next_

See **[ARCHITECTURE.md](ARCHITECTURE.md)** for how it all fits together.

## Licence

Functional Source License (FSL-1.1, future-converts to Apache 2.0). See
[LICENSE.md](LICENSE.md). Internal/commercial use permitted; competing hosting
services reserved for two years, then auto-converts to OSS.
