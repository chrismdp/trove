# Proposal: per-folder vaults (`trove init` / `trove clone`)

Status: **settled — ready to build** · supersedes the global-config / `trove
install` / `trove clone` model · no migration path (no users yet)

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

## The model: one `trove init`, folder-aware

There's a single command. You `cd` into a folder and run `trove init`; it derives
everything from the folder name and figures out whether this is a *new* vault or
an *existing* one to attach to. No separate `clone`.

```
cd notes && trove init     # adopt the existing "notes" vault, or create it
```

**Names derived from the folder (`<name>` = folder basename):**
- schema → `trove_<name>`   (underscore form — `my-notes` → `trove_my_notes`)
- bucket → `trove-<name>`   (hyphen form — `my-notes` → `trove-my-notes`; S3 forbids `_`)

**`trove init` resolves creds, then probes for those two resources:**

1. Resolve the **DB URL** and **R2 creds** — from the shared cred store / env if
   present, else prompt (and save them shared).
2. Probe: does schema `trove_<name>` exist (a valid trove vault) **and** does
   bucket `trove-<name>` exist (the user must have created it — trove doesn't)?
   - **Schema + non-empty bucket, consistent** → existing vault. *"Found vault
     `notes` — attach to it? [Y/n]"* → mount. (The old `clone`, folded in — fast
     on a second machine: same folder name + creds, one confirm.)
   - **Empty bucket, no schema** → new vault. Create the schema, migrate, format,
     mount. (trove does **not** create the bucket — see below.)
   - **Bucket missing** → *"create bucket `trove-notes` in your R2 dashboard
     (trove uses the whole bucket), then re-run."*
   - **Other mismatch** (schema without bucket, non-empty bucket without schema)
     → conflict error: rename the folder, or clear the stray resource.

The content is a **live FUSE projection** (decision (P)) mounted at the cwd; the
folder *is* the vault. Other commands (`search`, `log`, `doctor`, `embed`,
`backup`) resolve the vault from the cwd. `trove install` and `trove clone` are
both removed.

### Decided: trove validates the bucket, the user creates it

- **trove does not create buckets.** The user creates `trove-<name>` once in the
  dashboard. This keeps the R2 token at plain **Object Read & Write** (no admin
  scope) and means trove's S3 client only needs **HeadBucket / ListObjects**
  (existence + empty/non-empty), never `CreateBucket`. trove still *needs* that
  read-only S3 client (libjfs neither lists nor creates buckets) — it's what the
  present/empty validation requires.
- **The R2 endpoint is a shared input** — `https://<account>.r2.cloudflarestorage.com`,
  given once with the creds (we can't derive the account id from the access
  key). trove appends `trove-<name>`.

### Name normalization (decided)

S3 buckets allow only `-`; Postgres schemas effectively allow only `_` (a `-`
needs the identifier quoted everywhere). So instead of restricting the input,
**normalize**: treat `-`/`_`/case as one word-break in the volume name, then emit
each target's native separator — bucket `trove-<name>` (`-`), schema
`trove_<name>` (`_`). Folder `my-notes` *and* `my_notes` both work and resolve to
the same vault (deterministic, so attach matches). Reject only genuinely-invalid
characters (spaces, dots, other punctuation) with a re-prompt.

## Decision (P): live FUSE projection, not a git working copy

A vault's content (files, schemas, version history, embeddings) lives in the DB
+ bucket and is surfaced by the mount — **nothing of it is on local disk.** The
alternative considered, **(W)** a git-style local working copy synced to the
backend, was rejected: it abandons the live-FUSE thesis and needs a whole
push/pull/merge/conflict engine — a different product. (P) keeps trove what it
is, and "schemas travel" falls out for free (below).

## Local state — outside the projection, creds shared across volumes

Local state is kept **outside** the projected folder (so the mount can't shadow
it and the write pipeline can't sweep it into the backend), and **split into
shared credentials and per-volume config** — because one DB + one R2 credential
typically backs *many* volumes (see Fleet, below), and the creds shouldn't be
duplicated per volume.

`~/.config/trove/credentials.toml` — **shared, machine-wide, never synced** (`chmod 600`):

```
versions_db = "postgres://…"      # embeds the DB password
# R2 creds resolved from env first (R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY),
# else from here — kept for op/1password workflows:
# r2_access_key_id = "…"
# r2_secret_access_key = "…"
```

`~/.config/trove/volumes/<volume>.toml` — **per-volume, references the creds:**

```
bucket     = "https://<account>.r2.cloudflarestorage.com/<volume>"
schema     = "trove_<volume>"
mountpoint = "/home/you/<volume>"
cache      = "/tmp/trove-cache"
```

Rotation is one-place (the credentials file / env). Nothing lives in the vault
folder, so there's nothing to git-ignore and nothing for the write pipeline to
exclude — the secret isn't reachable from the projection.

**"Separate" is about the *backend*, not local disk.** The rule is: never
co-locate the two creds in each other's backend store (DB URL never in the
bucket; R2 keys never in the DB) — that's the blast-radius property. Holding
both in the operator's local `~/.config` cred store is fine and expected.

**Schemas travel as vault content.** The JSON-Schema type registry lives *in*
the vault (`<vault>/.types/`), versioned like any file. A `clone` pulls it down
for free, and the mount loads it at startup to build the validation gate — no
separate sync, no local copy to keep in step. This is what "we'd need the
schemas everywhere" resolves to under (P).

## Fleet: one DB + one R2 credential → many volumes

The substrate already supports this and it's a first-class case, not an edge:

- **One database, many schemas** — each volume is its own `trove_<volume>`
  schema (already shipped).
- **One R2 credential, many buckets** — an R2 API token reaches every bucket in
  the account, so the *same* creds format and mount any number of volumes, each
  in its **own bucket**.

So a machine holds **one shared credential set** and **N per-volume configs**
(schema + bucket + mountpoint). This is exactly why local state is split as
above. Concretely, after the first vault exists, a second is just:

```
cd ~/projectB && trove init           # reuses the shared DB URL + R2 creds;
                                       # only asks for the bucket
```

No re-entering creds, no duplication, one-place rotation.

JuiceFS strips the object-store secret key from the metadata by default
(`--keep-secret-key` would persist it; we won't). The two credentials stay
independent and are never co-located:

| Secret | Where it lives | Why |
|---|---|---|
| DB URL (+ password) | `~/.config/trove/credentials.toml` / env — shared, local only | the handle to the vault |
| R2 access key + secret | environment / keychain (or the shared creds file) | account-level; reaches every bucket |

**Property preserved:** a database leak yields metadata + version chain +
embeddings, but **not the file bytes** — those need the R2 secret, which lives
in neither store. Storing either secret inside the other's store (DB URL in the
bucket, or R2 keys in the DB) was rejected: it collapses that blast-radius
separation, and object stores go accidentally-public more often than DBs. The
shipped schema isolation (`trove_<volume>`, off Supabase's anon API) keeps
`jfs_setting` and our tables unreadable by the anon key regardless.

## `trove init` — step by step

Inputs are minimal: the **DB URL** and **R2 creds** (+ R2 endpoint), read from
the shared cred store / env if present, else prompted once and saved shared. The
volume name is the folder basename (validated). Everything else is derived:
schema `trove_<name>`, bucket `trove-<name>`, `meta` = the DB URL, `cache` =
default, store = the folder.

1. Validate the folder name → clean token; re-prompt if not. Confirm the cwd is
   **empty** (else: "run `trove import` to adopt existing files").
2. Resolve creds; **validate as you go, plain errors:** DB URL → connect now
   (bad host/creds fail immediately, not at migration); R2 creds + endpoint →
   reach the object store.
3. **Probe** schema `trove_<name>` and bucket `trove-<name>`:
   - **both present & consistent** → attach (confirm) → mount. *(former `clone`)*
   - **neither** → create the bucket + schema, run the migration, format. *(former `init`)*
   - **partial / mismatch** → conflict error (rename the folder, or remove the
     stray bucket/schema).
4. Write `~/.config/trove/volumes/<name>.toml`, **mount at the cwd**.

## Bucket input — be generous, normalize

Accept any common form and canonicalize to what libjfs wants (same approach as
the shipped `postgresql://` → `postgres://` scheme fix):

- `https://<account>.r2.cloudflarestorage.com/<bucket>` (path-style — JuiceFS's
  documented R2 form)
- `https://<bucket>.<account>.r2.cloudflarestorage.com` (virtual-hosted)
- bare `<bucket>` + account id, with/without scheme

## Volume-name handling — validate, don't silently transform

At the prompt, **normalize** rather than restrict (see *Name normalization*):
`-`/`_`/case are one word-break; reject only genuinely-invalid characters
(spaces, dots, other punctuation) with a re-prompt. Echo both derived names
inline (`→ schema trove_notes · bucket trove-notes`). `trove` is a **valid**
name; the `trove → trove_default` special-case is dropped.

## Unchanged substrate

Untouched: per-volume schema isolation, the embedded migration, `vector` in a
shared schema, and the validate → COW-version → embed write pipeline. The new
surface is a config/UX skin over the plumbing validated on v0.2.x, **plus** a
read-only S3 client (new — for bucket validation).

## All decisions settled

1. Live FUSE projection (P), not a working copy (W).
2. **One command, `trove init`**, run inside a folder: derives names from the
   folder, probes the backend, attaches to an existing vault or creates a new
   one. `trove install` and `trove clone` both removed.
3. Names derived from the folder, separators normalized: schema `trove_<name>`
   (`_`), bucket `trove-<name>` (`-`); `my-notes` and `my_notes` resolve alike.
4. **The user creates the bucket; trove only validates it** (present + empty/
   non-empty). Keeps the R2 token at Object R/W; S3 client is HeadBucket /
   ListObjects only, no `CreateBucket`.
5. Local state split: **shared** creds (`~/.config/trove/credentials.toml` / env,
   incl. the R2 endpoint) + **per-volume** config
   (`~/.config/trove/volumes/<name>.toml`); never synced; schemas travel as vault
   content; secrets never co-located in the backend. One DB + one R2 cred → many
   volumes.
6. Minimal inputs (DB URL, R2 creds + endpoint, all shared); validate each at
   entry; mount implicitly at the cwd.
7. Volume names normalized/validated; `trove` allowed; no `trove_default`.

## Build order (suggested)

1. **Read-only S3 client** (sigv4): HeadBucket / ListObjects — for the
   present/empty bucket validation. New component (libjfs can't list buckets).
2. Config refactor: shared creds + per-volume config; resolve the vault from the
   cwd; remove the global `config.toml`.
3. `trove init` — folder-name normalize/validate → resolve+validate creds →
   probe (schema × bucket) → attach-or-create (create schema + migrate + format;
   bucket must pre-exist) → mount at cwd.
4. Move the type registry to `<vault>/.types/` as vault content; load at mount.
5. Retire `trove install` / `trove clone`; rewrite the docs.
