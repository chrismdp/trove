//! Trove config — non-secret connection settings persisted to
//! `~/.config/trove/config.toml` so the common commands don't need their flags
//! every time. **Secrets never live here** (`OPENAI_API_KEY`, R2 keys stay in
//! the environment / 1Password); the config holds only URLs and names.
//!
//! Precedence for any setting: explicit flag > environment variable > config
//! file. [`resolve`] applies it. `trove install` writes the file; everything
//! else reads it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Postgres URL for the version chain + embeddings (also JuiceFS `--meta`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub versions_db: Option<String>,
    /// JuiceFS volume name.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub volume: Option<String>,
    /// JuiceFS metadata engine URL (usually the same as `versions_db`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub meta: Option<String>,
    /// Local block-cache directory.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cache: Option<String>,
    /// R2/S3 bucket (reference for `trove doctor`; not a secret).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub r2_bucket: Option<String>,
}

impl Config {
    /// `$XDG_CONFIG_HOME/trove/config.toml`, falling back to `~/.config/trove/`.
    pub fn path() -> Result<PathBuf> {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
            .context("neither XDG_CONFIG_HOME nor HOME is set")?;
        Ok(base.join("trove").join("config.toml"))
    }

    /// Load the config file if present; an absent file is an empty config (not
    /// an error — every setting can still come from a flag or env var).
    pub fn load() -> Config {
        let Ok(path) = Self::path() else { return Config::default() };
        let Ok(text) = std::fs::read_to_string(&path) else { return Config::default() };
        toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("trove: ignoring malformed {}: {e}", path.display());
            Config::default()
        })
    }

    /// Write the config to its path, creating the directory if needed.
    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialising config")?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }
}

/// Resolve a setting by precedence: explicit `flag` > environment `env_var` >
/// config-file `from_cfg`. Errors (naming the setting) when none is set.
pub fn resolve(
    flag: Option<String>,
    env_var: &str,
    from_cfg: Option<String>,
    name: &str,
) -> Result<String> {
    flag.filter(|s| !s.is_empty())
        .or_else(|| std::env::var(env_var).ok().filter(|s| !s.is_empty()))
        .or(from_cfg)
        .with_context(|| {
            format!("no {name} — pass the flag, set {env_var}, or run `trove install`")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_beats_env_beats_config() {
        std::env::set_var("TROVE_TEST_X", "from-env");
        let r = resolve(Some("from-flag".into()), "TROVE_TEST_X", Some("from-cfg".into()), "x").unwrap();
        assert_eq!(r, "from-flag");
        let r = resolve(None, "TROVE_TEST_X", Some("from-cfg".into()), "x").unwrap();
        assert_eq!(r, "from-env");
        std::env::remove_var("TROVE_TEST_X");
        let r = resolve(None, "TROVE_TEST_X", Some("from-cfg".into()), "x").unwrap();
        assert_eq!(r, "from-cfg");
        assert!(resolve(None, "TROVE_TEST_X", None, "x").is_err());
    }

    #[test]
    fn empty_flag_and_env_are_ignored() {
        std::env::set_var("TROVE_TEST_Y", "");
        let r = resolve(Some(String::new()), "TROVE_TEST_Y", Some("cfg".into()), "y").unwrap();
        assert_eq!(r, "cfg", "empty flag and empty env fall through to config");
        std::env::remove_var("TROVE_TEST_Y");
    }

    #[test]
    fn round_trips_toml_without_secrets() {
        let c = Config {
            versions_db: Some("postgres://x".into()),
            volume: Some("vol".into()),
            meta: None,
            cache: Some("/tmp/c".into()),
            r2_bucket: Some("trove".into()),
        };
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(text.contains("versions_db"));
        assert!(!text.contains("meta"), "None fields are omitted");
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.versions_db.as_deref(), Some("postgres://x"));
        assert_eq!(back.meta, None);
    }
}
