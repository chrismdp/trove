# A filesystem that talks back

The shortest statement of what Trove is doing differently.

## The problem

Agents (LLMs, scripts, editors, humans) write to a shared knowledge store.
Without coordination they produce:

- Notes in inconsistent shapes (some have `tags:`, some have `keywords:`, some
  bury keywords in prose).
- Files that *almost* match a schema, with a one-character typo nobody
  noticed.
- Duplicate concepts because nobody checked what was already there.
- A growing pile of broken cross-references nobody can find.

The conventional fix is **MCP/SDK glue per agent**: every agent learns the
schema by integrating with a server that gates writes. This works, but it
scales by `O(agents × schemas)` — every new tool needs to learn about every
schema, and the discipline only holds if every tool actually integrates.

## The thesis

Push validation, history, and discoverability **down to the filesystem
layer**. Then:

- Any program that can `open` / `write` / `close` gets schema validation for
  free.
- A new agent or script needs zero integration. `vim`, `claude code`,
  `python`, `curl > file`, an editor's "save" — all become well-behaved.
- The schema is just a JSON Schema file inside the store. Editing a schema
  *is* the migration; writes self-heal lazily as records are touched.

This is what we mean by "a filesystem that talks back". A write that breaks
the schema returns `EINVAL` and writes a sidecar explaining why. The agent
reads its own error message via the standard read path it already knows.

## Why filesystems

A filesystem is the **lowest-common-denominator API for shared mutable
state**. Every programming language, every script, every editor speaks it.
You don't have to convince anyone to integrate with it; they already do.

Push coordination down here and you get a property MCP can't give you:
**zero-integration enforcement**. The store gets to define what counts as a
valid record, and every writer is gated by the kernel itself.

POSIX-shaped, not POSIX-complete. Trove deliberately deviates in exactly one
place: the commit barrier. A `flush`/`fsync` on a governed file can return
`EINVAL`. Everything else delegates to libjfs and behaves like a normal
POSIX filesystem.

## What it isn't

- **Not a database.** Files are still files. You can `grep`, `cat`, `cp`,
  `mv`. The schema is YAML frontmatter; the body is free-form markdown.
- **Not a content-management system.** No web UI, no permissions model, no
  workflow engine. (`trove server` is a viewer, not a CMS.)
- **Not an indexer.** It builds an index, but the bytes live in JuiceFS, not
  in a separate search store.
- **Not a replacement for git.** History here is byte-level COW snapshots
  inside the filesystem. Use git for branches, PRs, and remote sync; use
  Trove for "show me every version of this note as it lived in this store".

## Three pipelines, one substrate

Everything Trove does is a write going through one of three pipelines:

1. **Validation pipeline** — does this write satisfy the schema its path
   selects? Reject at the commit barrier if not.
   ([Write pipeline →](/docs/write-pipeline))

2. **Versioning pipeline** — after validation, COW-clone the committed file
   into the version archive and append a chain row.
   ([COW versioning →](/docs/cow-versions))

3. **Embedding pipeline** — after versioning, push `(hash, content)` to a
   background thread that chunks + embeds via OpenAI and writes to
   `blob_chunks`.
   ([Embeddings →](/docs/embedding-pipeline))

The rest of the codebase is plumbing for those three.

Next: [`frontmatter.rs` — parse the fence →](/docs/frontmatter)
