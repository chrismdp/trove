# Quickstart

The fastest path from a fresh machine to a validated, versioned,
searchable vault.

## Install trove

```sh
curl -fsSL https://raw.githubusercontent.com/chrismdp/trove/main/install.sh | sh
```

Linux + macOS, amd64 + arm64. Drops `trove` into `~/.local/bin/` and
the matching `libjfs` shared library beside it under
`~/.local/share/trove/<version>/`.

On macOS, `trove mount` additionally needs macFUSE
(`brew install --cask macfuse`, with a one-time KEXT approval in System
Settings — see [packaging — macOS runtime requirements](/docs/packaging#macos-runtime-requirements)).
Every other trove command works without it.

Building from source is the alternative (needs Rust + Go ≥ 1.22 for
`libjfs`):

```sh
cargo build --release --features mount
# or, without the FUSE substrate:
cargo build --release
```

See [packaging](/docs/packaging) for the libjfs build details.

## The minimum: three secrets

If you have a **Postgres server** and an **S3-compatible bucket**, you have
everything you need. Trove uses three variables:

```bash
export TROVE_VERSIONS_DB="postgres://user:pass@host:5432/dbname"  # DATABASE_URL also accepted
export R2_ACCESS_KEY_ID="..."
export R2_SECRET_ACCESS_KEY="..."
```

That's it. The Postgres URL is both the metadata engine AND the
version-chain + embeddings store — **one connection, one substrate**. The
S3 bucket holds the file data; trove chunks and stores it for you. The
keys are named `R2_*` but any S3-compatible store works (MinIO, AWS S3).

- **Running on one machine?** A local `postgres` install is fine — point
  `TROVE_VERSIONS_DB` at `127.0.0.1:5432`.
- **Running across machines?** Use a hosted Postgres (Supabase, Neon, RDS).
  Point `TROVE_VERSIONS_DB` at it. On Supabase, click **Connect → Connection
  string → Session pooler** and use that URI (host ends in
  `.pooler.supabase.com`, port 5432). Do **not** use the **Direct connection**
  (`db.<ref>.supabase.co`) — it's IPv6-only and fails with a DNS lookup error
  on most machines — nor the **Transaction pooler** (6543), whose transaction
  mode breaks the live session trove keeps.

Optional fourth: `OPENAI_API_KEY` for semantic search. Without it, pass
`--no-embed` to `trove mount` and you'll still get validation + version
history.

Don't have a Postgres database or a bucket yet? You can provision both in
one go with [Stripe Projects](/docs/stripe-projects) (optional) instead of
clicking through provider dashboards — then come back here for
`trove install`.

## Just the validator (no native deps, no Postgres)

If you only want **schema-on-write validation** over a local directory,
none of the above applies. Build the core and run it:

```bash
cargo build --release          # no `--features mount`
./target/release/trove check ./my-store
```

A "store" is any directory with a `.types/` subdirectory of JSON Schemas.
Minimal example:

```
my-store/
├── .types/
│   └── person.json
└── people/
    └── alice.md
```

`my-store/.types/person.json`:

```json
{
  "globs": ["people/**.md"],
  "type": "object",
  "required": ["type", "name"],
  "properties": {
    "type": { "const": "person" },
    "name": { "type": "string" },
    "dob":  { "type": "string", "format": "date" }
  }
}
```

`my-store/people/alice.md`:

```markdown
---
type: person
name: Alice
dob: "1990-01-15"
---

Alice is a person.
```

Run it:

```bash
trove check ./my-store
# ok   people/alice.md
# trove: 1 checked · 1 valid · 0 untyped · 0 failures
```

Break the date format and try again — you'll get a `FAIL` line and a
non-zero exit code, suitable for a pre-commit hook.

That's the **whole core of Trove**: a registry of JSON Schemas, picked by
path glob, applied to YAML frontmatter. The rest of the system builds the
*write path* that runs this same validation at filesystem-commit time.

## The full substrate (mount + history + search)

With the three secrets above exported:

```bash
# 1. Write config + provision the backend (one-time).
trove install
#    → at a terminal: a guided setup — prompts for the Postgres URL, bucket
#      endpoint, volume name and vault path, and reads any missing secrets
#      (R2 keys, OpenAI key) without echoing them
#    → no TTY (an agent/script): reads everything from the environment
#      (TROVE_VERSIONS_DB, TROVE_R2_BUCKET, R2_ACCESS_KEY_ID,
#      R2_SECRET_ACCESS_KEY) and provisions with no prompts — or prints
#      exactly which variables to set if something's missing
#    → writes ~/.config/trove/config.toml
#    → applies the embedded SQL migration (blobs, file_versions, blob_chunks, pgvector)
#    → formats the storage volume on your bucket
```

`trove install` runs the migration and formats the volume automatically;
safety flags `--reuse` / `--reinstall` cover non-empty DBs (the default
refuses to clobber existing Trove data, and refuses to re-format a
volume against a different bucket — that would orphan its chunks).

`trove install` is idempotent, so if it fails partway — a flaky DB
connection, a missing key — fix the cause and run it again; it skips the
steps that already succeeded (`--reuse` to keep an existing populated DB
or volume). The volume is formatted in-process, so there's no separate
tool to run — re-running `trove install` is how you retry that step. If
you'd rather apply the schema migration by hand, it's a single file:

```bash
psql "$TROVE_VERSIONS_DB" -f supabase/migrations/*_init_version_chain_and_embeddings.sql
```

**Preflight + mount**:

```bash
trove doctor                            # all green?
mkdir -p /mnt/trove
trove mount /mnt/trove --types ./my-store
```

Got an existing vault you want trove to manage? Use `trove import
~/vault` instead — it moves the files aside, mounts trove at the
original path, and streams the files back through the validation gate.
See [Running it end-to-end](/docs/running#mounting-onto-an-existing-directory).

**Write a file, watch it validate + version + embed**:

```bash
echo '---
type: person
name: Bob
dob: "1985-03-20"
---' > /mnt/trove/people/bob.md

trove log /people/bob.md                # version history
trove search "people born in March"     # semantic search
```

## What `trove install` writes

`~/.config/trove/config.toml`:

```toml
versions_db = "postgres://user:pass@host:5432/dbname"
volume      = "notes"
meta        = "postgres://user:pass@host:5432/dbname"   # same as versions_db
cache       = "/tmp/trove-cache"
r2_bucket   = "https://<bucket>.<acct>.r2.cloudflarestorage.com"
store       = "/home/you/vault"
schema      = "trove_notes"   # derived from the volume name
```

**Secrets are NOT in this file.** `R2_ACCESS_KEY_ID`,
`R2_SECRET_ACCESS_KEY`, and `OPENAI_API_KEY` stay in the environment (or
your `.envrc` / `1password run`).

### One database, many volumes — and nothing in `public`

Each volume's metadata lives in its **own Postgres schema** (`trove_<volume>`,
derived from the volume name), not in `public`. Two consequences:

- **Isolation.** One database can back many volumes — each gets its own
  `blobs` / `file_versions` / `blob_chunks` and its own JuiceFS `jfs_*`
  tables, namespaced by schema. Install a second volume with a different
  name and it lands in its own `trove_<name>` schema in the same DB.
- **Not exposed by Supabase's API.** Supabase's auto REST/GraphQL API only
  serves `public` (+ `graphql_public`), so a `trove_*` schema is invisible
  to the `anon` key by default, and the schema grants no access to `anon`.
  (The role in your connection string still has full access — that's how
  trove reads and writes.) To confirm, hit `…/rest/v1/blobs` with your anon
  key: it should 404.

The `vector` extension is database-global, so `trove install` creates it
once in a shared location, not inside a volume's schema.

## Next

- Why does Trove exist? → [A filesystem that talks back](/docs/thesis)
- Want to read the code? → [`frontmatter.rs`](/docs/frontmatter) is the
  smallest module and the right place to start.
- Need the full operating manual? → [Running it end-to-end](/docs/running)
