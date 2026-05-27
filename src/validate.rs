//! Validate a document's frontmatter against a specific schema. Schema
//! *selection* lives in the registry (glob + type-const); this module just
//! runs the chosen schema. This is the core of "a filesystem that talks back":
//! a write is well-formed only if its frontmatter satisfies the schema its
//! path (and type) select.

use crate::types::TypeSchema;

#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    /// JSON pointer into the frontmatter, e.g. `/dob`.
    pub instance_path: String,
    pub message: String,
}

/// Validate parsed frontmatter against one schema.
pub fn validate_against(
    frontmatter: &serde_json::Value,
    schema: &TypeSchema,
) -> Result<(), Vec<Violation>> {
    let compiled = match jsonschema::JSONSchema::compile(&schema.schema) {
        Ok(c) => c,
        Err(e) => {
            return Err(vec![Violation {
                instance_path: String::new(),
                message: format!("schema `{}` is itself invalid: {e}", schema.name),
            }])
        }
    };

    let result = compiled.validate(frontmatter);
    match result {
        Ok(()) => Ok(()),
        Err(errors) => {
            let violations: Vec<Violation> = errors
                .map(|e| Violation {
                    instance_path: {
                        let p = e.instance_path.to_string();
                        if p.is_empty() { "(root)".into() } else { p }
                    },
                    message: e.to_string(),
                })
                .collect();
            Err(violations)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter;
    use crate::types::Registry;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn registry_with(name: &str, schema: serde_json::Value) -> Registry {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("trove-vtest-{}-{}", std::process::id(), n));
        let types = dir.join(".types");
        std::fs::create_dir_all(&types).unwrap();
        std::fs::write(
            types.join(format!("{name}.json")),
            serde_json::to_string(&schema).unwrap(),
        )
        .unwrap();
        let r = Registry::load(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        r
    }

    #[test]
    fn valid_person() {
        let reg = registry_with(
            "person",
            serde_json::json!({
                "globs": ["*.md"],
                "type": "object",
                "required": ["type"],
                "properties": { "type": {"const": "person"}, "dob": { "type": "string" } }
            }),
        );
        let doc = frontmatter::parse("---\ntype: person\ndob: \"2010-06-23\"\n---\n").unwrap();
        let schemas = reg.select(std::path::Path::new("Rebekah.md"), Some("person"));
        assert_eq!(schemas.len(), 1);
        assert!(validate_against(&doc.frontmatter, schemas[0]).is_ok());
    }

    #[test]
    fn wrong_field_type_flagged() {
        let reg = registry_with(
            "person",
            serde_json::json!({
                "globs": ["*.md"],
                "type": "object",
                "properties": { "type": {"const": "person"}, "dob": { "type": "string" } }
            }),
        );
        let doc = frontmatter::parse("---\ntype: person\ndob: 42\n---\n").unwrap();
        let schemas = reg.select(std::path::Path::new("Bad.md"), Some("person"));
        assert!(validate_against(&doc.frontmatter, schemas[0]).is_err());
    }
}
