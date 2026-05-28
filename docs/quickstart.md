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
export DATABASE_URL="postgres://user:pass@host:5432/dbname"
export S3_ACCESS_KEY_ID="..."
export S3_SECRET_ACCESS_KEY="..."
```

That's it. The Postgres URL doubles as JuiceFS's metadata engine AND the
version-chain + embeddings store — **one connection, one substrate**. The
S3 bucket holds the file data; JuiceFS chunks and stores it for you.

- **Running on one machine?** A local `postgres` install is fine — point
  `DATABASE_URL` at `127.0.0.1:5432`.
- **Running across machines?** Use a hosted Postgres (Supabase, Neon, RDS).
  Just point `DATABASE_URL` at it. JuiceFS handles the metadata coordination
  itself.

Optional fourth: `OPENAI_API_KEY` for semantic search. Without it, pass
`--no-embed` to `trove mount` and you'll still get validation + version
history.

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
# 1. Write config + provision the backend (one-time, interactive).
trove install
#    → asks for DATABASE_URL, S3 bucket URL, volume name, vault path
#    → writes ~/.config/trove/config.toml
#    → applies the embedded SQL migration (blobs, file_versions, blob_chunks, pgvector)
#    → formats the JuiceFS volume on your bucket
```

`trove install` runs migrations and formats the volume automatically;
safety flags `--reuse` / `--reinstall` cover non-empty DBs (the default
refuses to clobber existing Trove data, and refuses to re-format a
volume against a different bucket — that would orphan its chunks).

If install fails partway, the equivalent manual steps are:

```bash
# Apply the schema migration manually.
psql "$DATABASE_URL" -f supabase/migrations/*_init_version_chain_and_embeddings.sql

# Format the JuiceFS volume on your bucket.
juicefs format \
    --storage s3 \
    --bucket   "https://<bucket>.<acct>.r2.cloudflarestorage.com" \
    --access-key  "$S3_ACCESS_KEY_ID" \
    --secret-key  "$S3_SECRET_ACCESS_KEY" \
    "$DATABASE_URL" \
    trove
```

**Preflight + mount**:

```bash
trove doctor                            # all green?
mkdir -p /mnt/trove
trove mount /mnt/trove --types ./my-store
```

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
volume      = "trove"
meta        = "postgres://user:pass@host:5432/dbname"   # same as versions_db
cache       = "/tmp/trove-cache"
r2_bucket   = "trove"
store       = "/home/you/vault"
```

**Secrets are NOT in this file.** `S3_ACCESS_KEY_ID`,
`S3_SECRET_ACCESS_KEY`, and `OPENAI_API_KEY` stay in the environment (or
your `.envrc` / `1password run`).

## Next

- Why does Trove exist? → [A filesystem that talks back](/docs/thesis)
- Want to read the code? → [`frontmatter.rs`](/docs/frontmatter) is the
  smallest module and the right place to start.
- Need the full operating manual? → [Running it end-to-end](/docs/running)
