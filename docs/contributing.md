# Contributing

How to work on Trove.

## The two builds

```bash
cargo build                         # native-dep-free core
cargo build --features mount        # full substrate (libjfs + FUSE + Postgres + OpenAI)
```

The default build produces a binary that runs `trove check`, `trove
install`, and `trove docs`. The `mount` feature adds `mount`, `embed`,
`search`, `server`, `doctor`, and the history commands (`log`, `cat`,
`diff`, `restore`).

This split matters: the **core crate has no native dependencies**, so:

- It builds on any platform with stable Rust.
- It runs on CI containers without FUSE or libjfs.
- `trove check` is a self-contained binary you can ship to anyone with a
  vault.

Don't introduce a native dep outside the `mount` feature without good
reason.

## Building libjfs

Single entry point for all platforms:

```bash
./libjfs/build.sh
```

The script fetches upstream JuiceFS at the SHA pinned at the top of
`libjfs/build.sh`, applies the patches in `libjfs/patches/`, and runs
`make`. Output lands in `libjfs/build/`. Idempotent — re-running is a
no-op unless the SHA or patches change. Use `--force` to rebuild.

`build.rs` defaults `LIBJFS_DIR` to `libjfs/build/`, so `cargo build
--features mount` works straight after. Override `LIBJFS_DIR` if you've
built libjfs somewhere else.

For the release-engineering matrix (how the four-platform tarballs are
produced), see [Packaging & release matrix](/docs/packaging).

## Running tests

```bash
# Native-dep-free unit + integration tests:
cargo test

# Full substrate tests (need libjfs, juicefs, /dev/fuse, Postgres, R2, OpenAI):
cargo test --features mount

# A single test binary:
cargo test --features mount --test mount

# A single test by name:
cargo test --features mount --test mount -- --nocapture validation_rejects_invalid_write
```

Tests that hit live services (R2, OpenAI) are integration tests; they're
opt-in via the `mount` feature flag and require the env vars
`R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`, `OPENAI_API_KEY`, plus a
running Postgres at `$VERSIONS_DB`.

See [Troubleshooting](/docs/troubleshooting) for CI tips.

## The test map

| File | What it proves |
|---|---|
| `tests/check.rs` | Schema-on-write validation works over real stores. |
| `tests/jfs.rs` | libjfs FFI round-trips; `jfs_clone` is a true COW snapshot; concurrent ops on distinct files are safe. |
| `tests/version.rs` | Version chain in Postgres: monotonic revs, parent links, dedup, blob_hash_at, pending-embedding query. |
| `tests/versioning.rs` | History accumulates across edits; every revision's exact bytes are recoverable via `cat`; identical content dedups to one clone. |
| `tests/mount.rs` | Full FUSE stack through the kernel: read/write, the validation gate, a committed write becomes a version, a committed write self-triggers embedding. |
| `tests/embed.rs` | Real OpenAI embed of a blob → 3072-dim vector per chunk in `blob_chunks`. |
| `tests/history.rs` | The CLI: `log` / `cat` / `diff` / `restore`. |
| `tests/server.rs` | `trove server` HTTP routes. |
| `tests/search.rs` | Semantic search returns the relevant chunks. |
| `tests/doctor.rs` | Preflight check outputs. |
| `src/embed.rs` units | The markdown chunker (heading splits, paragraph clustering, preamble, byte ranges). |
| `src/frontmatter.rs` units | YAML fence parsing. |
| `src/validate.rs` units | JSON Schema validation. |

## Code layout

| | |
|---|---|
| `src/main.rs` | Thin CLI shell. No logic; everything calls into `lib`. |
| `src/lib.rs` | Module declarations. |
| `src/config.rs` | `~/.config/trove/config.toml`; resolve(flag > env > config). |
| `src/frontmatter.rs` | YAML frontmatter parser. |
| `src/types.rs` | Schema registry, glob selection. |
| `src/validate.rs` | JSON Schema runner. |
| `src/commands/check.rs` | `trove check` walker. |
| `src/jfs.rs` | libjfs FFI (mount feature). |
| `src/mount.rs` | FUSE filesystem (mount feature). |
| `src/versioning.rs` | COW snapshot + cat (mount feature). |
| `src/version.rs` | Postgres client for chain + embeddings. |
| `src/embed.rs` | Chunker + OpenAI worker (mount feature). |
| `src/commands/history.rs` | `log` / `cat` / `diff` / `restore`. |
| `src/commands/server.rs` | The localhost data viewer. |
| `src/commands/doctor.rs` | Preflight checks. |
| `src/commands/docs.rs` | `trove docs` — this very documentation. |
| `docs/` | The markdown source for `trove docs`, embedded via `rust-embed`. |
| `supabase/migrations/` | The Postgres schema. |
| `build.rs` | Links libjfs when `mount` is on. |

## Code style notes

- **Comments explain why, not what.** Names + types should carry the
  what; comments are for non-obvious constraints, gotchas, and "this
  exists because of an incident on X date".
- **No silent fallbacks.** Every error has a clear path: surface it,
  log it, or sentinel it. The `dbg!`/`eprintln!` lines you see are
  deliberate observability for the best-effort paths (versioning,
  embedding) where the write must not fail.
- **No async** outside of where it's strictly necessary. The mount is
  sync; `postgres` is sync; `tiny_http` is sync. Adding tokio is a
  significant decision, not a default.
- **Tests over docs over comments.** A test is executable
  documentation. A test that proves COW versioning works (`tests/versioning.rs`)
  is better than a paragraph in a comment.

## Adding a new subcommand

1. Add a variant to `Command` in `src/main.rs`.
2. Add a match arm calling into `trove::commands::<name>::run(...)`.
3. Create `src/commands/<name>.rs` with the actual logic.
4. Add a test binary `tests/<name>.rs`.
5. Add a docs page `docs/<name>.md` and an entry in `docs/meta.toml`.

If the command needs native deps (libjfs, Postgres, OpenAI), gate it on
`#[cfg(feature = "mount")]`. If not, it lives in the core.

## Adding a new file type

1. Write `<store>/.types/<name>.json` with `globs` and a JSON Schema.
2. Optionally: `properties.type.const` to pin it to a `type:` field.
3. Run `trove check` — your existing files become governed.
4. Mount; subsequent writes go through the gate.

You don't need to touch the binary.

## Schema migration

Editing a schema is the migration. There's no `down` step:

- Writes immediately conform to the new schema (or get rejected).
- Existing files **don't** auto-update — they're only re-checked on
  next write. This is "self-heal lazily as records are touched".

For a hard cutover, run `trove check` after the schema change and fix
violations manually (or with a script).

## Reporting bugs

Open an issue. Include:

- `trove --version`
- The output of `trove doctor`
- A minimal reproducer (a store + a write + the expected vs actual
  behaviour)

For mount-related bugs especially, the `trove doctor` output is the
fastest way to rule out config issues.

## A note on scope

v0.1 is single-tenant by design (per the strategy pivot). PRs that
introduce multi-tenancy, ACLs, or remote-write APIs will be politely
declined for v0.1. The next big-ticket items are:

1. **`trove gc`** — prune unreferenced blobs in `/.trove/versions/`.
2. **Multi-mount embedding** — replace the in-process channel with a
   Postgres `LISTEN/NOTIFY` so multiple mounts can share one worker.
3. **Image/PDF support** — vision-model captions + OCR, embedded as
   text into the same vector space.

End of the walkthrough. Back to [What is Trove? →](/docs/intro)
