# `types.rs` — the schema registry

**~155 lines.** Loads the `.types/` directory and picks which schema(s)
govern a given file.

## The mental model

A type is a JSON Schema file at `<store>/.types/<name>.json`. The schema
declares **which files it governs** via a top-level `globs` array (Cursor-rules
style):

```json
{
  "globs": ["people/**.md", "humans/**.md"],
  "type": "object",
  "properties": { "type": { "const": "person" }, … }
}
```

Two questions the registry answers:

1. **Given a path, which schemas can claim it?** (glob match)
2. **Given a parse of the file, which of those actually do?** (path + `type`
   field disambiguation)

## Selection in two steps

```rust
let registry = Registry::load(&store)?;
let schemas: Vec<&TypeSchema> = registry.select(rel_path, file_type);
```

**Step 1: candidates.** A schema is a candidate when:

- Its `globs` array matches the path, OR
- It has no `globs` at all (back-compat: applies on any path)

**Step 2: claims.** A candidate *claims* the file when:

- It pins no `type` const — the glob *is* the identifier (e.g.
  `links/reference/**` is unambiguous), OR
- It pins a const AND the file's declared `type` matches it.

A type-pinning schema **does not claim a typeless file**. This is
deliberate. A broad glob like `*.md` is shared by many types (`person`,
`concept`, daily notes) — a typeless root note should not be forcibly
validated as `person`.

## The "may govern" pre-filter

```rust
pub fn may_govern(&self, rel: &Path) -> bool;
```

This is what the mount uses to decide whether to **buffer or stream**.
Before we've read a byte of content, we can ask: "could any schema possibly
claim this path, once the content is known?" If no — the path glob can't
match and no globless schema exists — the file is **ungoverned by
construction**. The mount streams it straight through (`PassThrough`) and
saves the whole-file buffering cost.

This matters because Trove buffers every governed write in memory to
validate it as a unit. Without `may_govern`, a 5 GB binary blob in a
`vendored/` directory would balloon RAM for no reason. With it, anything no
schema can possibly claim is cheap.

## Globs: literal separators

`globset` is configured with `literal_separator(true)`:

```rust
let glob = GlobBuilder::new(pat)
    .literal_separator(true)
    .build()?;
```

This makes `*` **not cross `/`**, so:

- `*.md` matches **only** root-level files
- `**/*.md` or `**.md` matches at any depth

If you forget this, `*.md` will match `deep/nested/foo.md` (the default
behaviour), which is almost never what you want. Trove's convention is the
strict one.

## The `path_is_governed` helper

```rust
pub fn path_is_governed(&self, rel: &Path) -> bool;
```

A narrower question than `may_govern`: does **any glob** match this path?
Used for **unparseable files**. We can't read the `type` field of a broken
YAML, so we can only decide on the path. If the path is glob-governed, an
unparseable file is a finding. If not, it's a template or a vendored file
and we silently skip it.

## What the registry doesn't do

- It doesn't validate (`validate.rs`).
- It doesn't compile schemas at selection time — `select()` returns
  references to the loaded schemas; `jsonschema::JSONSchema::compile()`
  happens in `validate_against`. (This is a fair perf complaint; see
  [Contributing](/docs/contributing).)
- It doesn't watch the directory. Re-load to pick up schema changes (the
  mount loads once at startup; `trove check` loads per-run).

## The `Registry::empty()` constructor

```rust
let registry = Registry::empty();
```

A registry with no schemas. The mount uses this when `--types` is omitted —
nothing is governed, so the write path gates nothing and the mount becomes a
pure pass-through over JuiceFS. Useful as a sanity check (does the mount
itself work, before adding schemas?) and as a deliberate "I want history but
no validation" mode.

## A complete schema example

```json
{
  "globs": ["people/**.md"],
  "type": "object",
  "required": ["type", "name"],
  "additionalProperties": false,
  "properties": {
    "type":    { "const": "person" },
    "name":    { "type": "string", "minLength": 1 },
    "aliases": { "type": "array", "items": { "type": "string" } },
    "dob":     { "type": "string", "format": "date" },
    "company": { "type": "string" }
  }
}
```

Drop this at `<store>/.types/person.json` and every file under `people/`
becomes a governed `person`. A write missing `name`, or with `dob` set to
something that isn't an ISO date, will be rejected at the commit barrier.

Next: [`validate.rs` — run a schema →](/docs/validate)
