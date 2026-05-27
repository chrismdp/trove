//! End-to-end tests for `trove check` against a real on-disk store.
//! Each test builds an isolated temp store, writes schemas + notes, and asserts
//! on the Summary the sweep returns. Selection is glob-based (Cursor-rules
//! style), with the `type` field disambiguating co-located types.

use std::fs;
use std::path::{Path, PathBuf};
use trove::commands::check;

struct TempStore {
    root: PathBuf,
}

impl TempStore {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "trove-it-{}-{}-{}",
            tag,
            std::process::id(),
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

// Person governs root-level *.md (not subdirs), pinned to type=person.
const PERSON_SCHEMA: &str = r#"{
  "globs": ["*.md"],
  "type": "object",
  "required": ["type"],
  "properties": {
    "type": { "const": "person" },
    "aliases": { "type": "array", "items": { "type": "string" } },
    "dob": { "type": "string" }
  },
  "additionalProperties": true
}"#;

// Concept also lives at root — co-located with person, disambiguated by type.
const CONCEPT_SCHEMA: &str = r#"{
  "globs": ["*.md"],
  "type": "object",
  "required": ["type"],
  "properties": { "type": { "const": "concept" } },
  "additionalProperties": true
}"#;

// Project governs projects/<slug>/PROJECT.md only.
const PROJECT_SCHEMA: &str = r#"{
  "globs": ["projects/**/PROJECT.md"],
  "type": "object",
  "required": ["type", "status"],
  "properties": {
    "type": { "const": "project" },
    "status": { "enum": ["todo", "doing", "done", "someday"] }
  },
  "additionalProperties": true
}"#;

#[test]
fn valid_person_at_root_passes() {
    let store = TempStore::new("valid");
    store
        .schema("person", PERSON_SCHEMA)
        .note("Rebekah.md", "---\ntype: person\ndob: \"2010-06-23\"\n---\nbody");

    let s = check::run(store.path(), true).unwrap();
    assert_eq!((s.checked, s.valid, s.failed, s.untyped), (1, 1, 0, 0));
}

#[test]
fn wrong_field_type_fails() {
    let store = TempStore::new("wrongtype");
    store
        .schema("person", PERSON_SCHEMA)
        .note("Bad.md", "---\ntype: person\ndob: 42\n---\n");
    assert_eq!(check::run(store.path(), true).unwrap().failed, 1);
}

#[test]
fn co_located_types_disambiguate_by_type_field() {
    // person + concept both glob *.md; the type field decides which claims.
    let store = TempStore::new("colocated");
    store
        .schema("person", PERSON_SCHEMA)
        .schema("concept", CONCEPT_SCHEMA)
        .note("Rebekah.md", "---\ntype: person\n---\n")          // -> person, valid
        .note("Some Idea.md", "---\ntype: concept\n---\n")        // -> concept, valid
        .note("Bad Person.md", "---\ntype: person\ndob: 9\n---\n"); // -> person, fail
    let s = check::run(store.path(), true).unwrap();
    assert_eq!((s.valid, s.failed), (2, 1));
}

#[test]
fn project_glob_governs_only_project_files() {
    let store = TempStore::new("project");
    store
        .schema("project", PROJECT_SCHEMA)
        .note("projects/Foo/PROJECT.md", "---\ntype: project\nstatus: todo\n---\n")     // valid
        .note("projects/Bar/PROJECT.md", "---\ntype: project\nstatus: bogus\n---\n");   // bad status
    let s = check::run(store.path(), true).unwrap();
    assert_eq!((s.valid, s.failed), (1, 1));
}

#[test]
fn file_outside_any_glob_is_untyped() {
    // A subdir note doesn't match the root-only *.md glob -> untyped, not failed.
    let store = TempStore::new("outside");
    store
        .schema("person", PERSON_SCHEMA)
        .note("subdir/Note.md", "---\ntype: person\ndob: 42\n---\n"); // would fail IF claimed
    let s = check::run(store.path(), true).unwrap();
    assert_eq!((s.untyped, s.failed), (1, 0));
}

#[test]
fn template_files_skipped_via_globs() {
    // templates/ doesn't match person's root-only *.md glob, so the {{...}}
    // placeholder file is simply untyped — no special-casing needed.
    let store = TempStore::new("template");
    store
        .schema("person", PERSON_SCHEMA)
        .note("templates/person.md", "---\ntype: person\nname: {{title}}\n---\n");
    let s = check::run(store.path(), true).unwrap();
    assert_eq!((s.failed, s.untyped), (0, 1));
}

#[test]
fn type_with_no_matching_schema_is_untyped() {
    let store = TempStore::new("noschema");
    store
        .schema("person", PERSON_SCHEMA)
        .note("Some Project.md", "---\ntype: project\nstatus: doing\n---\n");
    let s = check::run(store.path(), true).unwrap();
    assert_eq!((s.untyped, s.failed), (1, 0));
}

#[test]
fn type_const_schema_does_not_claim_typeless_files() {
    // A broad/shared glob must not force-validate a typeless file. project.json
    // pins type=project, so a PROJECT.md with no type field is left untyped
    // rather than wrongly claimed — the type field gates type-const schemas.
    let store = TempStore::new("notypefield");
    store
        .schema("project", PROJECT_SCHEMA)
        .note("projects/Foo/PROJECT.md", "---\nstatus: bogus\n---\n");
    let s = check::run(store.path(), true).unwrap();
    assert_eq!((s.untyped, s.failed), (1, 0));
}

#[test]
fn typeless_files_under_broad_root_glob_are_untyped() {
    // The real-vault failure mode: person globs *.md (root) but the root holds
    // daily notes, untyped concepts, etc. None of those should be claimed.
    let store = TempStore::new("rootbroad");
    store
        .schema("person", PERSON_SCHEMA)
        .note("2024-04-09.md", "## a daily note, no frontmatter")
        .note("Some Concept.md", "---\nsome: value\n---\n") // typeless frontmatter
        .note("Rebekah.md", "---\ntype: person\n---\n");      // the only real person
    let s = check::run(store.path(), true).unwrap();
    assert_eq!((s.valid, s.failed, s.untyped), (1, 0, 2));
}

#[test]
fn unclosed_fence_fails_when_governed() {
    let store = TempStore::new("unclosed");
    store
        .schema("person", PERSON_SCHEMA)
        .note("Broken.md", "---\ntype: person\nno closing fence here");
    assert_eq!(check::run(store.path(), true).unwrap().failed, 1);
}

#[test]
fn unclosed_fence_skipped_when_not_governed() {
    // A parse-broken file no schema governs is untyped, not a failure.
    let store = TempStore::new("unclosed-ungoverned");
    store
        .schema("project", PROJECT_SCHEMA) // only governs projects/**
        .note("Random.md", "---\ntype: person\nno closing fence");
    let s = check::run(store.path(), true).unwrap();
    assert_eq!((s.untyped, s.failed), (1, 0));
}

#[test]
fn empty_registry_checks_nothing() {
    let store = TempStore::new("empty");
    store.note("Lonely.md", "---\ntype: person\n---\n");
    let s = check::run(store.path(), true).unwrap();
    assert_eq!((s.untyped, s.failed), (1, 0));
}

#[test]
fn dotdirs_and_non_md_ignored() {
    let store = TempStore::new("ignore");
    store
        .schema("person", PERSON_SCHEMA)
        .note("Good.md", "---\ntype: person\n---\n")
        .note(".obsidian/workspace.md", "---\ntype: person\ndob: 9\n---\n")
        .note("notes.txt", "not markdown");
    let s = check::run(store.path(), true).unwrap();
    assert_eq!((s.checked, s.valid, s.failed), (1, 1, 0));
}
