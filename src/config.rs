//! Trove config. `trove init` writes shared machine credentials to
//! `~/.config/trove/credentials.toml` and per-folder volume files to
//! `~/.config/trove/volumes/<volume>.toml`; commands resolve the active volume
//! from the current working directory.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    /// Path to the vault root (used by doctor's validation sweep and as the default --types for mount).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub store: Option<String>,
    /// Path to a local mirror directory. When set, `trove backup` writes here by default.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub backup_dir: Option<String>,
    /// Postgres schema that isolates this volume's metadata from `public` and
    /// lets one database host many volumes. Set by `trove init` from the folder
    /// name; when absent it's derived on the fly via [`schema_for`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub schema: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub versions_db: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub r2_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub r2_access_key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub r2_secret_access_key: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct VolumeConfig {
    pub bucket: String,
    pub schema: String,
    pub mountpoint: String,
    pub cache: String,
}

impl Config {
    pub fn credentials_path() -> Result<PathBuf> {
        Ok(config_dir()?.join("credentials.toml"))
    }

    pub fn volumes_dir() -> Result<PathBuf> {
        Ok(config_dir()?.join("volumes"))
    }

    pub fn volume_path(volume: &str) -> Result<PathBuf> {
        Ok(Self::volumes_dir()?.join(format!("{volume}.toml")))
    }

    /// Load the config file if present; an absent file is an empty config (not
    /// an error — every setting can still come from a flag or env var).
    pub fn load() -> Config {
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(cfg) = Self::load_for_dir(&cwd) {
                return cfg;
            }
        }
        Config::default()
    }

    pub fn load_for_dir(cwd: &Path) -> Option<Config> {
        let creds = Credentials::load();
        let dir = Self::volumes_dir().ok()?;
        let entries = std::fs::read_dir(dir).ok()?;
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut best: Option<(usize, String, VolumeConfig)> = None;
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(vol) = toml::from_str::<VolumeConfig>(&text) else {
                continue;
            };
            let mount = PathBuf::from(&vol.mountpoint)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&vol.mountpoint));
            if !cwd.starts_with(&mount) {
                continue;
            }
            let depth = mount.components().count();
            let Some(name) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            if best.as_ref().map(|(d, _, _)| depth > *d).unwrap_or(true) {
                best = Some((depth, name, vol));
            }
        }
        let (_, volume, vol) = best?;
        Some(Config {
            versions_db: creds.versions_db,
            volume: Some(volume),
            meta: None,
            cache: Some(vol.cache),
            r2_bucket: Some(vol.bucket),
            store: Some(vol.mountpoint),
            backup_dir: None,
            schema: Some(vol.schema),
        })
    }

    /// The schema this install's metadata lives in: the stored `schema`, else
    /// derived from the volume name. `None` only when no volume is known yet.
    pub fn schema_name(&self) -> Option<String> {
        self.schema
            .clone()
            .or_else(|| self.volume.as_deref().map(schema_for))
    }
}

impl Credentials {
    pub fn load() -> Credentials {
        let Ok(path) = Config::credentials_path() else {
            return Credentials::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Credentials::default();
        };
        toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("trove: ignoring malformed {}: {e}", path.display());
            Credentials::default()
        })
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = Config::credentials_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialising credentials")?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&path, perms)?;
        }
        Ok(path)
    }
}

impl VolumeConfig {
    pub fn save(&self, volume: &str) -> Result<PathBuf> {
        let path = Config::volume_path(volume)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialising volume config")?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }
}

fn config_dir() -> Result<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("neither XDG_CONFIG_HOME nor HOME is set")?;
    Ok(base.join("trove"))
}

/// Resolve a setting by precedence: explicit `flag` > environment `env_var` >
/// config-file `from_cfg`. Errors (naming the setting) when none is set.
pub fn resolve(
    flag: Option<String>,
    env_var: &str,
    from_cfg: Option<String>,
    name: &str,
) -> Result<String> {
    resolve_with_source(flag, env_var, from_cfg, name).map(|(v, _)| v)
}

/// Same as [`resolve`] but also returns where the value came from: `"flag"`,
/// `"env"`, or `"config"`. Used by `trove doctor` to show provenance.
pub fn resolve_with_source(
    flag: Option<String>,
    env_var: &str,
    from_cfg: Option<String>,
    name: &str,
) -> Result<(String, &'static str)> {
    if let Some(v) = flag.filter(|s| !s.is_empty()) {
        return Ok((v, "flag"));
    }
    if let Some(v) = std::env::var(env_var).ok().filter(|s| !s.is_empty()) {
        return Ok((v, "env"));
    }
    if let Some(v) = from_cfg {
        return Ok((v, "config"));
    }
    Err(anyhow::anyhow!(
        "no {name} — pass the flag, set {env_var}, or run `trove init` in the vault folder"
    ))
}

/// Derive the Postgres schema that isolates a volume's metadata from `public`.
/// Folder-vault names treat `-`, `_`, and case as equivalent, then emit the
/// Postgres-native underscore form under the `trove_` namespace.
pub fn schema_for(volume: &str) -> String {
    let token = normalise_volume_name(volume).unwrap_or_else(|_| "default".to_string());
    let mut s = format!("trove_{}", token.replace('-', "_"));
    s.truncate(63);
    s
}

pub fn bucket_for(volume: &str) -> Result<String> {
    Ok(format!("trove-{}", normalise_volume_name(volume)?))
}

pub fn normalise_volume_name(name: &str) -> Result<String> {
    let raw = name.trim();
    if raw.is_empty() {
        bail!("volume name is empty");
    }
    let mut out = String::new();
    let mut prev_sep = true;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_sep = false;
        } else if c == '-' || c == '_' {
            if !prev_sep {
                out.push('-');
                prev_sep = true;
            }
        } else {
            bail!("invalid volume name `{name}` — use letters, numbers, '-' or '_'");
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        bail!("invalid volume name `{name}`");
    }
    Ok(out)
}

pub fn canonical_r2_bucket(endpoint: &str, bucket: &str) -> Result<String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    let bucket = bucket.trim().trim_matches('/');
    if endpoint.is_empty() {
        bail!("R2 endpoint is required");
    }
    if bucket.is_empty() {
        bail!("bucket name is required");
    }
    if endpoint.contains("://") {
        Ok(format!("{endpoint}/{bucket}"))
    } else {
        Ok(format!(
            "https://{}.r2.cloudflarestorage.com/{bucket}",
            endpoint.trim_start_matches("https://")
        ))
    }
}

pub fn r2_endpoint_from_bucket_input(
    input: &str,
    expected_bucket: &str,
) -> Result<(String, String)> {
    let input = input.trim().trim_end_matches('/');
    if input.is_empty() {
        bail!("R2 bucket endpoint is empty");
    }
    let expected_bucket = expected_bucket.trim();

    let Some(rest) = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
    else {
        if input == expected_bucket {
            bail!("bare bucket `{input}` needs an R2 account endpoint too");
        }
        return canonical_r2_bucket(input, expected_bucket).map(|bucket| {
            (
                format!("https://{}.r2.cloudflarestorage.com", input),
                bucket,
            )
        });
    };

    let scheme = if input.starts_with("http://") {
        "http"
    } else {
        "https"
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let suffix = ".r2.cloudflarestorage.com";

    if let Some(bucket) = path.split('/').find(|s| !s.is_empty()) {
        if bucket != expected_bucket {
            bail!("R2 endpoint names bucket `{bucket}`, expected `{expected_bucket}` from the folder name");
        }
        let endpoint = format!("{scheme}://{host}");
        return Ok((endpoint.clone(), format!("{endpoint}/{expected_bucket}")));
    }

    if let Some(prefix) = host.strip_suffix(suffix) {
        if let Some((bucket, account)) = prefix.split_once('.') {
            if bucket != expected_bucket {
                bail!("R2 endpoint names bucket `{bucket}`, expected `{expected_bucket}` from the folder name");
            }
            let endpoint = format!("{scheme}://{account}{suffix}");
            return Ok((endpoint.clone(), format!("{endpoint}/{expected_bucket}")));
        }
    }

    let endpoint = format!("{scheme}://{host}");
    Ok((endpoint.clone(), format!("{endpoint}/{expected_bucket}")))
}

/// Append JuiceFS's `search_path` query parameter to a Postgres meta URL, so its
/// `jfs_*` tables are created in the volume's schema instead of `public`.
/// JuiceFS honours a single schema here; trove's own connections set a fuller
/// `search_path` (schema, public, extensions) for the `vector` type.
pub fn with_search_path(meta_url: &str, schema: &str) -> String {
    let sep = if meta_url.contains('?') { '&' } else { '?' };
    format!("{meta_url}{sep}search_path={schema}")
}

/// Build the meta URL handed to JuiceFS/libjfs. Two fixups the rust-postgres
/// driver doesn't need but libjfs does:
/// 1. **Scheme**: libjfs's Postgres meta driver only recognises `postgres://` —
///    it rejects the `postgresql://` that Supabase (and many tools) emit with
///    `FATAL: Invalid meta driver: postgresql`. rust-postgres accepts both, so
///    trove's own connection is fine; only the JuiceFS hand-off needs this.
/// 2. **Schema**: point JuiceFS's `jfs_*` tables at the volume's schema.
pub fn juicefs_meta_url(meta_url: &str, schema: &str) -> String {
    let normalised = match meta_url.strip_prefix("postgresql://") {
        Some(rest) => format!("postgres://{rest}"),
        None => meta_url.to_string(),
    };
    with_search_path(&normalised, schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_beats_env_beats_config() {
        std::env::set_var("TROVE_TEST_X", "from-env");
        let r = resolve(
            Some("from-flag".into()),
            "TROVE_TEST_X",
            Some("from-cfg".into()),
            "x",
        )
        .unwrap();
        assert_eq!(r, "from-flag");
        let r = resolve(None, "TROVE_TEST_X", Some("from-cfg".into()), "x").unwrap();
        assert_eq!(r, "from-env");
        std::env::remove_var("TROVE_TEST_X");
        let r = resolve(None, "TROVE_TEST_X", Some("from-cfg".into()), "x").unwrap();
        assert_eq!(r, "from-cfg");
        assert!(resolve(None, "TROVE_TEST_X", None, "x").is_err());
    }

    #[test]
    fn resolve_with_source_labels_provenance() {
        std::env::set_var("TROVE_TEST_Z", "from-env");
        let (v, src) = resolve_with_source(
            Some("from-flag".into()),
            "TROVE_TEST_Z",
            Some("from-cfg".into()),
            "z",
        )
        .unwrap();
        assert_eq!((v.as_str(), src), ("from-flag", "flag"));
        let (v, src) =
            resolve_with_source(None, "TROVE_TEST_Z", Some("from-cfg".into()), "z").unwrap();
        assert_eq!((v.as_str(), src), ("from-env", "env"));
        std::env::remove_var("TROVE_TEST_Z");
        let (v, src) =
            resolve_with_source(None, "TROVE_TEST_Z", Some("from-cfg".into()), "z").unwrap();
        assert_eq!((v.as_str(), src), ("from-cfg", "config"));
        assert!(resolve_with_source(None, "TROVE_TEST_Z", None, "z").is_err());
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
            store: None,
            backup_dir: None,
            schema: None,
        };
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(text.contains("versions_db"));
        assert!(!text.contains("meta"), "None fields are omitted");
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.versions_db.as_deref(), Some("postgres://x"));
        assert_eq!(back.meta, None);
    }

    #[test]
    fn schema_for_sanitises_and_namespaces() {
        assert_eq!(schema_for("trove-test"), "trove_trove_test");
        assert_eq!(schema_for("test"), "trove_test");
        assert!(schema_for("My Vault!").starts_with("trove_default"));
        assert_eq!(schema_for("trove"), "trove_trove");
        assert!(schema_for(&"x".repeat(100)).len() <= 63);
    }

    #[test]
    fn normalise_volume_names_for_folder_vaults() {
        assert_eq!(normalise_volume_name("My-Notes").unwrap(), "my-notes");
        assert_eq!(normalise_volume_name("my_notes").unwrap(), "my-notes");
        assert!(normalise_volume_name("my notes").is_err());
        assert_eq!(bucket_for("my_notes").unwrap(), "trove-my-notes");
        assert_eq!(
            canonical_r2_bucket("https://acct.r2.cloudflarestorage.com", "trove-notes").unwrap(),
            "https://acct.r2.cloudflarestorage.com/trove-notes"
        );
        assert_eq!(
            r2_endpoint_from_bucket_input(
                "https://acct.r2.cloudflarestorage.com/trove-my-notes",
                "trove-my-notes"
            )
            .unwrap(),
            (
                "https://acct.r2.cloudflarestorage.com".to_string(),
                "https://acct.r2.cloudflarestorage.com/trove-my-notes".to_string()
            )
        );
        assert_eq!(
            r2_endpoint_from_bucket_input(
                "https://trove-my-notes.acct.r2.cloudflarestorage.com",
                "trove-my-notes"
            )
            .unwrap(),
            (
                "https://acct.r2.cloudflarestorage.com".to_string(),
                "https://acct.r2.cloudflarestorage.com/trove-my-notes".to_string()
            )
        );
    }

    #[test]
    fn schema_name_prefers_stored_then_derives() {
        let mut c = Config {
            volume: Some("notes".into()),
            ..Default::default()
        };
        assert_eq!(c.schema_name().as_deref(), Some("trove_notes")); // derived
        c.schema = Some("custom".into());
        assert_eq!(c.schema_name().as_deref(), Some("custom")); // stored wins
        let empty = Config::default();
        assert_eq!(empty.schema_name(), None); // no volume, no schema
    }

    #[test]
    fn with_search_path_picks_separator() {
        assert_eq!(
            with_search_path("postgres://h/db", "trove_x"),
            "postgres://h/db?search_path=trove_x"
        );
        assert_eq!(
            with_search_path("postgres://h/db?sslmode=require", "trove_x"),
            "postgres://h/db?sslmode=require&search_path=trove_x"
        );
    }

    #[test]
    fn juicefs_meta_url_normalises_scheme() {
        // libjfs only accepts `postgres://`, not the `postgresql://` Supabase emits.
        assert_eq!(
            juicefs_meta_url("postgresql://u:p@h:5432/db", "trove_x"),
            "postgres://u:p@h:5432/db?search_path=trove_x"
        );
        // Already-correct scheme is left alone.
        assert_eq!(
            juicefs_meta_url("postgres://h/db", "trove_x"),
            "postgres://h/db?search_path=trove_x"
        );
    }
}
