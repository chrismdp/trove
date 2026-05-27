//! Thin CLI shell over the `trove` library. All logic lives in the lib so it
//! can be tested directly (see `tests/`).
//!
//! Connection settings (`--versions-db`, `--volume`, `--meta`, `--cache`) all
//! fall back to env vars then `~/.config/trove/config.toml` (written by `trove
//! install`), so the common commands need no flags once configured. Secrets
//! (`OPENAI_API_KEY`, R2 keys) are read from the environment only — never the
//! config file.

use anyhow::Result;
use clap::{Parser, Subcommand};
use colored::Colorize;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "trove",
    version,
    about = "A filesystem that talks back — schema-on-write for knowledge stores"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate every typed markdown file in a store against its type schema.
    Check {
        /// Path to the store (defaults to the current directory).
        #[arg(default_value = ".")]
        store: PathBuf,
        /// Only print failures and the summary, not every passing file.
        #[arg(short, long)]
        quiet: bool,
    },

    /// Write ~/.config/trove/config.toml interactively (non-secret settings:
    /// volume, meta, versions_db, cache, r2 bucket). Secrets stay in the
    /// environment. After this, other commands work without the connection flags.
    Install,

    /// Mount a JuiceFS-backed Trove filesystem at <mountpoint> (foreground).
    #[cfg(feature = "mount")]
    Mount {
        /// Where to mount (an existing empty directory).
        mountpoint: PathBuf,
        /// JuiceFS volume name (must already be formatted). Falls back to config.
        #[arg(long)]
        volume: Option<String>,
        /// Metadata engine URL, e.g. postgres://… or sqlite3://… Falls back to config.
        #[arg(long)]
        meta: Option<String>,
        /// Local block-cache directory. Falls back to config, then /tmp/trove-cache.
        #[arg(long)]
        cache: Option<PathBuf>,
        /// Directory containing a `.types/` schema registry. When set, writes
        /// are validated against it (the "filesystem that talks back"); when
        /// omitted the mount is a plain pass-through.
        #[arg(long)]
        types: Option<PathBuf>,
        /// Postgres URL for the version chain. When resolvable (flag/env/config)
        /// every validated write is versioned best-effort; omit everywhere to
        /// disable versioning.
        #[arg(long)]
        versions_db: Option<String>,
        /// Embed each committed file as it's written (self-triggering, no cron).
        /// Requires a resolvable versions_db and OPENAI_API_KEY.
        #[arg(long)]
        embed: bool,
    },

    /// Embed un-embedded version blobs into `blob_chunks` for search. Reads
    /// content via libjfs, chunks by header, calls OpenAI. Needs OPENAI_API_KEY.
    #[cfg(feature = "mount")]
    Embed {
        #[arg(long)]
        volume: Option<String>,
        #[arg(long)]
        meta: Option<String>,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long)]
        versions_db: Option<String>,
        /// Run forever, sweeping every N seconds, instead of a single pass.
        #[arg(long)]
        watch: Option<u64>,
    },

    /// Semantic search over file contents: embed the query, then rank chunks by
    /// cosine similarity. Needs only the embeddings DB + OPENAI_API_KEY — no
    /// libjfs, no mount (search reads Postgres, not the filesystem).
    #[cfg(feature = "mount")]
    Search {
        /// The natural-language query.
        query: String,
        #[arg(long)]
        versions_db: Option<String>,
        /// How many results to return.
        #[arg(short = 'k', long, default_value_t = 10)]
        top_k: i64,
    },

    /// Serve a single-tenant, read-only HTTP view of the store (file list +
    /// semantic search + raw file content). Binds 127.0.0.1 only; front with
    /// nginx for external access. Needs the version DB, libjfs, and OPENAI_API_KEY.
    #[cfg(feature = "mount")]
    Server {
        /// Port to bind on 127.0.0.1.
        #[arg(long, default_value_t = 38080)]
        port: u16,
        #[arg(long)]
        volume: Option<String>,
        #[arg(long)]
        meta: Option<String>,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long)]
        versions_db: Option<String>,
    },

    /// Preflight: check secrets, the version DB + pgvector + schema, and the
    /// JuiceFS backend are all wired up. Exits non-zero if anything's missing.
    #[cfg(feature = "mount")]
    Doctor {
        #[arg(long)]
        versions_db: Option<String>,
        #[arg(long)]
        volume: Option<String>,
        #[arg(long)]
        meta: Option<String>,
        #[arg(long)]
        cache: Option<PathBuf>,
    },

    /// Show a path's version history, newest first. Needs only the version DB.
    #[cfg(feature = "mount")]
    Log {
        /// Path within the volume (e.g. /people/alice.md).
        path: String,
        #[arg(long)]
        versions_db: Option<String>,
    },

    /// Print a path's content at revision <rev> to stdout.
    #[cfg(feature = "mount")]
    Cat {
        /// Path within the volume.
        path: String,
        /// Revision number (see `trove log`).
        #[arg(long)]
        rev: i32,
        #[arg(long)]
        volume: Option<String>,
        #[arg(long)]
        meta: Option<String>,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long)]
        versions_db: Option<String>,
    },

    /// Unified line diff between two revisions of a path (rev_a -> rev_b).
    #[cfg(feature = "mount")]
    Diff {
        path: String,
        rev_a: i32,
        rev_b: i32,
        #[arg(long)]
        volume: Option<String>,
        #[arg(long)]
        meta: Option<String>,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long)]
        versions_db: Option<String>,
    },

    /// Restore a path to an earlier revision (recorded as a new revision, never
    /// a silent overwrite).
    #[cfg(feature = "mount")]
    Restore {
        path: String,
        rev: i32,
        #[arg(long)]
        volume: Option<String>,
        #[arg(long)]
        meta: Option<String>,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long)]
        versions_db: Option<String>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(failed) if failed == 0 => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("{}: {e:#}", "error".red().bold());
            ExitCode::FAILURE
        }
    }
}

/// Resolve + connect the version DB (flag > env `TROVE_VERSIONS_DB` > config).
#[cfg(feature = "mount")]
fn connect_versions(
    flag: Option<String>,
    cfg: &trove::config::Config,
) -> Result<trove::version::VersionStore> {
    let url = trove::config::resolve(flag, "TROVE_VERSIONS_DB", cfg.versions_db.clone(), "versions DB URL")?;
    trove::version::VersionStore::connect(&url)
}

/// Resolve + init the JuiceFS volume (volume/meta from flag > env > config;
/// cache from flag > env `TROVE_CACHE` > config > /tmp/trove-cache).
#[cfg(feature = "mount")]
fn init_fs(
    volume: Option<String>,
    meta: Option<String>,
    cache: Option<PathBuf>,
    cfg: &trove::config::Config,
) -> Result<trove::jfs::Fs> {
    let volume = trove::config::resolve(volume, "TROVE_VOLUME", cfg.volume.clone(), "volume name")?;
    let meta = trove::config::resolve(meta, "TROVE_META", cfg.meta.clone(), "meta URL")?;
    let cache = cache
        .map(|c| c.to_string_lossy().into_owned())
        .or_else(|| std::env::var("TROVE_CACHE").ok().filter(|s| !s.is_empty()))
        .or_else(|| cfg.cache.clone())
        .unwrap_or_else(|| "/tmp/trove-cache".to_string());
    trove::jfs::Fs::init(&volume, &meta, &cache)
}

/// `OPENAI_API_KEY` or a clear error.
#[cfg(feature = "mount")]
fn openai_key() -> Result<String> {
    std::env::var("OPENAI_API_KEY").map_err(|_| anyhow::anyhow!("OPENAI_API_KEY not set"))
}

/// Returns the number of failed files (0 = clean).
fn run() -> Result<usize> {
    let cli = Cli::parse();
    #[cfg(feature = "mount")]
    let cfg = trove::config::Config::load();
    match cli.command {
        Command::Check { store, quiet } => {
            let s = trove::commands::check::run(&store, quiet)?;
            println!(
                "\n{} {} checked · {} valid · {} untyped · {} {}",
                "trove:".bold(),
                s.checked,
                s.valid.to_string().green(),
                s.untyped,
                s.failed.to_string().red(),
                if s.failed == 1 { "failure" } else { "failures" }
            );
            Ok(s.failed)
        }

        Command::Install => {
            use std::io::{self, Write};
            let cur = trove::config::Config::load();
            let path = trove::config::Config::path()?;
            println!("{} writing {}", "trove install:".bold(), path.display());
            println!(
                "{}\n",
                "secrets stay in the environment, NOT this file: OPENAI_API_KEY, R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY".dimmed()
            );
            let ask = |label: &str, current: Option<&str>| -> io::Result<Option<String>> {
                match current {
                    Some(c) => print!("{label} [{}]: ", c.dimmed()),
                    None => print!("{label}: "),
                }
                io::stdout().flush()?;
                let mut line = String::new();
                io::stdin().read_line(&mut line)?;
                let line = line.trim();
                Ok(if line.is_empty() {
                    current.map(str::to_string)
                } else {
                    Some(line.to_string())
                })
            };
            let new = trove::config::Config {
                versions_db: ask("versions_db (postgres URL)", cur.versions_db.as_deref())?,
                volume: ask("volume name", cur.volume.as_deref())?,
                meta: ask("meta URL (often the same as versions_db)", cur.meta.as_deref())?,
                cache: ask("cache dir", cur.cache.as_deref().or(Some("/tmp/trove-cache")))?,
                r2_bucket: ask("r2 bucket (optional, for `trove doctor`)", cur.r2_bucket.as_deref())?,
            };
            let written = new.save()?;
            println!("\n{} wrote {}", "trove:".bold(), written.display());
            println!(
                "{}",
                "export OPENAI_API_KEY (and R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY for the backend) before mounting.".dimmed()
            );
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Mount { mountpoint, volume, meta, cache, types, versions_db, embed } => {
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let registry = match &types {
                Some(dir) => trove::types::Registry::load(dir)?,
                None => trove::types::Registry::empty(),
            };
            // Versioning is optional: resolve the URL without erroring (flag >
            // env > config); None anywhere = versioning off.
            let versions_url = versions_db
                .or_else(|| std::env::var("TROVE_VERSIONS_DB").ok().filter(|s| !s.is_empty()))
                .or_else(|| cfg.versions_db.clone());
            let versions = match &versions_url {
                Some(url) => Some(trove::version::VersionStore::connect(url)?),
                None => None,
            };
            // Optional self-triggering embedding: needs versioning + the key.
            let embed_tx = if embed {
                let url = versions_url
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("--embed requires a resolvable versions_db"))?;
                Some(trove::embed::spawn_embedder(url, openai_key()?)?)
            } else {
                None
            };
            println!(
                "{} mounting at {} ({}; versioning {}; embed {})",
                "trove:".bold(),
                mountpoint.display(),
                if registry.is_empty() {
                    "no validation".to_string()
                } else {
                    format!("validating via {}", types.as_ref().unwrap().display())
                },
                if versions.is_some() { "on" } else { "off" },
                if embed_tx.is_some() { "on" } else { "off" },
            );
            trove::mount::mount_blocking(fs, registry, versions, embed_tx, &mountpoint)?;
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Embed { volume, meta, cache, versions_db, watch } => {
            let api_key = openai_key()?;
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            match watch {
                Some(secs) => {
                    println!("{} embedding (watch, every {secs}s)…", "trove:".bold());
                    trove::embed::run_watch(&fs, &mut versions, &api_key, std::time::Duration::from_secs(secs))?;
                    Ok(0)
                }
                None => {
                    let n = trove::embed::run_once(&fs, &mut versions, &api_key)?;
                    println!("{} embedded {n} blob(s)", "trove:".bold());
                    Ok(0)
                }
            }
        }

        #[cfg(feature = "mount")]
        Command::Search { query, versions_db, top_k } => {
            let literal = trove::embed::embed_query_literal(&openai_key()?, &query)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            let hits = versions.search_chunks(&literal, top_k)?;
            if hits.is_empty() {
                println!("{} no matches for {query:?}", "trove:".bold());
                return Ok(0);
            }
            println!("{} {} result(s) for {query:?}", "trove:".bold(), hits.len());
            for h in &hits {
                // cosine similarity reads better than distance for a human.
                let score = format!("{:.3}", 1.0 - h.distance);
                let where_ = match &h.heading {
                    Some(h) => format!("{} {}", "›".dimmed(), h),
                    None => String::new(),
                };
                println!("  {}  {} {where_}", score.green(), h.path.bold());
            }
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Server { port, volume, meta, cache, versions_db } => {
            let api_key = openai_key()?;
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            trove::commands::server::run(&fs, &mut versions, &api_key, port)?;
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Doctor { versions_db, volume, meta, cache } => {
            let checks = trove::commands::doctor::run(&cfg, versions_db, volume, meta, cache);
            let failed = checks.iter().filter(|c| !c.ok).count();
            println!("{}", "trove doctor".bold());
            for c in &checks {
                let mark = if c.ok { "✓".green() } else { "✗".red() };
                println!("  {mark} {:<18} {}", c.name, c.detail.dimmed());
            }
            if failed == 0 {
                println!("\n{} all checks passed", "✓".green().bold());
            } else {
                println!("\n{} {failed} check(s) failed", "✗".red().bold());
            }
            Ok(failed)
        }

        #[cfg(feature = "mount")]
        Command::Log { path, versions_db } => {
            let mut versions = connect_versions(versions_db, &cfg)?;
            let entries = trove::commands::history::log(&mut versions, &path)?;
            if entries.is_empty() {
                println!("{} no versions for {path}", "trove:".bold());
                return Ok(0);
            }
            println!("{} {} ({} revision(s))", "trove:".bold(), path, entries.len());
            for v in &entries {
                let author = v.author.as_deref().unwrap_or("—");
                println!(
                    "  {} {}  {} bytes  {}  {}",
                    "rev".dimmed(),
                    v.rev.to_string().bold(),
                    v.size,
                    author,
                    &v.blob_hash[..12.min(v.blob_hash.len())].dimmed()
                );
            }
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Cat { path, rev, volume, meta, cache, versions_db } => {
            use std::io::Write;
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            let bytes = trove::commands::history::cat(&fs, &mut versions, &path, rev)?;
            std::io::stdout().write_all(&bytes)?;
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Diff { path, rev_a, rev_b, volume, meta, cache, versions_db } => {
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            let out = trove::commands::history::diff(&fs, &mut versions, &path, rev_a, rev_b)?;
            print!("{out}");
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Restore { path, rev, volume, meta, cache, versions_db } => {
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            let new_rev = trove::commands::history::restore(&fs, &mut versions, &path, rev)?;
            println!(
                "{} restored {path} to rev {rev} (recorded as new rev {new_rev})",
                "trove:".bold()
            );
            Ok(0)
        }
    }
}
