//! `trove doctor` — a preflight that checks everything Trove needs is wired up,
//! and prints a tidy ✅/❌ report. Each check is independent and best-effort; the
//! command exits non-zero if any check fails so it's usable in scripts/CI.
//!
//! Checks, in order: secrets present (env only), version DB reachable + pgvector
//! + the migration tables, and the JuiceFS backend (libjfs + volume + object
//! store) — the last one, when it passes, proves the whole storage path works
//! (libjfs loads, the meta DB answers, and R2/the object store is reachable).

use crate::config::{self, Config};
use crate::jfs::Fs;
use crate::version::VersionStore;
use std::path::PathBuf;

/// One line of the report.
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Check { name, ok: true, detail: detail.into() }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Check { name, ok: false, detail: detail.into() }
    }
}

fn env_present(var: &str) -> bool {
    std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false)
}

/// Run all checks. `versions_db`/`volume`/`meta`/`cache` are the CLI flag
/// overrides (each falls back to env then `cfg`). Returns the report.
pub fn run(
    cfg: &Config,
    versions_db: Option<String>,
    volume: Option<String>,
    meta: Option<String>,
    cache: Option<PathBuf>,
) -> Vec<Check> {
    let mut checks = Vec::new();

    // --- Secrets (environment only; never the config file) ---
    checks.push(if env_present("OPENAI_API_KEY") {
        Check::ok("OPENAI_API_KEY", "set (needed for embed + search)")
    } else {
        Check::fail("OPENAI_API_KEY", "not set — embed/search will fail")
    });
    let r2 = env_present("R2_ACCESS_KEY_ID") && env_present("R2_SECRET_ACCESS_KEY");
    checks.push(if r2 {
        Check::ok("R2 credentials", "R2_ACCESS_KEY_ID + R2_SECRET_ACCESS_KEY set")
    } else {
        Check::fail("R2 credentials", "R2_ACCESS_KEY_ID and/or R2_SECRET_ACCESS_KEY missing")
    });

    // --- Version DB: resolvable, reachable, pgvector, tables ---
    match config::resolve(versions_db, "TROVE_VERSIONS_DB", cfg.versions_db.clone(), "versions DB URL") {
        Err(e) => checks.push(Check::fail("versions DB", e.to_string())),
        Ok(url) => match VersionStore::connect(&url) {
            Err(e) => checks.push(Check::fail("versions DB", format!("unreachable: {e:#}"))),
            Ok(mut vs) => {
                checks.push(Check::ok("versions DB", "reachable"));
                match vs.diagnostics() {
                    Err(e) => checks.push(Check::fail("schema", format!("query failed: {e:#}"))),
                    Ok((pgvector, missing)) => {
                        checks.push(if pgvector {
                            Check::ok("pgvector", "extension installed")
                        } else {
                            Check::fail("pgvector", "extension `vector` not installed (run migrations)")
                        });
                        checks.push(if missing.is_empty() {
                            Check::ok("schema tables", "blobs, file_versions, blob_chunks present")
                        } else {
                            Check::fail("schema tables", format!("missing: {} (run migrations)", missing.join(", ")))
                        });
                    }
                }
            }
        },
    }

    // --- JuiceFS backend (libjfs + volume + object store) ---
    // Only attempted when a volume + meta resolve; a success proves libjfs loads,
    // the meta DB answers, and the object store (R2) is reachable.
    let vol = config::resolve(volume, "TROVE_VOLUME", cfg.volume.clone(), "volume");
    let met = config::resolve(meta, "TROVE_META", cfg.meta.clone(), "meta URL");
    match (vol, met) {
        (Ok(vol), Ok(met)) => {
            let cache = cache
                .map(|c| c.to_string_lossy().into_owned())
                .or_else(|| cfg.cache.clone())
                .unwrap_or_else(|| "/tmp/trove-cache".to_string());
            match Fs::init(&vol, &met, &cache) {
                Ok(_) => checks.push(Check::ok("JuiceFS backend", format!("libjfs + volume {vol:?} + object store OK"))),
                Err(e) => checks.push(Check::fail("JuiceFS backend", format!("{e:#}"))),
            }
        }
        _ => checks.push(Check::fail("JuiceFS backend", "volume/meta not configured — skipped (set them or run `trove install`)")),
    }

    checks
}
