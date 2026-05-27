# trove

**A filesystem that talks back.** Coordination, validation and visibility pushed
down to the filesystem layer, so any agent — or human — gets typed, validated,
schema-checked shared state with zero per-agent integration. You don't need MCP
to coordinate agents; you need a filesystem that talks back.

## Status — v0.1 (single-tenant, local-first)

The load-bearing core first: **schema-on-write validation** for a
markdown+frontmatter store. This is the moat (the *backing*, not the interface),
and it's what makes the rest — embeddings search, the FUSE projection, per-path
ACLs — meaningful rather than a dumb mount.

### `trove check <store>`

Walks a store, and for every markdown file whose frontmatter declares a `type`,
validates it against the matching JSON Schema in `<store>/.types/<type>.json`.
Untyped files and types with no schema are skipped; parse failures (broken YAML,
unclosed fences, non-UTF-8) are reported regardless of schema. Non-zero exit on
any failure, so it drops straight into a pre-commit hook or CI.

```
trove check ~/vault          # validate a whole store
trove check ~/vault --quiet  # failures + summary only
```

The type registry (`.types/*.json`) is **data in the store, not code** — editing
a schema is how you migrate; writes self-heal lazily as records are touched.

## Roadmap

1. **`trove check`** — schema-on-write validation. ✅ done
2. **`trove search`** — embeddings / semantic search over the store (embedded
   vector index, local-first; see `docs/backend.md`).
3. **`trove mount`** — FUSE projection so the validation contract runs on the
   write path: a `close()`/`fsync()` that violates schema returns `EINVAL` with
   a sibling `.errors` file, ACL violations return `EACCES`/`EROFS`.

## Licence

Functional Source License (FSL-1.1, future-converts to Apache 2.0). See
[LICENSE.md](LICENSE.md). Internal/commercial use permitted; competing hosting
services reserved for two years, then auto-converts to OSS.
