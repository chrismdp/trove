# Troubleshooting

Symptoms and what they usually mean.

## `trove check` says everything is "untyped"

Your `.types/<name>.json` schemas don't have `globs` arrays, or the globs
don't match.

Check:

- Is `<store>/.types/` populated?
- Do the globs use `**.md` (recursive) rather than `*.md` (root only)?
  `globset` is configured with `literal_separator(true)`, so `*` does not
  cross `/`.
- Does each schema's `type.const` (if set) match the `type:` field in
  the files you expect it to govern?

```bash
trove check ./store          # without --quiet, see every "ok" / "FAIL"
```

If a file you expect to be governed is missing from the output entirely,
it's not being walked — check the extension (`.md` only) and that it's
not under a dotfile-prefixed directory (`.git/`, `.obsidian/`, etc. are
skipped).

## `EINVAL` on every write to the mount

The validation gate is rejecting everything. Two common causes:

1. **Missing `type` field on files governed by a type-pinning schema.** A
   schema with `properties.type.const: person` won't claim a typeless
   file (deliberate), but a schema *without* a const that globs the path
   will. If both exist, the type-pinning one needs `type: person` in the
   file.
2. **A schema is itself invalid.** `validate.rs` catches this and reports
   it as a violation in the sidecar:
   `schema "person" is itself invalid: …`. Fix the schema.

`cat <path>.errors` always has the verdict.

## "JuiceFS backend" check fails

`trove doctor` says the backend is bad. In order of likelihood:

- **Volume not formatted.** Run `juicefs format` once (see
  [Running](/docs/running)).
- **Postgres unreachable from the trove process.** Test with `psql
  $VERSIONS_DB`. Container networking is the usual culprit.
- **R2 credentials wrong.** `juicefs format` succeeded but `jfs_init`
  fails to fetch a list of chunks. Check `R2_ACCESS_KEY_ID` and
  `R2_SECRET_ACCESS_KEY` are exported in the trove process's
  environment (not just your shell).
- **`libjfs-amd64.so` not found.** The build embeds an rpath from
  `LIBJFS_DIR`; if you've moved the `.so`, rebuild with the new path.

## Mount comes up but every read returns ENOENT

Two possibilities:

1. **The volume is empty.** It works; you just haven't written anything
   yet. Check with `juicefs status <meta-url>` if you're not sure
   whether the metadata is talking to the same volume as the binary.
2. **Path mismatch.** JuiceFS is path-based, FUSE is inode-based. The
   `ino_to_path` map in the mount is what bridges them. If something
   gets out of sync (rare; usually after a process crash mid-rename),
   restart the mount.

## `OPENAI_API_KEY not set` on mount

You're seeing this since the v0.2 change that fails fast at mount time.
Two fixes:

- **Export the key** in the process's environment, OR
- **Pass `--no-embed`** to mount without embedding. The mount still
  works for validation and history; search just won't get fresh vectors.

## Embeddings are stale / search finds nothing

The embed thread runs in-process. If something interrupted it:

```bash
trove embed                 # one sweep, exits when caught up
trove embed --watch 30      # loop forever, sweep every 30s
```

Both query `pending_embedding_hashes` (blobs with no rows in
`blob_chunks`). If `trove embed` reports `embedded 0 blob(s)` and your
new file still doesn't show up in search, check:

- Is the file UTF-8? Sentinel-embedded files (binary/non-UTF-8) are
  filtered out of search results. (`file <path>` will tell you.)
- Did the file actually commit? `trove log <path>` should show the
  revision. If not, the validation gate rejected the write.

## Search returns hits from old content

Cosine search is over `blob_chunks.embedding`. Two blob hashes from
different revisions of the same path both have chunks; the lateral join
in `search_chunks` picks the **highest-rev path that references each
blob**. So:

- If you delete a path's chunks but the same content is referenced by
  another path, you'll still get hits.
- If you change a file and the new content has been embedded but not
  the old, both will show as candidates and the more relevant one wins
  on score.

If you really need to clear history-embedded chunks, that's a `delete
from blob_chunks where blob_hash in (...)` job; not exposed as a CLI
yet.

## `trove server` won't bind

```
binding 127.0.0.1:38080: Address already in use
```

Something else has the port. `lsof -i :38080` will tell you. Pass
`--port` to pick another.

## `trove search` is slow on the first hit, fast after

Postgres needs to load the HNSW index into shared buffers on the first
query. Subsequent queries are fast. Nothing to fix — that's how
pgvector works.

## "FUSE: failed to mount" / "permission denied"

User-space FUSE mounts need either:

- The user in the `fuse` group, OR
- `user_allow_other` in `/etc/fuse.conf` (less common for personal use),
  OR
- Root.

On modern Linux distributions just installing `fuse3` and being in the
`fuse` group is enough.

## The mount process crashes the first time

Almost always libjfs's config parser panicking on an empty rate-limit
field. Trove passes literal `"0"` strings for all of them — if you've
forked `jfs.rs` and removed any, restore them. (Process crash, no Rust
backtrace, mention of `ParseBytesStr` or `ParseMbpsStr` in the libjfs
log → that's the smell.)

## `cargo test` works locally but fails in CI

The mount-feature tests need:

- libjfs `.so` available (`LIBJFS_DIR`)
- `juicefs` binary on PATH (for `juicefs format` in test setup)
- `/dev/fuse` accessible (most CI containers don't expose it — needs
  `--device=/dev/fuse --cap-add SYS_ADMIN` for docker)
- A local Postgres with pgvector
- R2 / OpenAI creds

The default `cargo test` (no `--features mount`) only runs the
native-dep-free core — it should work in any CI.

Next: [Contributing →](/docs/contributing)
