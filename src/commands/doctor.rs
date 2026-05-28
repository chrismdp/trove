//! `trove doctor` — a full health check that prints a tidy ✅/❌ report grouped
//! into four sections: **Configuration** (resolved settings + their provenance),
//! **Secrets** (env-only), **Backend** (DB + pgvector + schema + JuiceFS), and
//! **Validation** (the `trove check` sweep over the configured store). Exits
//! non-zero if any check fails so it's usable in scripts/CI.
//!
//! `trove check` remains the standalone subcommand for the validation sweep;
//! `doctor` simply *invokes* it as one of its checks.

use crate::config::{self, Config};
use crate::jfs::Fs;
use crate::version::VersionStore;
use std::path::{Path, PathBuf};

/// One line of the report. `section` groups rows under a heading in the
/// printer; `name` is the row label.
pub struct Check {
    pub section: &'static str,
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    fn ok(section: &'static str, name: &'static str, detail: impl Into<String>) -> Self {
        Check { section, name, ok: true, detail: detail.into() }
    }
    fn fail(section: &'static str, name: &'static str, detail: impl Into<String>) -> Self {
        Check { section, name, ok: false, detail: detail.into() }
    }
}

pub const SECTION_CONFIG: &str = "Configuration";
pub const SECTION_SECRETS: &str = "Secrets";
pub const SECTION_BACKEND: &str = "Backend";
pub const SECTION_VALIDATION: &str = "Validation";

/// Section order for printing. Anything not listed appears last.
pub const SECTION_ORDER: &[&str] = &[
    SECTION_CONFIG,
    SECTION_SECRETS,
    SECTION_BACKEND,
    SECTION_VALIDATION,
];

fn env_present(var: &str) -> bool {
    std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false)
}

/// Truncate a long value for display. Keeps the start and tags `…`.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Mask the password segment of a `postgres://user:password@host…` URL. Leaves
/// non-postgres values untouched. Best-effort — does not try to parse the URL,
/// just spots the `:password@` between `://` and the next `@`.
fn mask_postgres(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else { return url.to_string() };
    let rest_start = scheme_end + 3;
    let rest = &url[rest_start..];
    let Some(at) = rest.find('@') else { return url.to_string() };
    let userinfo = &rest[..at];
    let Some(colon) = userinfo.find(':') else { return url.to_string() };
    let user = &userinfo[..colon];
    let after_at = &rest[at..]; // includes '@'
    format!("{}{}:***{}", &url[..rest_start], user, after_at)
}

/// Format a resolved value + source for display, masking + truncating.
fn render_value(name: &str, value: &str, source: &str) -> String {
    let v = if name == "versions_db" || name == "meta" {
        mask_postgres(value)
    } else {
        value.to_string()
    };
    format!("{} ({})", truncate(&v, 60), source)
}

/// Run all checks. `versions_db`/`volume`/`meta`/`cache`/`store` are the CLI
/// flag overrides (each falls back to env then `cfg`). Returns the report.
pub fn run(
    cfg: &Config,
    versions_db: Option<String>,
    volume: Option<String>,
    meta: Option<String>,
    cache: Option<PathBuf>,
    store: Option<PathBuf>,
) -> Vec<Check> {
    let mut checks = Vec::new();

    // --- Configuration: resolved values + provenance ---
    // Each field is resolved with the same precedence the real command would
    // use, then printed as `value (source)`. None of these are FAIL by
    // themselves; missing values just show "not set" so the user can see what's
    // wired up. Whether a missing value matters is reported by the relevant
    // Backend / Validation check below.
    let store_str = store.as_ref().map(|p| p.to_string_lossy().into_owned());
    let cache_str = cache.as_ref().map(|p| p.to_string_lossy().into_owned());

    let config_settings: [(&'static str, Option<String>, &'static str, Option<String>); 6] = [
        ("versions_db", versions_db.clone(), "TROVE_VERSIONS_DB", cfg.versions_db.clone()),
        ("volume",      volume.clone(),      "TROVE_VOLUME",      cfg.volume.clone()),
        ("meta",        meta.clone(),        "TROVE_META",        cfg.meta.clone()),
        ("cache",       cache_str.clone(),   "TROVE_CACHE",       cfg.cache.clone()),
        ("r2_bucket",   None,                "TROVE_R2_BUCKET",   cfg.r2_bucket.clone()),
        ("store",       store_str.clone(),   "TROVE_STORE",       cfg.store.clone()),
    ];
    for (name, flag, env, from_cfg) in config_settings {
        match config::resolve_with_source(flag, env, from_cfg, name) {
            Ok((value, source)) => {
                checks.push(Check::ok(SECTION_CONFIG, name, render_value(name, &value, source)));
            }
            Err(_) => {
                checks.push(Check::ok(SECTION_CONFIG, name, "not set".to_string()));
            }
        }
    }

    // config.toml exists?
    match Config::path() {
        Ok(p) => {
            let exists = p.exists();
            let detail = format!("{} ({})", p.display(), if exists { "found" } else { "missing" });
            checks.push(Check { section: SECTION_CONFIG, name: "config.toml", ok: exists, detail });
        }
        Err(e) => {
            checks.push(Check::fail(SECTION_CONFIG, "config.toml", format!("path error: {e:#}")));
        }
    }

    // LIBJFS_DIR — env if set, else the build-time default baked into build.rs.
    let libjfs_default = "/home/cp/code/trove/spike/juicefs/sdk/java/libjfs";
    let (libjfs_val, libjfs_src) = match std::env::var("LIBJFS_DIR").ok().filter(|s| !s.is_empty()) {
        Some(v) => (v, "env"),
        None => (libjfs_default.to_string(), "build default"),
    };
    checks.push(Check::ok(SECTION_CONFIG, "LIBJFS_DIR", format!("{} ({libjfs_src})", truncate(&libjfs_val, 60))));

    // schemas dir — how many `.types/*.json` files the configured store has.
    // Counts only; whether each is well-formed is the Validation section's job.
    if let Some(store_path) = store_str
        .clone()
        .or_else(|| std::env::var("TROVE_STORE").ok().filter(|s| !s.is_empty()))
        .or_else(|| cfg.store.clone())
    {
        let report = crate::types::lint(Path::new(&store_path));
        let detail = if report.schemas_dir.is_dir() {
            format!("{} schemas in {}", report.schemas_present, truncate(&report.schemas_dir.display().to_string(), 50))
        } else {
            format!("no .types/ dir at {}", truncate(&store_path, 50))
        };
        checks.push(Check { section: SECTION_CONFIG, name: "schemas", ok: report.schemas_dir.is_dir(), detail });
    } else {
        checks.push(Check::ok(SECTION_CONFIG, "schemas", "n/a (no store configured)".to_string()));
    }

    // --- Secrets (environment only; never the config file) ---
    checks.push(if env_present("OPENAI_API_KEY") {
        Check::ok(SECTION_SECRETS, "OPENAI_API_KEY", "set (needed for embed + search)")
    } else {
        Check::fail(SECTION_SECRETS, "OPENAI_API_KEY", "not set — embed/search will fail")
    });
    let r2 = env_present("R2_ACCESS_KEY_ID") && env_present("R2_SECRET_ACCESS_KEY");
    checks.push(if r2 {
        Check::ok(SECTION_SECRETS, "R2 credentials", "R2_ACCESS_KEY_ID + R2_SECRET_ACCESS_KEY set")
    } else {
        Check::fail(SECTION_SECRETS, "R2 credentials", "R2_ACCESS_KEY_ID and/or R2_SECRET_ACCESS_KEY missing")
    });

    // --- Backend: DB resolvable + reachable + pgvector + schema; JuiceFS ---
    match config::resolve(versions_db, "TROVE_VERSIONS_DB", cfg.versions_db.clone(), "versions DB URL") {
        Err(e) => checks.push(Check::fail(SECTION_BACKEND, "versions DB", e.to_string())),
        Ok(url) => match VersionStore::connect(&url) {
            Err(e) => checks.push(Check::fail(SECTION_BACKEND, "versions DB", format!("unreachable: {e:#}"))),
            Ok(mut vs) => {
                checks.push(Check::ok(SECTION_BACKEND, "versions DB", "reachable"));
                match vs.diagnostics() {
                    Err(e) => checks.push(Check::fail(SECTION_BACKEND, "schema", format!("query failed: {e:#}"))),
                    Ok((pgvector, missing)) => {
                        checks.push(if pgvector {
                            Check::ok(SECTION_BACKEND, "pgvector", "extension installed")
                        } else {
                            Check::fail(SECTION_BACKEND, "pgvector", "extension `vector` not installed (run migrations)")
                        });
                        checks.push(if missing.is_empty() {
                            Check::ok(SECTION_BACKEND, "schema tables", "blobs, file_versions, blob_chunks present")
                        } else {
                            Check::fail(SECTION_BACKEND, "schema tables", format!("missing: {} (run migrations)", missing.join(", ")))
                        });
                    }
                }
            }
        },
    }

    // JuiceFS backend — only attempted when volume + meta resolve.
    let vol = config::resolve(volume, "TROVE_VOLUME", cfg.volume.clone(), "volume");
    let met = config::resolve(meta, "TROVE_META", cfg.meta.clone(), "meta URL");
    match (vol, met) {
        (Ok(vol), Ok(met)) => {
            let cache_dir = cache
                .map(|c| c.to_string_lossy().into_owned())
                .or_else(|| cfg.cache.clone())
                .unwrap_or_else(|| "/tmp/trove-cache".to_string());
            match Fs::init(&vol, &met, &cache_dir) {
                Ok(_) => checks.push(Check::ok(SECTION_BACKEND, "JuiceFS backend", format!("libjfs + volume {vol:?} + object store OK"))),
                Err(e) => checks.push(Check::fail(SECTION_BACKEND, "JuiceFS backend", format!("{e:#}"))),
            }
        }
        _ => checks.push(Check::fail(SECTION_BACKEND, "JuiceFS backend", "volume/meta not configured — skipped (set them or run `trove install`)")),
    }

    // --- Validation: lint schemas, then run `trove check` over the store ---
    let resolved_store = store
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| std::env::var("TROVE_STORE").ok().filter(|s| !s.is_empty()))
        .or_else(|| cfg.store.clone());
    match resolved_store {
        None => {
            // Mirror the lint row's shape so the section structure stays
            // predictable even with no store configured.
            checks.push(Check::fail(
                SECTION_VALIDATION,
                "schema lint",
                "store not configured — pass --store, set TROVE_STORE, or run `trove install`",
            ));
            checks.push(Check::fail(
                SECTION_VALIDATION,
                "store validation",
                "skipped (no store configured)",
            ));
        }
        Some(s) => {
            let path = Path::new(&s);
            let lint = crate::types::lint(path);
            let errs = lint.errors().count();
            let warns = lint.warnings().count();

            // Schema lint row: one line per outcome, with the first few error
            // messages inlined so the user doesn't have to run `trove check` to
            // see what's broken.
            let lint_detail = match (errs, warns) {
                (0, 0) => format!("{} schemas (all well-formed)", lint.schemas_present),
                (0, w) => format!("{} schemas ({} warnings)", lint.schemas_present, w),
                (e, _) => {
                    let first: Vec<String> = lint
                        .errors()
                        .take(3)
                        .map(|f| format!("{}.json: {}", f.schema_name, f.message))
                        .collect();
                    let more = if e > 3 { format!(" (+{} more)", e - 3) } else { String::new() };
                    format!(
                        "{} schemas ({} errors){} — {}",
                        lint.schemas_present,
                        e,
                        more,
                        first.join("; ")
                    )
                }
            };
            if errs == 0 {
                checks.push(Check::ok(SECTION_VALIDATION, "schema lint", lint_detail));
            } else {
                checks.push(Check::fail(SECTION_VALIDATION, "schema lint", lint_detail));
            }

            // Store validation: skipped on lint failure (the registry won't load).
            if errs > 0 {
                checks.push(Check::fail(
                    SECTION_VALIDATION,
                    "store validation",
                    "skipped (schema lint failed)",
                ));
            } else {
                match crate::commands::check::run(path, true) {
                    Err(e) => checks.push(Check::fail(
                        SECTION_VALIDATION,
                        "store validation",
                        format!("{}: {e:#}", path.display()),
                    )),
                    Ok(sum) => {
                        let detail = format!(
                            "{} checked, {} valid, {} failed (store: {})",
                            sum.checked, sum.valid, sum.failed, truncate(&s, 40)
                        );
                        if sum.failed == 0 {
                            checks.push(Check::ok(SECTION_VALIDATION, "store validation", detail));
                        } else {
                            checks.push(Check::fail(SECTION_VALIDATION, "store validation", detail));
                        }
                    }
                }
            }
        }
    }

    checks
}

