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

    /// Write ~/.config/trove/config.toml interactively, then provision the
    /// backend: apply the embedded SQL migration to the version DB and format
    /// the JuiceFS volume. Refuses to clobber existing non-empty Trove tables
    /// or to re-format a volume against a different bucket — use the safety
    /// flags to override. Secrets stay in the environment.
    Install {
        /// Accept existing Trove tables / a formatted volume; skip the create
        /// steps. Use when re-running install against a backend you intend to keep.
        #[arg(long)]
        reuse: bool,
        /// DROP existing Trove tables and reformat the JuiceFS volume.
        /// DESTRUCTIVE — every destructive step still prompts for an explicit
        /// `destroy` confirmation. Re-formatting against a new bucket orphans
        /// the chunks under the old one; this flag is the only way through.
        #[arg(long)]
        reinstall: bool,
    },

    /// Serve the bundled documentation on localhost. No native deps, no
    /// Postgres, no OpenAI — `trove check`-only installs get the docs too.
    /// The content lives under `docs/` in the source tree and is baked into
    /// the binary at build time.
    Docs {
        /// Port to bind on 127.0.0.1.
        #[arg(long, default_value_t = 38081)]
        port: u16,
    },

    /// Mount a JuiceFS-backed Trove filesystem at <mountpoint> (foreground).
    #[cfg(feature = "mount")]
    Mount {
        /// Where to mount (an existing empty directory).
        mountpoint: PathBuf,
        /// Mount onto a non-empty directory. By default `trove mount` refuses
        /// — FUSE overlays the mountpoint, so existing files become invisible
        /// while mounted (recoverable on unmount, but alarming). Pass this when
        /// you've thought about it; use `trove import` to bring existing files
        /// into a vault instead.
        #[arg(long)]
        allow_non_empty: bool,
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
        /// Disable on-commit embedding. By default, when versions_db is
        /// resolvable, every committed file is embedded into `blob_chunks` for
        /// semantic search; pass `--no-embed` to skip the OpenAI call (useful
        /// for offline runs or when search isn't needed).
        #[arg(long)]
        no_embed: bool,
    },

    /// Take over an existing directory: move its contents to a backup,
    /// mount trove at the original path, and stream the files back through
    /// the validation gate so they get versioned and embedded. Foreground —
    /// the mount stays running after import (this directory IS your vault now).
    #[cfg(feature = "mount")]
    Import {
        /// The directory to take over. Becomes the trove mountpoint.
        path: PathBuf,
        /// Schemas to validate against during import. Defaults to <path>/.types/.
        #[arg(long)]
        types: Option<PathBuf>,
        #[arg(long)]
        volume: Option<String>,
        #[arg(long)]
        meta: Option<String>,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long)]
        versions_db: Option<String>,
        #[arg(long)]
        no_embed: bool,
        /// Skip the typed-confirmation step. Use in scripts. The same safety
        /// thresholds (path checks, size limits) still apply.
        #[arg(long)]
        yes: bool,
        /// Skip the file-count / total-size safety thresholds. Required to
        /// import a directory with > 10k files or > 1 GB.
        #[arg(long)]
        force: bool,
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
        /// Re-embed any blob whose current chunks use a different embedding_model
        /// than the one this binary is built against. Use after bumping the MODEL
        /// constant; idempotent once caught up.
        #[arg(long)]
        remodel: bool,
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

    /// Full health check: configuration + provenance, secrets, backend (DB
    /// reachable + pgvector + schema + JuiceFS), and a validation sweep over
    /// the configured store. Exits non-zero if anything's missing or invalid.
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
        /// Override the configured store path (defaults to config / TROVE_STORE).
        #[arg(long)]
        store: Option<PathBuf>,
    },

    /// Show how much space Trove is using — DB (versions + embeddings) and the
    /// JuiceFS volume's view of the bucket. A quick "is this growing the way I
    /// expect?" check.
    #[cfg(feature = "mount")]
    Usage {
        #[arg(long)]
        volume: Option<String>,
        #[arg(long)]
        meta: Option<String>,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long)]
        versions_db: Option<String>,
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

    /// Write a local mirror of every committed file. Walks the version chain so
    /// history is preserved. Incremental by default (re-running skips revs whose
    /// bytes already exist at the destination with a matching hash).
    #[cfg(feature = "mount")]
    Backup {
        /// Destination directory. Falls back to config `backup_dir`.
        #[arg(long)]
        dest: Option<PathBuf>,
        /// `by-path` (default): live tree at <dest>/<path> + history under
        /// <dest>/.versions/. `by-rev`: one full tree per rev under <dest>/rev-N/.
        #[arg(long, default_value = "by-path")]
        layout: String,
        /// Walk + count, but don't write any files.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        volume: Option<String>,
        #[arg(long)]
        meta: Option<String>,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long)]
        versions_db: Option<String>,
    },

    /// Upgrade trove in place by re-running the canonical install script
    /// (`install.sh` from the repo). Resolves the latest release tag from
    /// GitHub, short-circuits if you're already on it, otherwise shells out
    /// to `curl … | sh` so the script's sha256 verification + symlink dance
    /// stay the single source of truth. Pass `--version vX.Y.Z` to pin a
    /// specific release.
    SelfUpdate {
        /// Pin a specific release tag (e.g. `v0.1.2`). Defaults to latest.
        #[arg(long)]
        version: Option<String>,
        /// Skip the "already on latest" short-circuit and re-install anyway.
        #[arg(long)]
        force: bool,
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
            // Schema count printed first so a `0 schemas` result is unmissable —
            // an empty registry validates nothing, which is rarely what you want.
            println!(
                "\n{} {} schema(s) · {} checked · {} valid · {} untyped · {} {}",
                "trove:".bold(),
                s.schemas_present,
                s.checked,
                s.valid.to_string().green(),
                s.untyped,
                s.failed.to_string().red(),
                if s.failed == 1 { "failure" } else { "failures" }
            );
            Ok(s.failed)
        }

        Command::Docs { port } => {
            trove::commands::docs::run(port)?;
            Ok(0)
        }

        Command::Install { reuse, reinstall } => {
            trove::commands::install::run(trove::commands::install::InstallFlags { reuse, reinstall })?;
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Mount { mountpoint, allow_non_empty, volume, meta, cache, types, versions_db, no_embed } => {
            // FUSE overlays the mountpoint while mounted — any existing files
            // become invisible (recoverable on unmount, but alarming). Refuse
            // non-empty mountpoints by default; the `--allow-non-empty` escape
            // hatch is for users who've thought about it. Hidden files (.DS_Store,
            // .Spotlight-V100, .directory) are ignored — every filesystem drops
            // those and they're not what "non-empty" is trying to protect.
            use anyhow::Context;
            let visible = std::fs::read_dir(&mountpoint)
                .with_context(|| format!("opening mountpoint {}", mountpoint.display()))?
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .chars()
                        .next()
                        != Some('.')
                })
                .count();
            if visible > 0 && !allow_non_empty {
                anyhow::bail!(
                    "mountpoint {} is not empty — FUSE will hide its contents while mounted.\n\
                     To import these files into a trove vault, run:\n  \
                     trove import {}\n\
                     To mount anyway (advanced; existing files become temporarily invisible), pass --allow-non-empty.",
                    mountpoint.display(),
                    mountpoint.display()
                );
            }
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
            // On-commit embedding is ON by default whenever versioning is on.
            // Resolve the API key up front so a missing key fails the mount
            // immediately, not lazily on the first write. `--no-embed` opts out
            // for offline runs.
            let embed_tx = match (&versions_url, no_embed) {
                (Some(url), false) => {
                    let key = openai_key().map_err(|e| anyhow::anyhow!(
                        "{e}. Set OPENAI_API_KEY, or pass --no-embed to mount without embedding."
                    ))?;
                    Some(trove::embed::spawn_embedder(url, key)?)
                }
                _ => None,
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
                if embed_tx.is_some() {
                    "on"
                } else if versions.is_none() {
                    "off (needs versioning)"
                } else {
                    "off (--no-embed)"
                },
            );
            trove::mount::mount_blocking(fs, registry, versions, embed_tx, &mountpoint)?;
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Import { path, types, volume, meta, cache, versions_db, no_embed, yes, force } => {
            use trove::commands::import::ImportOptions;
            trove::commands::import::run(
                ImportOptions { path, types, yes, force },
                &cfg,
                volume,
                meta,
                cache,
                versions_db,
                no_embed,
            )?;
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Embed { volume, meta, cache, versions_db, watch, remodel } => {
            if remodel && watch.is_some() {
                return Err(anyhow::anyhow!(
                    "--remodel and --watch are mutually exclusive — remodel is a one-shot migration"
                ));
            }
            let api_key = openai_key()?;
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            if remodel {
                let n = trove::embed::run_remodel(&fs, &mut versions, &api_key)?;
                println!("{} re-embedded {n} blob(s) for model upgrade", "trove:".bold());
                return Ok(0);
            }
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
        Command::Doctor { versions_db, volume, meta, cache, store } => {
            let checks = trove::commands::doctor::run(&cfg, versions_db, volume, meta, cache, store);
            let failed = checks.iter().filter(|c| !c.ok).count();
            println!("{}", "trove doctor".bold());

            // Group rows by section, in the canonical order, blank line between.
            use trove::commands::doctor::SECTION_ORDER;
            let mut printed_sections: Vec<&str> = Vec::new();
            let mut sections_in_order: Vec<&str> = SECTION_ORDER.to_vec();
            // Append any extra sections we don't know about (defensive).
            for c in &checks {
                if !sections_in_order.contains(&c.section) {
                    sections_in_order.push(c.section);
                }
            }
            for section in sections_in_order {
                let rows: Vec<_> = checks.iter().filter(|c| c.section == section).collect();
                if rows.is_empty() { continue; }
                if !printed_sections.is_empty() { println!(); }
                println!("  {}", section.bold());
                for c in rows {
                    let mark = if c.ok { "✓".green() } else { "✗".red() };
                    println!("    {mark} {:<18} {}", c.name, c.detail.dimmed());
                }
                printed_sections.push(section);
            }

            if failed == 0 {
                println!("\n{} all checks passed", "✓".green().bold());
            } else {
                println!("\n{} {failed} check(s) failed", "✗".red().bold());
            }
            Ok(failed)
        }

        #[cfg(feature = "mount")]
        Command::Usage { volume, meta, cache, versions_db } => {
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            let report = trove::commands::usage::run(&fs, &mut versions)?;
            trove::commands::usage::print(&report);
            Ok(0)
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
        Command::Backup { dest, layout, dry_run, volume, meta, cache, versions_db } => {
            use trove::commands::backup::{self, BackupOptions, Layout};
            let layout = Layout::parse(&layout)?;
            // Flag > config — backup_dir is purely optional, so failing-fast
            // here is cleaner than the `resolve` helper (no env var pairing).
            let dest = dest
                .or_else(|| cfg.backup_dir.as_deref().map(PathBuf::from))
                .ok_or_else(|| anyhow::anyhow!(
                    "no backup destination — pass --dest <dir>, or set `backup_dir` in ~/.config/trove/config.toml (run `trove install`)"
                ))?;
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            let report = backup::run(
                &fs,
                &mut versions,
                &BackupOptions { dest: dest.clone(), layout, since: None, dry_run },
            )?;
            let prefix = if dry_run { "trove backup (dry-run):" } else { "trove backup:" };
            println!(
                "{} {} path(s) walked \u{2192} {}; {} rev(s) written ({}), {} unchanged",
                prefix.bold(),
                report.paths,
                dest.display(),
                report.revisions_written,
                trove::commands::usage::human_bytes(report.bytes_written),
                report.skipped_unchanged,
            );
            Ok(0)
        }

        Command::SelfUpdate { version, force } => {
            trove::commands::self_update::run(version.as_deref(), force)?;
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
