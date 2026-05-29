# `trove usage`

A quick read on how much room Trove is taking and whether it's growing the
way you expect. Three sections, one snapshot.

```
trove usage
```

```
trove usage

  Postgres
    database total             17.2 MB
    blobs                        1,234    rows · 412.0 KB
    file_versions                1,891    rows · 287.0 KB
    blob_chunks                  3,902    rows · 14.6 MB    (embeddings)

  JuiceFS volume (file data → bucket)
    volume total              250.0 GB
    volume used                 2.4 GB    (chunks in R2)
    volume available          247.6 GB

  Content
    distinct paths               1,234    files
    embedded blobs               1,210
    pending embedding               24    (run `trove embed`)
```

## What each row means

### Postgres

- **`database total`** — `pg_database_size(current_database())`. The whole
  DB, not just Trove's tables. Useful as a sanity check against your hosting
  bill.
- **`blobs`** — the content-addressed registry. One row per unique content
  hash. If two files contain identical bytes (or you `restore` an older
  revision), they share one row.
- **`file_versions`** — the version chain. One row per `(path, rev)`. The
  file count is here; the *path* count is in the Content section.
- **`blob_chunks`** — the embeddings table, by a wide margin the heaviest.
  3072-dimensional vectors per chunk; expect this to dwarf everything else.
  That's normal and expected.

Per-table bytes are `pg_total_relation_size()` — table + indexes + toast +
free space the table is holding. The bill-shaped figure, not just live
tuple bytes.

### JuiceFS volume

The volume's view of the bucket. `volume used` is the byte count for chunks
that JuiceFS has actually written into R2 (or S3, or whichever object store
you formatted against), after JuiceFS's own chunking and dedup. It's what
the application sees and what you'll see on your object-store bill.

Trove doesn't query the bucket directly — it asks JuiceFS, which is the
right number for "what's actually stored". If the bucket has stragglers
left over from a previous `juicefs format` (orphan chunks that no metadata
points at), they won't show here. Those need cleaning up at the bucket
level, not in Trove.

### Content

- **`distinct paths`** — the live tree's known files (a file with 5
  revisions counts once).
- **`embedded blobs`** — blobs with at least one row in `blob_chunks`.
  Sentinel rows count: an empty or binary blob with `embedding = null` is
  still "processed", so the embedder won't pick it up again.
- **`pending embedding`** — blobs with no `blob_chunks` rows. This is the
  number `trove embed` will pick up next.

## What to watch

The most useful pattern in this report is the ratio between **pending
embedding** and **embedded blobs**.

Healthy growth over a few days looks like:

```
day 1   embedded:  1,000   pending:  24
day 2   embedded:  1,047   pending:  18   ← embedder is keeping up
day 3   embedded:  1,089   pending:  31   ← caught a burst of writes, will drain
```

The warning pattern is **pending climbing, embedded flat**:

```
day 1   embedded:  1,000   pending:  24
day 2   embedded:  1,000   pending:  72
day 3   embedded:  1,000   pending: 198   ← something's wrong
```

That means the embed worker isn't running. Likely causes:

- `OPENAI_API_KEY` not set in the mount's environment
- `trove mount` started with `--no-embed`
- `trove embed --watch` isn't running and you're relying on it instead
  of on-commit embedding

`trove doctor` will tell you which.

## How it's computed

The whole snapshot runs inside one read-only Postgres transaction so the
report is internally consistent (`embedded + pending == blobs`,
`distinct_paths ≤ file_versions_rows`). The JuiceFS figures come from one
`statvfs` call. No bucket SDK call, no walk of the live tree.

If a Trove table is missing, `trove usage` errors loudly with a "schema not
migrated — run `trove init`" message rather than silently reporting 0.
Schema readiness is `trove doctor`'s job; `usage` assumes the migration has
run.
