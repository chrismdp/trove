# `commands/check.rs` — the validator CLI

**~120 lines.** Walks a store, validates every typed markdown file, prints a
summary, and exits non-zero on any failure.

This is the **CLI shell around the core**. No FUSE, no Postgres, no
embeddings — just `Registry` + `frontmatter::parse` + `validate_against`.
It's what you run in a pre-commit hook or in CI.

## What it does, in order

```rust
pub fn run(store: &Path, quiet: bool) -> Result<Summary>;

pub struct Summary {
    pub checked: usize,
    pub valid: usize,
    pub untyped: usize,
    pub failed: usize,
}
```

1. **Load the registry** from `<store>/.types/`. An empty registry triggers
   a warning (we'll walk the files, but nothing will validate).
2. **Walk the tree** with `walkdir`, skipping any directory starting with
   `.` except the store root itself. (`.types/` itself is loaded explicitly;
   `.git`, `.obsidian`, etc. are ignored.)
3. For each `*.md` file:
   - Read bytes (a non-UTF-8 file is a finding, not an abort).
   - Parse frontmatter. A parse error is a finding *only on a governed
     path*; on an ungoverned path (templates, vendored dirs) it's a silent
     skip.
   - Read the `type` field, ask the registry which schemas claim this file.
   - No claimants → counted as `untyped`, no output unless `--quiet` is off
     and you're verbose.
   - One or more → run `validate_against` on each, collect all violations.
4. Print `ok` lines for passes (unless `--quiet`), `FAIL` lines with
   indented violations for failures.
5. Return the `Summary`. `main.rs` prints the final counts and exits with
   `s.failed` as the return code.

## The output format

A passing file:

```
ok   people/alice.md
```

A failing file:

```
FAIL people/bob.md
      ↳ [person] /dob: "not-a-date" is not of type "string" matching format "date"
      ↳ [person] (root): "name" is a required property
```

Each violation line carries the **schema name** in brackets — useful when a
file is governed by multiple schemas (e.g. a "person" + a "has-audit-fields"
component schema). The `(root)` token replaces the empty JSON pointer for
readability.

## The `--quiet` flag

Without `--quiet`, every passing file gets an `ok` line — verbose but useful
when you want to *see* the sweep work. With `--quiet`, only failures plus the
summary print — what you want in CI logs.

## Three classes of file outcome

| Class | Condition | What happens |
|---|---|---|
| **Valid** | a schema claimed it and it passed | `ok` line, `valid++` |
| **Untyped** | no schema claimed it | counted as `untyped`, no output |
| **Failed** | parse error on a governed path, or any violation | `FAIL` line, `failed++` |

The distinction matters in CI. A repo with 1000 markdown files and 5
schemas might have 50 governed files; the other 950 are `untyped` and
don't pollute the output. The `untyped` count tells you at a glance whether
your schema coverage is what you think it is.

## Why a separate module?

The mount has its own validation path (in `mount.rs::Inner::validate`) that
runs against an in-memory buffer rather than a disk file. The two paths
share **everything below the IO layer**: parsing, schema selection,
violation shaping. Keeping the CLI sweep here means `mount.rs` doesn't have
to depend on `walkdir`, and the validator core stays a pure data
transformation.

The single shared dependency surface is `crate::frontmatter`,
`crate::types::Registry`, and `crate::validate::validate_against`.
Everything else is local concern.

## How CI uses this

```yaml
- run: cargo run --release -- check ./vault --quiet
```

`cargo run` build is fine because the `mount` feature isn't required —
`trove check` builds and runs with no native deps. A non-zero exit
(`s.failed > 0`) fails the job. The `--quiet` flag keeps the log lean: only
failures plus a one-line summary at the end.

## A pre-commit hook

```bash
#!/usr/bin/env bash
# .git/hooks/pre-commit
trove check . --quiet || { echo "trove validation failed"; exit 1; }
```

Same exit code semantics; same idea. The hook keeps schema-violations from
ever landing in git history.

Next: [`jfs.rs` — libjfs FFI →](/docs/jfs)
