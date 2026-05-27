//! Split a markdown file into its YAML frontmatter and body, and parse the
//! frontmatter into a JSON value so it can be validated against a JSON Schema.

use anyhow::{anyhow, Result};

pub struct Document {
    /// Frontmatter parsed to a JSON value (`Null` if the file has none).
    pub frontmatter: serde_json::Value,
    /// True when the file opened with a `---` fence (i.e. it claims to have
    /// frontmatter, even if that block turned out to be empty or malformed).
    pub had_fence: bool,
}

/// Parse a markdown document. A document has frontmatter when its very first
/// line is `---`; the block runs to the next `---` on its own line.
pub fn parse(raw: &str) -> Result<Document> {
    // Tolerate a leading BOM and trailing whitespace on the fence line.
    let text = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = text.lines();

    let first = lines.next().unwrap_or("");
    if first.trim_end() != "---" {
        return Ok(Document {
            frontmatter: serde_json::Value::Null,
            had_fence: false,
        });
    }

    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }

    if !closed {
        return Err(anyhow!("frontmatter fence opened with `---` but never closed"));
    }

    if yaml.trim().is_empty() {
        return Ok(Document {
            frontmatter: serde_json::Value::Object(Default::default()),
            had_fence: true,
        });
    }

    // YAML → JSON value, so jsonschema can validate it directly.
    let yaml_val: serde_yaml::Value =
        serde_yaml::from_str(&yaml).map_err(|e| anyhow!("invalid YAML frontmatter: {e}"))?;
    let json_val: serde_json::Value = serde_json::to_value(yaml_val)
        .map_err(|e| anyhow!("frontmatter could not be represented as JSON: {e}"))?;

    Ok(Document {
        frontmatter: json_val,
        had_fence: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter() {
        let doc = parse("# just a heading\n\nbody").unwrap();
        assert!(!doc.had_fence);
        assert!(doc.frontmatter.is_null());
    }

    #[test]
    fn simple_frontmatter() {
        let doc = parse("---\ntype: person\ndob: 2010-06-23\n---\nbody").unwrap();
        assert!(doc.had_fence);
        assert_eq!(doc.frontmatter["type"], "person");
    }

    #[test]
    fn unclosed_fence_errors() {
        assert!(parse("---\ntype: person\nbody without close").is_err());
    }
}
