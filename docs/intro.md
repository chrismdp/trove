# What is Trove?

Trove is **a filesystem that talks back**. You read and write ordinary
markdown files; the filesystem itself enforces schemas on write, keeps full
version history, and builds a semantic search index — with zero per-agent
integration.

The thesis: coordination, validation and visibility belong *at the filesystem
layer*, not bolted onto each agent via MCP/SDK glue. Any tool that can write
a file becomes a well-behaved citizen of your knowledge store, automatically.

## The promise in one example

You mount Trove at `~/vault`. An agent (Claude Code, an editor, a script,
whatever) writes a markdown file:

```
~/vault/people/alice.md
```

```markdown
---
type: person
dob: not-a-date
---

Alice is a person.
```

The agent's `close()` returns `EINVAL`. Nothing persists. A sidecar appears:

```
~/vault/people/alice.md.errors
```

```
/dob: "not-a-date" is not of type "string" matching format "date"
```

The agent reads the sidecar, fixes the file, and tries again. No MCP server,
no SDK, no schema endpoint — just `open` / `write` / `close`. The
**filesystem talked back**.

## What's in v0.1

Working, end-to-end:

- **`trove check`** — walk a store and validate every typed file against its
  schema. No native deps; drops into a pre-commit hook or CI.
- **`trove mount`** — a FUSE projection over a JuiceFS volume (R2 + Postgres).
  Validation runs at the commit barrier; rejection returns `EINVAL` and writes
  a `.errors` sidecar.
- **COW version history** — every validated write COW-clones into
  `/.trove/versions/<sha>`. Zero byte duplication; every revision's exact
  bytes are recoverable.
- **Self-triggering embeddings** — a committed file is embedded into
  `blob_chunks` (pgvector) by a background thread, off the write path.
- **`trove search`** — semantic search over `blob_chunks` via cosine distance.
- **`trove log` / `cat` / `diff` / `restore`** — the history CLI.
- **`trove server`** — a localhost HTTP view of one store (file list, search,
  raw content). Front with nginx for external access.

Not built yet: anything that isn't above.

## What this walkthrough is for

Two audiences:

1. **You want to *use* Trove.** Read [Quickstart](/docs/quickstart), then
   [Running it end-to-end](/docs/running).
2. **You want to understand the code.** Work through the modules in order —
   [`frontmatter.rs`](/docs/frontmatter), [`types.rs`](/docs/types),
   [`validate.rs`](/docs/validate) are the native-dep-free core; the substrate
   modules ([`jfs.rs`](/docs/jfs), [`mount.rs`](/docs/mount), …) build on top.
   The deep-dive pages explain the three pipelines (write, versioning,
   embedding) that the modules combine to form.

## What's deliberately not here

- **No multi-tenancy.** v0.1 is single-tenant per the strategy pivot. ACLs,
  per-user volumes, and remote authentication are out of scope.
- **No SaaS.** `trove server` binds `127.0.0.1` only. You're meant to front it
  with nginx on your own VPS, where TLS and any auth terminate.
- **No write API in the server.** It's a viewer, not a CRUD endpoint.
- **No `juicefs` subprocess.** Trove embeds libjfs in-process. Nobody runs or
  sees "juicefs" — the `trove` binary *is* the JuiceFS client.

Read on: [Quickstart →](/docs/quickstart)
