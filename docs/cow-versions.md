# How COW versioning works

The mechanism that gives Trove "every revision's exact bytes" history
without paying for it in storage.

## The kernel idea: a JuiceFS clone is metadata-only

When you call `jfs_clone(src, dst)`:

1. JuiceFS reads the chunk-id list for `src` from Postgres.
2. It writes new metadata for `dst` pointing at the **same chunk-ids**.
3. It bumps a refcount on each chunk.

**No bytes are copied.** The R2 objects holding those chunks are
unchanged.

Now overwrite `src`. JuiceFS:

1. Allocates new chunks for the new content.
2. Writes them to R2.
3. Updates `src`'s metadata to point at the new chunks.
4. The **old chunks remain** — `dst` still references them, refcount > 0.

So `dst` keeps the original bytes; `src` has the new bytes; the two share
nothing now. **Storage cost of the snapshot: zero bytes copied**, just the
metadata rows.

## What Trove does with this

For every validated commit:

```rust
let hash = sha256_hex(&buf);
fs.clone_file(path, "/.trove/versions/<hash>", preserve=true)?;
versions.record_meta(path, &hash, size, author)?;
```

- The snapshot path is **content-addressed**: `/.trove/versions/<sha256>`.
- The destination already existing means we have *that content* archived;
  we skip the clone (dedup).
- The Postgres row links `(path, rev)` to `hash`. History is reconstructed
  by joining the chain row to the clone path.

## Storage analysis: a worked example

A 100 KiB markdown file edited 20 times. Each edit changes ~1 paragraph
(~500 bytes).

| | Naive copy | Git LFS-ish | Trove (COW) |
|---|---|---|---|
| Storage per snapshot | 100 KiB | ~500 bytes (delta) | ~zero (shared chunks) |
| Storage for 20 snapshots | 2 MiB | ~10 KiB | **~few KiB metadata** |
| Restore cost | direct | replay deltas | direct |

JuiceFS chunks at 4 MiB by default, so a 100 KiB file is one chunk. Edits
that touch any byte cause that chunk to be reallocated; the new chunk
goes to R2, the old chunk stays for the clone. So **for sub-chunk files,
COW is one R2 object per unique state**, not per edit.

For a multi-MiB file, COW gets better the more localised your edits are
— a typo fix in a 50 MiB doc clones 99% of chunks and reallocates one.

## Dedup is free

Content-addressing means **identical content → identical hash → identical
snapshot path**. Saving a file unchanged is a no-op for the version
archive: `fs.exists(dst)` is true, the clone is skipped, only a new chain
row is inserted (and that row points at the existing blob).

This matters more than you'd think:

- Restoring a previous rev re-saves its bytes → same hash → no new
  snapshot, just a new chain row.
- Two different paths with the same content (cross-references, copied
  templates) share one snapshot blob.
- An agent that "saves to be safe" without actually changing anything
  costs nothing.

## Why content-addressed?

Two alternatives:

1. **Per-rev paths** — `/.trove/versions/<path>/<rev>`. Simple and obvious.
2. **Content-addressed** — `/.trove/versions/<hash>`. What Trove does.

We chose content-addressed because:

- Dedup falls out automatically.
- The clone path is a function of content, so re-saves are idempotent.
- The chain row is the only place the `(path, rev) → hash` mapping lives.
  Pruning a rev is a Postgres delete; the blob lingers if anyone else
  references it, gets garbage-collected when nobody does.

The cost is that you can't `ls /.trove/versions/` and see what file each
clone belongs to — you need the chain row to make sense of it. A small
price.

## Walking through edits

File `/notes.md` with content `A`, hash `aaa…`:

- **Edit 1**: content `B`, hash `bbb…`.
  - Clone `/notes.md` → `/.trove/versions/bbb…`. (Wait — `/notes.md` now
    contains `B`, so its bytes are `B`'s bytes. The clone snapshots `B`.)
  - But where's `A`?

This is the subtle bit. The clone snapshots **the live file after the
write**. The first version is the *first* version, recorded on the first
write. Before that write, there's no history. So:

- **First write** (content `A`): `fs.write_all(path, A)` → live tree has
  `A`. Then `clone_file(path, /.trove/versions/aaa…)` → snapshot of `A`.
  Chain row: `(path=/notes.md, rev=1, hash=aaa…)`.
- **Second write** (content `B`): `fs.write_all(path, B)` → live tree now
  has `B` (old chunks → preserved by COW, because `/.trove/versions/aaa…`
  still references them). Then `clone_file(path, /.trove/versions/bbb…)`
  → snapshot of `B`. Chain row: `(path, rev=2, hash=bbb…)`.
- **`cat path --rev 1`** → lookup `(path, 1)` → `hash=aaa…` → read
  `/.trove/versions/aaa…` → bytes `A`.

The mental model: **the live tree is the working copy; the version
archive is a content-addressed pile of immutable snapshots, indexed by
the chain.**

## What COW doesn't give you

- **Branching**: there's no branch concept. The chain is linear per path.
  If you want branches, use git on top.
- **Authored diffs**: history records bytes, not author intent.
- **Diff compression**: storage is per-chunk, not per-delta. Two files
  with 99% identical content but different chunk boundaries (e.g. one
  was rewritten from scratch) won't dedup chunks.

## Pruning

Not built in v0.1. A future `trove gc` would:

1. Find blobs in `/.trove/versions/` that have no `file_versions` row.
2. `fs.unlink` them.

The Postgres-side cascade (deleting a chain row) doesn't auto-delete the
blob, because two paths might share a content hash (dedup). So GC is a
graph walk, not a DB cascade.

For now, the archive grows. A vault with hundreds of files and a few
edits per file each day is small; storage isn't the pressing constraint.

Next: [Self-triggering embeddings →](/docs/embedding-pipeline)
