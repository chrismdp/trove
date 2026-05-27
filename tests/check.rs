//! End-to-end tests for `trove check` against a real on-disk store.
//! Each test builds an isolated temp store, writes schemas + notes, and asserts
//! on the Summary the sweep returns.

use std::fs;
use std::path::{Path, PathBuf};
use trove::commands::check;

/// A throwaway store under the OS temp dir, cleaned up on drop.
struct TempStore {
    root: PathBuf,
}

impl TempStore {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "trove-it-{}-{}-{}",
            tag,
            std::process::id(),
            // nanosecond tag so parallel tests never collide
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn schema(&self, name: &str, json: &str) -> &Self {
        let dir = self.root.join(".types");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.json")), json).unwrap();
        self
    }

    fn note(&self, rel: &str, body: &str) -> &Self {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
        self
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const PERSON_SCHEMA: &str = r#"{
  "type": "object",
  "required": ["type"],
  "properties": {
    "type": { "const": "person" },
    "aliases": { "type": "array", "items": { "type": "string" } },
    "dob": { "type": "string" }
  },
  "additionalProperties": true
}"#;

#[test]
fn valid_typed_note_passes() {
    let store = TempStore::new("valid");
    store
        .schema("person", PERSON_SCHEMA)
        .note("Rebekah.md", "---\ntype: person\ndob: \"2010-06-23\"\n---\nbody");

    let s = check::run(store.path(), true).unwrap();
    assert_eq!(s.checked, 1);
    assert_eq!(s.valid, 1);
    assert_eq!(s.failed, 0);
    assert_eq!(s.untyped, 0);
}

#[test]
fn wrong_field_type_fails() {
    // The real-world bug class: `dob` written as a bare number, not a string.
    let store = TempStore::new("wrongtype");
    store
        .schema("person", PERSON_SCHEMA)
        .note("Bad.md", "---\ntype: person\ndob: 42\n---\n");

    let s = check::run(store.path(), true).unwrap();
    assert_eq!(s.failed, 1);
    assert_eq!(s.valid, 0);
}

#[test]
fn missing_required_field_fails() {
    let store = TempStore::new("required");
    store
        // require an `aliases` field to prove required-enforcement works
        .schema(
            "person",
            r#"{"type":"object","required":["type","aliases"],"properties":{"type":{"const":"person"}}}"#,
        )
        .note("NoAliases.md", "---\ntype: person\n---\n");

    let s = check::run(store.path(), true).unwrap();
    assert_eq!(s.failed, 1);
}

#[test]
fn untyped_note_is_skipped_not_failed() {
    let store = TempStore::new("untyped");
    store
        .schema("person", PERSON_SCHEMA)
        .note("Daily.md", "# just a daily note\n\nno frontmatter here");

    let s = check::run(store.path(), true).unwrap();
    assert_eq!(s.checked, 1);
    assert_eq!(s.untyped, 1);
    assert_eq!(s.failed, 0);
}

#[test]
fn type_with_no_schema_is_untyped() {
    // `type: project` declared, but no project.json registered → nothing to
    // validate against, so it's untyped, not a failure.
    let store = TempStore::new("noschema");
    store
        .schema("person", PERSON_SCHEMA)
        .note("Some Project.md", "---\ntype: project\nstatus: doing\n---\n");

    let s = check::run(store.path(), true).unwrap();
    assert_eq!(s.untyped, 1);
    assert_eq!(s.failed, 0);
}

#[test]
fn unclosed_frontmatter_fence_fails() {
    let store = TempStore::new("unclosed");
    store
        .schema("person", PERSON_SCHEMA)
        .note("Broken.md", "---\ntype: person\nno closing fence here");

    let s = check::run(store.path(), true).unwrap();
    assert_eq!(s.failed, 1);
}

#[test]
fn malformed_yaml_fails() {
    let store = TempStore::new("badyaml");
    store
        .schema("person", PERSON_SCHEMA)
        // tab indentation + broken mapping → YAML parse error
        .note("BadYaml.md", "---\ntype: person\n  - this: is\n bad: yaml: here\n---\n");

    let s = check::run(store.path(), true).unwrap();
    assert_eq!(s.failed, 1);
}

#[test]
fn dotdirs_and_non_md_ignored() {
    let store = TempStore::new("ignore");
    store
        .schema("person", PERSON_SCHEMA)
        .note("Good.md", "---\ntype: person\n---\n")
        .note(".obsidian/workspace.md", "---\ntype: person\ndob: 99\n---\n") // would fail if scanned
        .note("notes.txt", "not markdown");

    let s = check::run(store.path(), true).unwrap();
    // Only Good.md should be checked; the dotdir note and the .txt are skipped.
    assert_eq!(s.checked, 1);
    assert_eq!(s.valid, 1);
    assert_eq!(s.failed, 0);
}

#[test]
fn empty_store_with_no_types_checks_nothing() {
    let store = TempStore::new("empty");
    store.note("Lonely.md", "---\ntype: person\n---\n");
    // No .types dir at all → registry empty → everything untyped, nothing fails.
    let s = check::run(store.path(), true).unwrap();
    assert_eq!(s.untyped, 1);
    assert_eq!(s.failed, 0);
}

#[test]
fn template_placeholder_files_are_skipped_not_failed() {
    let store = TempStore::new("template");
    store
        .schema("person", PERSON_SCHEMA)
        // a template: {{placeholder}} is not valid YAML, but it's not corruption
        .note("templates/person.md", "---\ntype: person\nname: {{title}}\ndob: {{date}}\n---\n");

    let s = check::run(store.path(), true).unwrap();
    assert_eq!(s.failed, 0, "template files must not count as failures");
}

#[test]
fn mixed_store_counts_each_bucket() {
    let store = TempStore::new("mixed");
    store
        .schema("person", PERSON_SCHEMA)
        .note("A.md", "---\ntype: person\ndob: \"2010-01-01\"\n---\n") // valid
        .note("B.md", "---\ntype: person\ndob: 7\n---\n") // fail
        .note("C.md", "# daily, no frontmatter") // untyped
        .note("D.md", "---\ntype: project\n---\n"); // untyped (no schema)

    let s = check::run(store.path(), true).unwrap();
    assert_eq!(s.checked, 4);
    assert_eq!(s.valid, 1);
    assert_eq!(s.failed, 1);
    assert_eq!(s.untyped, 2);
}
