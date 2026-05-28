# `versioning.rs` — COW snapshots

**~130 lines.** Capture a validated commit as a version, with **zero byte
duplication**, and read historical versions back.

## The public surface

```rust
pub fn record_version(
    fs: &Fs,
    versions: &mut VersionStore,
    path: &str,
    content: &[u8],
    author: Option<&str>,
) -> Result<i32>;

pub fn record_version_from_fs(
    fs: &Fs,
    versions: &mut VersionStore,
    path: &str,
    author: Option<&str>,
) -> Result<(i32, String)>;

pub fn cat(
    fs: &Fs,
    versions: &mut VersionStore,
    path: &str,
    rev: i32,
) -> Result<Option<Vec<u8>>>;
```

Two ways to snapshot, one way to read. Why two snapshot paths?

## Buffered vs streamed snapshot

- **`record_version`** takes the **bytes the caller already has in memory**.
  Used by the mount's `commit()` for governed files (we just validated
  them, so their bytes are in the handle's buffer). No re-read.

- **`record_version_from_fs`** takes only a path and **stream-hashes the
  file on disk** in 1 MiB chunks. Used by the mount's
  `version_pass_through()` for binary files we never want to hold whole in
  memory. Returns `(rev, hash)` so the caller knows what to push to embed
  (with `vec![]` for binaries — they're never embedded).

Both converge on the same internal `snapshot()` function.

## What `snapshot()` actually does

```rust
fn snapshot(fs, versions, path, hash, size, author) -> Result<i32> {
    for attempt in 0..3 {
        ensure_versions_dir(fs);            // mkdir /.trove + /.trove/versions, idempotent
        if !fs.exists(&format!("/.trove/versions/{hash}")) {
            fs.clone_file(path, &dst, true)?;  // COW clone (zero bytes copied)
        }
        match versions.record_meta(path, hash, size, author) {
            Ok(rev) => return Ok(rev),
            Err(_) => { sleep(200ms); continue; }
        }
    }
    Err(last_err)
}
```

Three things to notice:

### 1. Dedup falls out of content-addressing

Versions live at `/.trove/versions/<sha256>`. Two writes with the same
content → same hash → same clone path. The second write skips the
`clone_file` call (we check `fs.exists(dst)` first) and just records the
chain row. Storage cost of a re-saved file: **a single Postgres row**.

### 2. COW clones cost no bytes

```rust
fs.clone_file(path, &dst, true)?;
```

This is the libjfs `jfs_clone` call. JuiceFS bumps a refcount on the
source's data blocks and writes new metadata pointing at them. The actual
*bytes* are still in one place (R2). Overwriting the source later
allocates new blocks for the new content; the clone's pointers keep the
old blocks alive. **History is byte-free.**

### 3. Best-effort, with retry

The whole thing is wrapped in a retry loop (3 attempts, 200ms apart). If
all three fail, the caller (`mount.rs::commit`) **logs but does not fail
the write**. The live tree is the source of truth; the version archive is
a derived index. A versioning hiccup doesn't lose your file.

Why best-effort and not durable?

- The version clone and the chain row ride the **same backend** the live
  write just succeeded against (one JuiceFS volume, one Postgres). There's
  no independent system to be "eventually consistent" with.
- A WAL would add complexity to handle a failure mode that, in practice,
  means "Postgres is down" — in which case your live writes were failing
  too.

If you need stronger guarantees later, the natural step is to make
`commit()` await the chain row in the same transaction as the validation
write. We didn't, because the simpler invariant has not been the bug.

## `cat`: reading history

```rust
pub fn cat(fs: &Fs, versions: &mut VersionStore, path: &str, rev: i32) -> Result<Option<Vec<u8>>>;
```

1. Look up the blob hash for `(path, rev)` in `file_versions`.
2. Read `/.trove/versions/<hash>` back through libjfs.

That's it. The mount layer doesn't know about history at read time; **the
live tree is always the working copy**. Past revisions are only accessible
via `trove cat <path> --rev N` (or the `restore` command).

## Why `/.trove/versions/` is in-volume

A natural alternative: store snapshots in a *different* JuiceFS volume,
or a different R2 bucket. We don't, because:

- Same volume = same `jfs_clone` works. Cross-volume cloning would
  require copying bytes.
- Same volume = one set of credentials, one set of caches, one set of
  upgrade paths.
- Same volume = the version archive cannot get "out of sync" with the
  live tree. If a clone exists, JuiceFS knows about it.

The directory is visible in the mounted tree (this is the
"cosmetic-but-known limitation" — should be hidden from `readdir`).

## What `cat` doesn't do

- It doesn't decode anything. Bytes in, bytes out.
- It doesn't fall back to the live tree if a rev is missing. A missing rev
  returns `Ok(None)`, which the CLI surfaces as "no such revision".
- It doesn't try to reconstruct deleted files. If a path has been removed
  and the version archive has been pruned, `cat` returns `None`. (Pruning
  isn't built; the archive grows forever in v0.1.)

## Tests

- `tests/versioning.rs` — full round-trip: edit a file repeatedly, then
  verify *every revision's exact bytes are recoverable* via `cat`. This is
  the test that proves COW didn't clobber an old version.
- `tests/version.rs` — the chain side: monotonic revs, parent links,
  blob_hash_at, pending-embedding query.

Together they're the "history works end-to-end" pair.

Next: [`version.rs` — the version DB →](/docs/version)
