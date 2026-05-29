# Proposal: per-folder vaults (`trove init` / `trove clone`)

Status: **settled — ready to build** · supersedes the global-config / `trove
install` model · no migration path (no users yet)

## Problem

The current model has one global `~/.config/trove/config.toml` and a `trove
install` flow that:

- asks for a vault **path** as config (awkward — a machine may want several
  vaults);
- assumes **one volume per machine**;
- silently transforms inputs (volume name → schema, bucket forms, `postgresql://`
  → `postgres://`) with little feedback;
- validates late (a bad DB URL or missing bucket surfaces only at the migration
  / format step).

The install fought the first real user at nearly every step. The fix is a more
obvious, git-shaped model.

## The model: git-shaped, but the content is a live projection

```
trove init                 # in an empty dir → create a NEW vault, mount here
trove clone <db-url>       # → make ./<volume>/ from the DB, mount there
```

Like git: `init` adopts the directory you're in; `clone <handle>` creates a
named subdirectory. The one difference from git is **decision (P)**: the vault
content isn't stored locally — it's a live FUSE projection of the DB + bucket
(trove's "filesystem that talks back" thesis). So there are no tracked local
files; the folder *is* the mount.

- **`trove init`** — run inside an **empty** folder. trove formats a new volume,
  mounts it **at the cwd** (the folder becomes the live vault), and names the
  volume after the folder basename (validated; re-prompt if it isn't a clean
  identifier).
- **`trove clone <db-url>`** — the **DB URL is the handle.** A bare volume name
  can't locate the database on a fresh machine (that's the secrets-separate
  decision), so clone takes the URL, connects, reads the volume name + bucket +
  schema back from the DB, creates **`./<volume>/`** (or a given dir) and mounts
  there. If the DB hosts several volumes, `--volume <name>` selects one.
- Other commands (`search`, `log`, `doctor`, `embed`, `backup`) resolve the
  vault from the cwd's mountpoint → its local config. `trove install` is removed.

Why `init` mounts at cwd but `clone` makes a subdir: on `init` you've already
chosen/created the folder and `cd`'d in; on `clone` you don't yet know the name
— it comes back from the DB and names the directory, exactly like `git clone`.

## Decision (P): live FUSE projection, not a git working copy

A vault's content (files, schemas, version history, embeddings) lives in the DB
+ bucket and is surfaced by the mount — **nothing of it is on local disk.** The
alternative considered, **(W)** a git-style local working copy synced to the
backend, was rejected: it abandons the live-FUSE thesis and needs a whole
push/pull/merge/conflict engine — a different product. (P) keeps trove what it
is, and "schemas travel" falls out for free (below).

## Local state — outside the projection

The only local artifact is this machine's connection state, kept **outside** the
projected folder so the mount can't shadow it and the write pipeline can't sweep
it into the backend:

`~/.config/trove/<volume>/env` — **local, never synced:**

```
versions_db = "postgres://…"    # embeds the DB password → secret, local only
bucket      = "https://<account>.r2.cloudflarestorage.com/<volume>"
schema      = "trove_<volume>"
cache       = "/tmp/trove-cache"
mountpoint  = "/home/you/<volume>"
```

Because it lives under `~/.config`, there's nothing in the vault folder to
git-ignore and nothing for the write pipeline to hard-exclude — the secret
simply isn't reachable from the projection. R2 creds stay in the environment
(`R2_ACCESS_KEY_ID` / `R2_SECRET_ACCESS_KEY`), never written here.

**Schemas travel as vault content.** The JSON-Schema type registry lives *in*
the vault (`<vault>/.types/`), versioned like any file. A `clone` pulls it down
for free, and the mount loads it at startup to build the validation gate — no
separate sync, no local copy to keep in step. This is what "we'd need the
schemas everywhere" resolves to under (P).

## Secrets stay separate (decided)

JuiceFS strips the object-store secret key from the metadata by default
(`--keep-secret-key` would persist it; we won't). The two credentials stay
independent and are never co-located:

| Secret | Where it lives | Why |
|---|---|---|
| DB URL (+ password) | `~/.config/trove/<volume>/env`, local only; passed as the arg to `clone` | the handle to the vault |
| R2 access key + secret | environment / keychain | supplied at runtime, never stored |

**Property preserved:** a database leak yields metadata + version chain +
embeddings, but **not the file bytes** — those need the R2 secret, which lives
in neither store. Storing either secret inside the other's store (DB URL in the
bucket, or R2 keys in the DB) was rejected: it collapses that blast-radius
separation, and object stores go accidentally-public more often than DBs. The
shipped schema isolation (`trove_<volume>`, off Supabase's anon API) keeps
`jfs_setting` and our tables unreadable by the anon key regardless.

## `trove init`

**Minimal inputs only:** **DB URL**, **bucket**, **R2 creds**. The volume name
defaults to the folder basename (re-prompt if it isn't a clean identifier).
Everything else is derived/defaulted, never asked: `meta` = the DB URL, `cache`
= default, store = the folder, schema = derived, backup = a separate concern.

1. Confirm the cwd is **empty** (else: "run `trove import` to adopt existing
   files").
2. **Validate as you go, fail early with plain errors:**
   - DB URL → connect now; bad host/creds fail immediately, not at migration.
   - bucket + creds → reach the object store; require it **present and EMPTY**
     ("bucket `x` isn't empty — clear it or use a fresh one"). One whole bucket =
     one volume; no prefix/subpath.
3. Create the schema, run the migration there, format the volume.
4. Write `~/.config/trove/<volume>/env`, then **mount at the cwd** (implicit).

## `trove clone <db-url>`

1. Connect to the DB (the URL is the handle). The **database is the source of
   truth** — read the volume + bucket + schema back from it (`jfs_setting` + a
   small trove config row added at init). `--volume` disambiguates a multi-volume
   DB.
2. Validate: DB connects; bucket **present and NON-empty** (data should be there).
3. Prompt/accept **R2 creds** (env/prompt) — not in the DB by design.
4. Write `~/.config/trove/<volume>/env`, create `./<volume>/`, **mount there**.

## Bucket input — be generous, normalize

Accept any common form and canonicalize to what libjfs wants (same approach as
the shipped `postgresql://` → `postgres://` scheme fix):

- `https://<account>.r2.cloudflarestorage.com/<bucket>` (path-style — JuiceFS's
  documented R2 form)
- `https://<bucket>.<account>.r2.cloudflarestorage.com` (virtual-hosted)
- bare `<bucket>` + account id, with/without scheme

## Volume-name handling — validate, don't silently transform

At the prompt, require a clean identifier (`[a-z0-9-]`). Anything else (spaces,
capitals, punctuation) is **rejected and re-prompted** with the reason — no
silent sanitizing. Echo the resulting schema inline (`→ metadata schema:
trove_notes`). `trove` is a **valid** name; the `trove → trove_default`
special-case is dropped.

## Unchanged substrate

Untouched: per-volume schema isolation, the embedded migration, `vector` in a
shared schema, and the validate → COW-version → embed write pipeline.
`init`/`clone` is a cleaner config/UX skin over the plumbing validated on v0.2.x.

## All decisions settled

1. Live FUSE projection (P), not a working copy (W).
2. `init` mounts at cwd (volume = folder name); `clone <db-url>` makes
   `./<volume>/`. `install` removed.
3. Local connection state in `~/.config/trove/<volume>/env`, never synced;
   schemas travel as vault content; secrets never co-located.
4. Minimal inputs (DB URL, bucket, R2 creds); validate each at entry; mount
   implicitly.
5. Volume names validated (reject non-clean; `trove` allowed; no `trove_default`).
6. Bucket input accepted generously and normalized; one whole bucket per volume.

## Build order (suggested)

1. Config refactor: per-volume `~/.config/trove/<volume>/env`; command resolves
   the vault from cwd mountpoint.
2. `trove init` (validate DB + empty bucket → schema + migrate + format + mount).
3. `trove clone` (DB-as-source-of-truth read-back + non-empty bucket + mount).
4. Move the type registry to `<vault>/.types/` as vault content; load at mount.
5. Retire `trove install`; update docs.
