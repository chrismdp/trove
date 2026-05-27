//! Validate one document against the type registry. This is the core of "a
//! filesystem that talks back": a write is only well-formed if its frontmatter
//! satisfies the schema its `type` selects. The same check runs here on a
//! `trove check` sweep and, later, on the FUSE write path.

use crate::frontmatter::Document;
use crate::types::Registry;

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No `type` field, or a type with no registered schema — nothing to check.
    Untyped,
    Valid,
    Invalid(Vec<Violation>),
    /// File couldn't even be parsed (e.g. unclosed fence, broken YAML).
    Unparseable(String),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    /// JSON pointer into the frontmatter, e.g. `/dob`.
    pub instance_path: String,
    pub message: String,
}

/// Resolve a document's declared type and validate against its schema.
pub fn validate(doc: &Document, registry: &Registry) -> Verdict {
    let type_name = doc
        .frontmatter
        .get("type")
        .and_then(|v| v.as_str());

    let Some(type_name) = type_name else {
        return Verdict::Untyped;
    };
    let Some(schema) = registry.get(type_name) else {
        return Verdict::Untyped;
    };

    let compiled = match jsonschema::JSONSchema::compile(schema) {
        Ok(c) => c,
        Err(e) => {
            return Verdict::Invalid(vec![Violation {
                instance_path: String::new(),
                message: format!("schema for `{type_name}` is itself invalid: {e}"),
            }])
        }
    };

    let result = compiled.validate(&doc.frontmatter);
    match result {
        Ok(()) => Verdict::Valid,
        Err(errors) => {
            let violations = errors
                .map(|e| Violation {
                    instance_path: {
                        let p = e.instance_path.to_string();
                        if p.is_empty() { "(root)".into() } else { p }
                    },
                    message: e.to_string(),
                })
                .collect();
            Verdict::Invalid(violations)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter;

    fn registry_with(name: &str, schema: serde_json::Value) -> Registry {
        // Build a registry by writing to a temp store.
        let dir = std::env::temp_dir().join(format!("trove-test-{}", std::process::id()));
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
                "type": "object",
                "required": ["type"],
                "properties": { "dob": { "type": "string" } }
            }),
        );
        let doc = frontmatter::parse("---\ntype: person\ndob: \"2010-06-23\"\n---\n").unwrap();
        assert_eq!(validate(&doc, &reg), Verdict::Valid);
    }

    #[test]
    fn wrong_field_type_flagged() {
        let reg = registry_with(
            "person",
            serde_json::json!({
                "type": "object",
                "properties": { "dob": { "type": "string" } }
            }),
        );
        // dob parses as an integer-ish scalar, not a string.
        let doc = frontmatter::parse("---\ntype: person\ndob: 42\n---\n").unwrap();
        matches!(validate(&doc, &reg), Verdict::Invalid(_));
    }
}
