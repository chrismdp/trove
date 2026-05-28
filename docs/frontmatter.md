# `frontmatter.rs` — parse the fence

**~85 lines.** The smallest module in the codebase, and the one to read
first.

## What it does

Splits a markdown file into its **YAML frontmatter** and **body**, parses the
frontmatter into a JSON value, and reports whether the file *claims* to have
frontmatter at all.

```rust
pub struct Document {
    pub frontmatter: serde_json::Value,  // Null if no fence; Object {} if empty fence
    pub had_fence: bool,                 // file opened with `---`?
}

pub fn parse(raw: &str) -> Result<Document>;
```

## The contract

A document **has frontmatter** when its first line is `---`. The block runs
to the next `---` on its own line. Everything before the closing fence is
YAML; everything after is the body (which `parse` ignores — Trove only
validates the frontmatter).

| Input | `had_fence` | `frontmatter` |
|---|---|---|
| `# just body` | `false` | `Null` |
| `---\ntype: person\n---\n…` | `true` | `{"type": "person"}` |
| `---\n---\n…` (empty fence) | `true` | `{}` |
| `---\nbroken yaml: :\n---` | — | **error** |
| `---\nopen but never close` | — | **error** |

## Why YAML → JSON?

`jsonschema` (the validator we use) takes a `serde_json::Value`. YAML is a
superset of JSON, so any valid YAML maps to a JSON value (with one caveat:
YAML allows non-string map keys, which JSON doesn't — we just propagate that
error). The two-step `serde_yaml::Value` → `serde_json::Value` conversion is
the whole bridge.

## Edge cases worth knowing

- **BOM**: a leading `\u{feff}` is stripped before checking the fence.
  Without this, a UTF-8 BOM at the start of a file would make the first line
  *appear* to be `\u{feff}---`, breaking detection.
- **Trailing whitespace on the fence line**: tolerated (`---  ` opens the
  fence). YAML linters sometimes add it.
- **Empty fence**: `---\n---\n` is a valid (empty) frontmatter. The
  `Document` reports `had_fence: true` and `frontmatter: {}`. The validator
  will then run a schema's `required` array against an empty object, which is
  the right behaviour (missing fields are flagged).
- **Unclosed fence**: a hard error. The file is malformed; we refuse to
  guess where the body starts.

## What it doesn't do

- It doesn't know about schemas. Selection happens in
  [`types.rs`](/docs/types).
- It doesn't read the file from disk. `parse(&str)` takes a string. The
  caller (`check.rs` or `mount.rs`) handles IO and UTF-8 errors.
- It doesn't validate. That's [`validate.rs`](/docs/validate).

## The tests

Three unit tests in the file itself. They're the cheapest sanity check in
the codebase — `cargo test` runs them in milliseconds:

```rust
#[test] fn no_frontmatter() { … }
#[test] fn simple_frontmatter() { … }
#[test] fn unclosed_fence_errors() { … }
```

If you change this file, run the tests. The rest of the validation pipeline
trusts this module's output without re-checking.

## Why a module this small?

Because it's the **boundary between bytes and structured data**. Everything
upstream of it sees `&str`; everything downstream sees a typed JSON value.
Keeping the boundary as a tiny pure function with no IO makes it trivially
testable and impossible to mis-use.

Next: [`types.rs` — the schema registry →](/docs/types)
