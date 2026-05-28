# `trove backup`

Write a local mirror of every committed file. Walks the version chain so
**every historical revision is on disk**, not just the live tree. Incremental
by default — re-running skips revisions whose bytes already exist at the
destination with a matching hash, so a nightly cron is cheap.

```
trove backup --dest /var/backups/trove
```

```
trove backup: 1,234 path(s) walked → /var/backups/trove; 3,902 rev(s) written (47.1 MB), 0 unchanged
```

A second run with no new commits:

```
trove backup: 1,234 path(s) walked → /var/backups/trove; 0 rev(s) written (0 B), 3,902 unchanged
```

## What it writes

Two layouts. Both are deterministic and content-addressed under the hood
(an existing file with a matching sha256 is left alone).

### `--layout by-path` (default)

The operator's mental model: a live tree, with history beside it.

```
/var/backups/trove/
  people/
    alice.md                 ← latest rev (rev 3)
    bob.md                   ← latest rev (rev 1)
  .versions/
    people/
      alice.md/
        rev-1
        rev-2
        rev-3
      bob.md/
        rev-1
```

`<dest>/<path>` matches what the mount projects. `<dest>/.versions/<path>/rev-<N>`
holds the COW snapshot bytes for every revision. The history sidecar is a
real directory — you can `grep -r` it like anything else.

### `--layout by-rev`

One full tree per revision. Useful when you want point-in-time snapshots you
can diff with `diff -r`.

```
/var/backups/trove/
  rev-1/
    people/
      alice.md
      bob.md
  rev-2/
    people/
      alice.md
  rev-3/
    people/
      alice.md
```

No live-tree copy; the latest rev is just whichever `rev-N` is highest.

## Behaviour

- **Incremental.** Before writing a revision, Trove hashes whatever is
  already at the destination path and compares against `file_versions.blob_hash`.
  Match → skip. Mismatch (or missing) → write. The report's `unchanged`
  count is the number of revisions Trove decided not to touch.
- **Atomic writes.** Each revision is written to `<target>.trove-backup.tmp`
  then renamed into place, so a crash mid-write won't leave a half-file
  that would pass a (wrong) hash check on the next run.
- **Best-effort.** Revisions whose COW clone is missing from this volume
  (e.g. the version chain is stitched to a different volume) are skipped
  rather than failing the whole run. Run `trove doctor` if you see fewer
  written-or-skipped revisions than you expect.
- **`--dry-run`.** Walks the chain and counts what *would* be written.
  Useful for sizing a destination before committing to it.

## A complete backup strategy

`trove backup` is the **third leg** of a belt-and-braces strategy. The other
two are already there:

1. **Bucket replication.** R2 (or S3, or whichever object store you
   formatted JuiceFS against) holds the actual blob bytes. Turn on
   versioning + a lifecycle replication rule to a second region. This is
   the cheapest and most durable copy.
2. **`pg_dump` of the version DB.** Postgres holds the version chain
   (`file_versions`, `blobs`) and the embeddings (`blob_chunks`). A nightly
   `pg_dump` lets you rebuild Trove from the bucket alone — without the
   chain you'd have content-addressed bytes but no idea which path each
   one belongs to.
3. **`trove backup` to a local mirror.** A plain tree you can `rsync`,
   `tar`, snapshot, or copy to an external drive with normal tools. With
   `--layout by-path` it's also what an operator might want to *read*
   without standing up a Trove mount — open it in any editor.

If the bucket goes dark and you've kept (2) + (3), the mirror is your
working copy. If the DB goes dark, the bucket has every blob; the chain
re-derives from `--layout by-rev` if you cared to keep one.

## Configure once, cron it

The install prompt asks for an optional `backup mirror directory`. Set it
and `trove backup` runs with no flags:

```bash
# during `trove install`
backup mirror directory [optional — write a local copy of every committed file]: /var/backups/trove
```

Then a nightly cron one-liner:

```cron
30 3 * * *  trove backup
```

The first run mirrors everything. Subsequent runs are incremental — only
new revisions are written. A repository with no new commits writes zero
bytes.

## How it works

For each path in `file_versions`, Trove walks the log oldest→newest. For
each revision it reads the bytes through the same `versioning::cat`
primitive `trove cat <path>@<rev>` uses — the COW clone in
`/.trove/versions/<hash>` inside the JuiceFS volume. Then either writes or
skips-unchanged based on the destination's sha256.

The walk is single-threaded and IO-bound; for a few thousand paths with
a handful of revs each it completes in seconds. The slow case is a fresh
mirror against a big DB — pace yourself with `--dry-run` first to see how
many revisions and bytes you're about to spool.
