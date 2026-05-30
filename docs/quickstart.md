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

Don't have a Postgres database or a bucket yet? Create a Postgres database and
create the R2 bucket named for your folder (`trove-<folder-name>`), then run
`trove init` from that folder.

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

With the DB URL and R2 credentials available, create or enter the folder that
will be the vault. The folder name is the vault name:

```bash
mkdir notes
cd notes

# Create bucket `trove-notes` in R2 first, then:
trove init
#    → derives schema `trove_notes` and bucket `trove-notes`
#    → validates the bucket exists and whether it is empty/non-empty
#    → creates or attaches the matching Postgres schema
#    → writes ~/.config/trove/credentials.toml and ~/.config/trove/volumes/notes.toml
#    → installs a per-vault boot agent and mounts in the background — shell back
```

`trove init` (alias `trove attach`) sets this machine up and **mounts in the
background**, so you get your prompt straight back. It also installs a per-vault
**boot agent** (launchd on macOS, a `systemd --user` service on Linux) so the
vault **re-mounts automatically at every login** — a FUSE mount never survives a
reboot on its own, and that re-mounting is the whole point. Pass `--no-autostart`
to skip the agent and mount in the foreground instead (no system change).

Once mounted, the lifecycle is two command pairs:

```bash
trove ls                       # every vault on this machine + mount/agent status
trove unmount --volume notes   # runtime "down for now" (re-mounts next login)
trove mount --volume notes     # bring it back up now (what the boot agent runs)
trove detach --volume notes    # remove this machine's footprint (backend untouched)
```

`unmount`/`mount` are transient up/down and never touch the config or agent.
`detach` removes this machine's config + agent but **leaves the vault intact in
its backend** — other machines are unaffected and `trove init` re-attaches here
later. See [the vault lifecycle](/docs/lifecycle) for the full model.

If the schema and a non-empty bucket already exist, `trove init` attaches this
machine to that vault. If the bucket is missing, create it in R2 and re-run. If
only one side exists, Trove stops with a conflict error so you can rename the
folder or clear the stray resource. If you'd rather apply the schema migration
by hand, it's a single file:

```bash
psql "$TROVE_VERSIONS_DB" -f supabase/migrations/*_init_version_chain_and_embeddings.sql
```

**Preflight**:

```bash
trove doctor                            # all green?
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
---' > people/bob.md

trove log /people/bob.md                # version history
trove search "people born in March"     # semantic search
```

## What `trove init` writes

Shared machine credentials in `~/.config/trove/credentials.toml`:

```toml
versions_db = "postgres://user:pass@host:5432/dbname"
r2_endpoint = "https://<acct>.r2.cloudflarestorage.com"
```

Per-volume config in `~/.config/trove/volumes/notes.toml`:

```toml
bucket     = "https://<acct>.r2.cloudflarestorage.com/trove-notes"
schema     = "trove_notes"
mountpoint = "/home/you/notes"
cache      = "/tmp/trove-cache"
```

`OPENAI_API_KEY` stays in the environment. R2 keys are read from the environment
first and may be saved in the shared credentials file for local operator
convenience.

…plus a **boot agent** so the vault re-mounts at login:
`~/Library/LaunchAgents/com.trove.notes.plist` (macOS) or the
`systemd --user` instance `trove@notes.service` (Linux). It runs `trove mount
--volume notes`, resolving everything from the saved config.

### Many accounts on one machine — credential profiles

The default (top-level) credentials back every volume — the fleet case (one DB +
one R2 cred → many volumes). When a machine holds vaults on *different* accounts,
add a **named profile** and attach under it:

```bash
cd work-notes
trove init --profile work     # prompts for + saves [profiles.work]; this volume uses it
```

```toml
# ~/.config/trove/credentials.toml
versions_db = "postgres://…A"          # default profile (unchanged)
r2_endpoint = "https://acctA.r2.cloudflarestorage.com"

[profiles.work]                         # an independent account
versions_db = "postgres://…B"
r2_endpoint = "https://acctB.r2.cloudflarestorage.com"
r2_access_key_id = "…"
r2_secret_access_key = "…"
```

The volume records only `credentials = "work"` (a profile name, not a secret);
every secret stays in the one `chmod 600` file. Volumes on different accounts
auto-mount independently. Omit `--profile` and nothing changes — you never see
profiles unless you need them.

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

The `vector` extension is database-global, so `trove init` creates it
once in a shared location, not inside a volume's schema.

## Next

- Why does Trove exist? → [A filesystem that talks back](/docs/thesis)
- Want to read the code? → [`frontmatter.rs`](/docs/frontmatter) is the
  smallest module and the right place to start.
- Need the full operating manual? → [Running it end-to-end](/docs/running)
