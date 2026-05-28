# `validate.rs` — run a schema

**~110 lines.** Validates parsed frontmatter against one schema. The
selection has already happened; this module just runs the validator and
shapes the errors.

## The contract

```rust
pub fn validate_against(
    frontmatter: &serde_json::Value,
    schema: &TypeSchema,
) -> Result<(), Vec<Violation>>;

pub struct Violation {
    pub instance_path: String,  // JSON pointer, e.g. `/dob`
    pub message: String,
}
```

A successful return = the frontmatter satisfies the schema. A failure
returns the *full set* of violations, not just the first one — agents repair
faster when they see every problem at once.

## Where the schema gets compiled

```rust
let compiled = jsonschema::JSONSchema::compile(&schema.schema)?;
```

This is *per-call*. The schema is compiled every time `validate_against`
runs. A future perf pass should cache compiled schemas on `TypeSchema`
(roughly: change `TypeSchema::schema` from `serde_json::Value` to
`OnceCell<JSONSchema>`). For now, a vault's typical write rate (a few files
per second) makes the cost invisible.

## The big footgun: JSON Schema drafts

This module has the longest comment in the codebase, and it's worth
understanding *why*:

> `jsonschema` (the crate) defaults to **Draft 7**. To forbid stray
> frontmatter keys *across* an `allOf` composition (the schema author's
> usual intent when they write `unevaluatedProperties: false`), you need
> **Draft 2019-09 or 2020-12**. On Draft 7, `unevaluatedProperties` is
> silently ignored.

What this means in practice:

| You want | Draft 7 | Draft 2020-12 |
|---|---|---|
| Forbid unknown keys at one level | `additionalProperties: false` ✓ | `additionalProperties: false` ✓ |
| Forbid unknown keys across `allOf` | **silent no-op** ✗ | `unevaluatedProperties: false` ✓ |

If you ever compose schemas via `allOf` (sharing components like
`audit-fields.json`), you'll want to enable the 2020-12 draft. The comment
in the file walks you through the Cargo features and the schema-side
declarations.

For v0.1, single, flat schemas-per-type, Draft 7 is fine.

## How errors are shaped

```rust
let result = compiled.validate(frontmatter);
match result {
    Ok(()) => Ok(()),
    Err(errors) => Err(errors.map(|e| Violation {
        instance_path: …,
        message: e.to_string(),
    }).collect()),
}
```

`jsonschema` returns errors as an iterator (it walks the schema
breadth-first, collecting everything it can find without bailing). We
materialise the full list because a single user write often violates
multiple constraints, and we want the sidecar to list them all.

The `instance_path` is a JSON Pointer:

- `""` (empty) → top-level error (e.g. "required field `name` missing")
  becomes `(root)` in our output for readability.
- `"/dob"` → field-level error
- `"/aliases/2"` → array element error

That's what the agent reads in the sidecar to find the broken field
without re-parsing the schema.

## What happens if the schema itself is broken?

We catch that:

```rust
let compiled = match jsonschema::JSONSchema::compile(&schema.schema) {
    Ok(c) => c,
    Err(e) => return Err(vec![Violation {
        instance_path: String::new(),
        message: format!("schema `{}` is itself invalid: {e}", schema.name),
    }]),
};
```

A bad schema becomes a violation against the file it tried to validate.
This is a deliberate choice: it surfaces in the same `.errors` sidecar the
agent already knows how to read, rather than crashing the validator or
silently passing the write through. The store author then knows their schema
is broken.

## Tests in the module

Two unit tests, using a `registry_with(name, schema)` helper that writes a
temporary `.types/<name>.json` and loads it:

- `valid_person` — a well-formed frontmatter passes
- `wrong_field_type_flagged` — `dob: 42` (an int where a string is required)
  fails

The integration tests in `tests/check.rs` cover the full sweep over a real
store.

## Where validation gets called from

Two places, both straightforward:

1. **`commands/check.rs`** — the CLI sweep. Walk the store, parse, select
   schemas, validate each file.
2. **`mount.rs::Inner::validate`** — the commit barrier. Parse the
   buffered write, select schemas, validate. Failure here returns `EINVAL`
   and writes the `.errors` sidecar.

Same module, same contract, both surfaces.

Next: [`commands/check.rs` — the validator CLI →](/docs/check)
