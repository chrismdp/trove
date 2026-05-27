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
            let schema: serde_json::Value = serde_json::from_str(&raw)
                .with_context(|| format!("parsing schema {}", path.display()))?;

            // Extract glob patterns (top-level "globs": ["...", ...]).
            let mut builder = GlobSetBuilder::new();
            let mut has_globs = false;
            if let Some(arr) = schema.get("globs").and_then(|g| g.as_array()) {
                for g in arr {
                    if let Some(pat) = g.as_str() {
                        // literal_separator(true): `*` does not cross `/`, so
                        // `*.md` is root-only and `**` is needed to descend.
                        let glob = GlobBuilder::new(pat)
                            .literal_separator(true)
                            .build()
                            .with_context(|| {
                                format!("invalid glob {pat:?} in {}", path.display())
                            })?;
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

            schemas.push(TypeSchema {
                name,
                schema,
                type_const,
                globs,
                has_globs,
            });
        }
        Ok(Self { schemas })
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
