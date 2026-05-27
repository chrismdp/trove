//! The type registry. Each type is a JSON Schema living in `.types/<name>.json`
//! inside the store. A document's `type` frontmatter field selects which schema
//! it must satisfy. The registry is data in the store, not code — editing a
//! schema is how you migrate (writes self-heal lazily).

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

pub struct Registry {
    schemas: HashMap<String, serde_json::Value>,
}

impl Registry {
    /// Load every `*.json` under `<store>/.types`. A store with no `.types`
    /// directory yields an empty registry (nothing to validate against).
    pub fn load(store_root: &Path) -> Result<Self> {
        let dir = store_root.join(".types");
        let mut schemas = HashMap::new();
        if !dir.is_dir() {
            return Ok(Self { schemas });
        }
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("reading type registry {}", dir.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading schema {}", path.display()))?;
            let schema: serde_json::Value = serde_json::from_str(&raw)
                .with_context(|| format!("parsing schema {}", path.display()))?;
            schemas.insert(name, schema);
        }
        Ok(Self { schemas })
    }

    pub fn get(&self, type_name: &str) -> Option<&serde_json::Value> {
        self.schemas.get(type_name)
    }

    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty()
    }

    pub fn known_types(&self) -> Vec<&str> {
        self.schemas.keys().map(String::as_str).collect()
    }
}
