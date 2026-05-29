//! The type registry. Each type is a JSON Schema living in `.types/<name>.json`
//! inside the store. A schema declares which files it governs via a top-level
//! `globs` array (Cursor-rules style) — path-based applicability. Where several
//! types share a path (e.g. person and concept both at the vault root), the
//! `type` frontmatter field disambiguates, matched against the schema's
//! `properties.type.const`.

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::path::{Path, PathBuf};

pub struct TypeSchema {
    pub name: String,
    pub schema: serde_json::Value,
    /// The `properties.type.const` this schema pins, if any. Used to
    /// disambiguate when multiple schemas glob the same path.
    pub type_const: Option<String>,
    /// Compiled globs from the schema's top-level `globs` array.
    globs: GlobSet,
    /// Whether the schema declared any globs. A schema with no globs applies
    /// by `type` field alone (back-compatible selection), on any path.
    pub has_globs: bool,
}

impl TypeSchema {
    fn glob_matches(&self, rel: &Path) -> bool {
        self.globs.is_match(rel)
    }
}

/// Parse one JSON-Schema file into a [`TypeSchema`]. Shared by the disk loader
/// ([`Registry::load`]) and the FUSE-projection loader ([`Registry::load_from_fs`])
/// so both interpret `globs` and the pinned type const identically. `src_label`
/// names the source in error messages.
fn parse_schema(name: String, raw: &str, src_label: &str) -> Result<TypeSchema> {
    let schema: serde_json::Value =
        serde_json::from_str(raw).with_context(|| format!("parsing schema {src_label}"))?;

    // Extract glob patterns (top-level "globs": ["...", ...]).
    let mut builder = GlobSetBuilder::new();
    let mut has_globs = false;
    if let Some(arr) = schema.get("globs").and_then(|g| g.as_array()) {
        for g in arr {
            if let Some(pat) = g.as_str() {
                // literal_separator(true): `*` does not cross `/`, so `*.md` is
                // root-only and `**` is needed to descend.
                let glob = GlobBuilder::new(pat)
                    .literal_separator(true)
                    .build()
                    .with_context(|| format!("invalid glob {pat:?} in {src_label}"))?;
                builder.add(glob);
                has_globs = true;
            }
        }
    }
    let globs = builder.build().context("building glob set")?;

    // Extract the pinned type const, if the schema declares one.
    let type_const = schema
        .pointer("/properties/type/const")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(TypeSchema {
        name,
        schema,
        type_const,
        globs,
        has_globs,
    })
}

pub struct Registry {
    schemas: Vec<TypeSchema>,
}

impl Registry {
    /// Load every `*.json` under `<store>/.types`. A store with no `.types`
    /// directory yields an empty registry.
    pub fn load(store_root: &Path) -> Result<Self> {
        let dir = store_root.join(".types");
        let mut schemas = Vec::new();
        if !dir.is_dir() {
            return Ok(Self { schemas });
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading type registry {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .collect();
        entries.sort();

        for path in entries {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading schema {}", path.display()))?;
            schemas.push(parse_schema(name, &raw, &path.display().to_string())?);
        }
        Ok(Self { schemas })
    }

    /// Load `.types/*.json` **through the volume** (libjfs), not local disk.
    /// Under the per-folder-vaults model the type registry is vault content
    /// (`<vault>/.types/`, versioned like any file): the mount reads it at
    /// startup to build the validation gate, so attaching to an existing vault
    /// on a second machine picks up its schemas without any local copy. A vault
    /// with no `.types/` directory (a fresh one) yields an empty registry —
    /// pass-through until schemas are written in.
    #[cfg(feature = "mount")]
    pub fn load_from_fs(fs: &crate::jfs::Fs) -> Result<Self> {
        let mut schemas = Vec::new();
        if !fs.exists("/.types") {
            return Ok(Self { schemas });
        }
        let mut entries: Vec<String> = fs
            .readdir("/.types")
            .context("reading /.types/ from the vault")?
            .into_iter()
            .filter(|e| !e.is_dir())
            .map(|e| e.name)
            .filter(|n| n.ends_with(".json"))
            .collect();
        entries.sort();
        for file in entries {
            let path = format!("/.types/{file}");
            let bytes = fs
                .read_all(&path)
                .with_context(|| format!("reading schema {path}"))?;
            let raw = String::from_utf8(bytes)
                .with_context(|| format!("schema {path} is not valid UTF-8"))?;
            let name = file.strip_suffix(".json").unwrap_or(&file).to_string();
            schemas.push(parse_schema(name, &raw, &path)?);
        }
        Ok(Self { schemas })
    }

    /// A registry with no schemas — nothing is governed, so the write path
    /// gates nothing (pure pass-through). Used when `trove mount` runs without
    /// a `--types` directory.
    pub fn empty() -> Self {
        Self { schemas: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty()
    }

    /// Does any schema's glob govern this path? Used for files that fail to
    /// parse — we can't read their `type`, so a path-level glob match is the
    /// only basis for deciding whether the parse error is a finding (a governed
    /// path) or a skip (a template / vendored dir nothing claims).
    pub fn path_is_governed(&self, rel: &Path) -> bool {
        self.schemas.iter().any(|s| s.has_globs && s.glob_matches(rel))
    }

    /// Could any schema *possibly* claim this path, once its content (and `type`)
    /// is known? Mirrors the candidacy filter in [`select`]: a glob match, or a
    /// schema with no globs (which applies on any path). When false, the file can
    /// never be validated — the mount streams it straight through unbuffered, so
    /// binary and ungoverned files stay cheap regardless of size.
    pub fn may_govern(&self, rel: &Path) -> bool {
        self.schemas
            .iter()
            .any(|s| !s.has_globs || s.glob_matches(rel))
    }

    /// Select the schemas that govern a file at `rel` with declared `file_type`.
    ///
    /// 1. Candidates are schemas whose globs match the path (or, for a schema
    ///    with no globs, any path).
    /// 2. A candidate *claims* the file when:
    ///    - it pins no type const → applies by path alone (the glob *is* the
    ///      identifier, e.g. `links/reference/**`); or
    ///    - it pins a const and the file's declared type matches it.
    ///
    /// A schema that pins a const will NOT claim a file with no type field.
    /// This is deliberate: a broad glob like `*.md` (root) is shared by many
    /// types (person, concept, daily notes), so a typeless root note must not
    /// be force-validated as a person. Type-const schemas need the type field.
    pub fn select(&self, rel: &Path, file_type: Option<&str>) -> Vec<&TypeSchema> {
        self.schemas
            .iter()
            .filter(|s| !s.has_globs || s.glob_matches(rel))
            .filter(|s| match (&s.type_const, file_type) {
                (Some(c), Some(t)) => c == t,
                (Some(_), None) => false,
                (None, _) => true,
            })
            .collect()
    }
}

// -----------------------------------------------------------------------------
// Schema lint
//
// Walk `<store>/.types/*.json` independently of `Registry::load` and surface
// every problem with each schema, rather than bailing on the first. Used by
// `trove check` to fail fast before walking files, and by `trove doctor` as one
// of its Validation-section checks.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LintLevel {
    /// Schema is broken: unparseable JSON, invalid as a JSON Schema, or an
    /// invalid glob pattern. `trove check` refuses to run when any are present.
    Error,
    /// Schema would never claim anything (no globs, no type-const). Not fatal —
    /// the schema just isn't load-bearing.
    Warning,
}

#[derive(Debug, Clone)]
pub struct LintFinding {
    pub schema_name: String,
    pub path: PathBuf,
    pub level: LintLevel,
    pub message: String,
}

#[derive(Debug, Default, Clone)]
pub struct LintReport {
    pub schemas_dir: PathBuf,
    /// Count of `*.json` files we *found* (regardless of validity).
    pub schemas_present: usize,
    pub findings: Vec<LintFinding>,
}

impl LintReport {
    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.level == LintLevel::Error)
    }
    pub fn errors(&self) -> impl Iterator<Item = &LintFinding> {
        self.findings.iter().filter(|f| f.level == LintLevel::Error)
    }
    pub fn warnings(&self) -> impl Iterator<Item = &LintFinding> {
        self.findings.iter().filter(|f| f.level == LintLevel::Warning)
    }
}

/// Walk every `.types/*.json` and check it for well-formedness. Catches:
///
/// - JSON parse failure
/// - Invalid glob pattern in `globs`
/// - Schema doesn't compile as JSON Schema (via `jsonschema::JSONSchema::compile`)
/// - Schema would govern nothing (no globs AND no `properties.type.const`) —
///   warning, not error
///
/// An absent `.types/` directory returns an empty report (not a finding) so
/// callers can decide whether that's a problem in their context. Findings are
/// sorted: errors first, then warnings, alphabetically by schema name within
/// each group.
pub fn lint(store_root: &Path) -> LintReport {
    let dir = store_root.join(".types");
    let mut report = LintReport {
        schemas_dir: dir.clone(),
        schemas_present: 0,
        findings: Vec::new(),
    };
    if !dir.is_dir() {
        return report;
    }
    let Ok(read) = std::fs::read_dir(&dir) else {
        return report;
    };
    let mut entries: Vec<PathBuf> = read
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    entries.sort();

    for path in entries {
        report.schemas_present += 1;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        lint_one(&mut report, &path, &name);
    }

    report.findings.sort_by(|a, b| match (&a.level, &b.level) {
        (LintLevel::Error, LintLevel::Warning) => std::cmp::Ordering::Less,
        (LintLevel::Warning, LintLevel::Error) => std::cmp::Ordering::Greater,
        _ => a.schema_name.cmp(&b.schema_name),
    });

    report
}

fn lint_one(report: &mut LintReport, path: &Path, name: &str) {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            report.findings.push(LintFinding {
                schema_name: name.to_string(),
                path: path.to_path_buf(),
                level: LintLevel::Error,
                message: format!("cannot read schema file: {e}"),
            });
            return;
        }
    };
    let schema: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            report.findings.push(LintFinding {
                schema_name: name.to_string(),
                path: path.to_path_buf(),
                level: LintLevel::Error,
                message: format!("JSON parse failed: {e}"),
            });
            return;
        }
    };

    // Globs (if declared) must each compile.
    let mut had_globs = false;
    if let Some(arr) = schema.get("globs").and_then(|g| g.as_array()) {
        for g in arr {
            let Some(pat) = g.as_str() else { continue };
            had_globs = true;
            if let Err(e) = GlobBuilder::new(pat).literal_separator(true).build() {
                report.findings.push(LintFinding {
                    schema_name: name.to_string(),
                    path: path.to_path_buf(),
                    level: LintLevel::Error,
                    message: format!("invalid glob {pat:?}: {e}"),
                });
            }
        }
    }

    // Schema must itself compile as a JSON Schema.
    if let Err(e) = jsonschema::JSONSchema::compile(&schema) {
        report.findings.push(LintFinding {
            schema_name: name.to_string(),
            path: path.to_path_buf(),
            level: LintLevel::Error,
            message: format!("schema does not compile as JSON Schema: {e}"),
        });
    }

    // Warning: a schema with no globs AND no type-const can only claim files by
    // the back-compat "applies on any path with matching type" path, but with no
    // type-const that becomes "applies to any file ever". Almost always a bug.
    let has_type_const = schema.pointer("/properties/type/const").is_some();
    if !had_globs && !has_type_const {
        report.findings.push(LintFinding {
            schema_name: name.to_string(),
            path: path.to_path_buf(),
            level: LintLevel::Warning,
            message: "schema has no `globs` and no `properties.type.const` — \
                      it would claim every file. Add globs or a type const."
                .to_string(),
        });
    }
}

#[cfg(test)]
mod lint_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Make a tmp store with `.types/<name>.json` files. Returns the store path.
    fn tmp_store(files: &[(&str, &str)]) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("trove-lint-{}-{}", std::process::id(), n));
        let types = dir.join(".types");
        std::fs::create_dir_all(&types).unwrap();
        for (name, body) in files {
            std::fs::write(types.join(format!("{name}.json")), body).unwrap();
        }
        dir
    }

    #[test]
    fn empty_registry_is_clean() {
        let store = tmp_store(&[]);
        let r = lint(&store);
        assert_eq!(r.schemas_present, 0);
        assert!(r.findings.is_empty());
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn absent_types_dir_is_not_a_finding() {
        let dir = std::env::temp_dir().join(format!("trove-lint-absent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = lint(&dir);
        assert_eq!(r.schemas_present, 0);
        assert!(r.findings.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bad_json_is_an_error() {
        let store = tmp_store(&[("broken", "this is not json {")]);
        let r = lint(&store);
        assert_eq!(r.schemas_present, 1);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].level, LintLevel::Error);
        assert!(r.findings[0].message.to_lowercase().contains("parse"));
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn schema_that_doesnt_compile_is_an_error() {
        // `"type": 42` is valid JSON but invalid JSON Schema.
        let store = tmp_store(&[(
            "bad-schema",
            r#"{"globs":["*.md"], "type": 42}"#,
        )]);
        let r = lint(&store);
        assert!(r.has_errors(), "expected an error finding; got {:?}", r.findings);
        assert!(
            r.errors().any(|f| f.message.contains("does not compile")),
            "expected 'does not compile' message; got {:?}",
            r.findings
        );
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn bad_glob_is_an_error() {
        let store = tmp_store(&[(
            "bad-glob",
            r#"{"globs":["[unclosed"], "type":"object", "properties":{"type":{"const":"x"}}}"#,
        )]);
        let r = lint(&store);
        assert!(r.has_errors(), "expected an error finding; got {:?}", r.findings);
        assert!(
            r.errors().any(|f| f.message.contains("invalid glob")),
            "expected 'invalid glob' message; got {:?}",
            r.findings
        );
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn no_globs_and_no_type_const_is_a_warning() {
        let store = tmp_store(&[(
            "loose",
            r#"{"type":"object", "properties":{"name":{"type":"string"}}}"#,
        )]);
        let r = lint(&store);
        assert!(!r.has_errors());
        assert_eq!(r.warnings().count(), 1);
        let w = r.warnings().next().unwrap();
        assert!(w.message.contains("globs"), "warning message: {}", w.message);
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn errors_sort_before_warnings_then_alphabetical() {
        let store = tmp_store(&[
            ("z-bad", "{not json"),
            ("a-bad", "{also not json"),
            ("m-warn", r#"{"type":"object"}"#),
        ]);
        let r = lint(&store);
        let names: Vec<_> = r.findings.iter().map(|f| f.schema_name.clone()).collect();
        assert_eq!(names, vec!["a-bad", "z-bad", "m-warn"]);
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn well_formed_schema_is_clean() {
        let store = tmp_store(&[(
            "person",
            r#"{
                "globs": ["people/**.md"],
                "type": "object",
                "required": ["type", "name"],
                "properties": {
                    "type": {"const": "person"},
                    "name": {"type": "string"}
                }
            }"#,
        )]);
        let r = lint(&store);
        assert_eq!(r.schemas_present, 1);
        assert!(r.findings.is_empty(), "expected no findings; got {:?}", r.findings);
        std::fs::remove_dir_all(&store).ok();
    }
}
