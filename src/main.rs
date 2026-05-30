//! Thin CLI shell over the `trove` library. All logic lives in the lib so it
//! can be tested directly (see `tests/`).
//!
//! Connection settings resolve from explicit flags, environment variables, or
//! the per-folder vault selected from the current working directory.

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
    /// Initialise or attach a vault using the current folder name. Installs a
    /// per-vault boot agent so it re-mounts at every login (auto-mount), and
    /// starts that mount now in the background — you get your shell straight
    /// back. Pass `--no-autostart` to mount in the foreground instead (no system
    /// change). `attach` is an alias.
    #[cfg(feature = "mount")]
    #[command(alias = "attach")]
    Init {
        /// Disable on-commit embedding for this mount.
        #[arg(long)]
        no_embed: bool,
        /// Don't install the login boot agent — mount in the foreground (blocks
        /// the terminal), like a bare `trove mount`. Use when you'd rather not
        /// make a system change (installing a login agent).
        #[arg(long)]
        no_autostart: bool,
        /// Attach this volume under a named credential profile — a separate
        /// DB / R2 account. A new profile is prompted for and saved under
        /// `[profiles.<name>]`. Omit for the default (single-account) creds.
        #[arg(long)]
        profile: Option<String>,
    },

    /// Validate every typed markdown file in a store against its type schema.
    Check {
        /// Path to the store (defaults to the current directory).
        #[arg(default_value = ".")]
        store: PathBuf,
        /// Only print failures and the summary, not every passing file.
        #[arg(short, long)]
        quiet: bool,
    },

    /// Read the bundled documentation. With no arguments it lists every page;
    /// pass a page slug to print that page's markdown to stdout (`trove docs
    /// quickstart`); `--all` prints the whole manual concatenated (handy to
    /// pipe to an agent); `--serve` opens the browser UI instead of printing.
    /// No native deps, no Postgres, no OpenAI — the content lives under `docs/`
    /// and is baked into the binary at build time.
    Docs {
        /// Page slug to print (e.g. `quickstart`). Omit to list all pages.
        page: Option<String>,
        /// Print every page concatenated, in nav order.
        #[arg(long)]
        all: bool,
        /// Serve the docs as a browser UI on 127.0.0.1 instead of printing.
        #[arg(long)]
        serve: bool,
        /// Port to bind on 127.0.0.1 with `--serve`.
        #[arg(long, default_value_t = 38081)]
        port: u16,
    },

    /// Unmount a vault's live FUSE mount — runtime "down for now". The local
    /// config and boot agent are untouched, so it re-mounts at the next login.
    /// Resolves the vault by `--volume <name>` or the current folder.
    #[cfg(feature = "mount")]
    Unmount {
        /// Volume to unmount. Defaults to the vault of the current folder.
        #[arg(long)]
        volume: Option<String>,
    },

    /// Detach a vault from this machine: unmount, remove its boot agent, and
    /// delete the local config. The backend (schema + bucket) is **left intact**
    /// — other machines are unaffected and `trove init` re-attaches it here
    /// later. (Destroying a vault is a separate, manual step.)
    #[cfg(feature = "mount")]
    Detach {
        /// Volume to detach. Defaults to the vault of the current folder.
        #[arg(long)]
        volume: Option<String>,
    },

    /// List every vault configured on this machine with its mount + boot-agent
    /// status — the fleet view.
    #[cfg(feature = "mount")]
    Ls,

    /// Mount a Trove filesystem (foreground). Give a <mountpoint>, or resolve a
    /// vault entirely from its saved config — by `--volume <name>` or the
    /// current folder. The `--volume` form (no cwd, no ambient env) is what each
    /// boot agent runs.
    #[cfg(feature = "mount")]
    Mount {
        /// Where to mount (an existing empty directory). Omit to resolve the
        /// mountpoint from saved config via `--volume <name>` or the cwd's vault.
        mountpoint: Option<PathBuf>,
        /// Mount onto a non-empty directory. By default `trove mount` refuses
        /// — FUSE overlays the mountpoint, so existing files become invisible
        /// while mounted (recoverable on unmount, but alarming). Pass this when
        /// you've thought about it; use `trove import` to bring existing files
        /// into a vault instead.
        #[arg(long)]
        allow_non_empty: bool,
        /// Storage volume name (must already be formatted). Falls back to config.
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
    /// reachable + pgvector + schema + storage), and a validation sweep over
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
    /// storage volume's view of the bucket. A quick "is this growing the way I
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
    let url = trove::config::resolve(
        flag,
        "TROVE_VERSIONS_DB",
        cfg.versions_db.clone(),
        "versions DB URL",
    )?;
    trove::version::VersionStore::connect(&url, cfg.schema_name().as_deref())
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
    // Point JuiceFS at the volume's schema so its jfs_* tables land there, not
    // in `public`, matching where the version tables live.
    let meta = match cfg.schema_name() {
        Some(schema) => trove::config::juicefs_meta_url(&meta, &schema),
        None => meta,
    };
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

/// Resolve the target volume name for a lifecycle command: the explicit
/// `--volume` flag, else the vault of the current folder (from config). The
/// boot agent always passes `--volume` (it has no cwd); a person in a vault
/// folder can omit it.
#[cfg(feature = "mount")]
fn volume_name(flag: Option<String>, cfg: &trove::config::Config) -> Result<String> {
    flag.filter(|s| !s.is_empty())
        .or_else(|| cfg.volume.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no volume — pass `--volume <name>`, or run inside a vault folder. \
                 `trove ls` lists what's configured."
            )
        })
}

/// Returns the number of failed files (0 = clean).
fn run() -> Result<usize> {
    let cli = Cli::parse();
    #[cfg(feature = "mount")]
    let cfg = trove::config::Config::load();
    match cli.command {
        #[cfg(feature = "mount")]
        Command::Init {
            no_embed,
            no_autostart,
            profile,
        } => {
            let init = trove::commands::init::run(trove::commands::init::InitOptions {
                no_embed,
                profile,
            })?;
            if no_autostart {
                // Foreground mount: no system change, blocks the terminal (the
                // classic `trove mount` shape). The vault is *not* set to come
                // back after logout/reboot — that's what the boot agent is for.
                let fs = trove::jfs::Fs::init(
                    &init.volume,
                    &trove::config::juicefs_meta_url(&init.meta, &init.schema),
                    &init.cache,
                )?;
                // The type registry travels *in* the vault (`<vault>/.types/`),
                // versioned like any file. Read it through the volume (libjfs),
                // not local disk — so attaching to an existing vault picks up its
                // schemas (the local folder is empty until the mount surfaces it).
                let registry = trove::types::Registry::load_from_fs(&fs)?;
                let versions = Some(trove::version::VersionStore::connect(
                    &init.versions_db,
                    Some(&init.schema),
                )?);
                let embed_tx = if !no_embed {
                    match openai_key() {
                        Ok(key) => Some(trove::embed::spawn_embedder(
                            &init.versions_db,
                            key,
                            Some(&init.schema),
                        )?),
                        Err(_) => {
                            eprintln!(
                                "{} OPENAI_API_KEY not set — embedding disabled for this mount",
                                "warning:".yellow().bold()
                            );
                            None
                        }
                    }
                } else {
                    None
                };
                println!(
                    "{} mounting `{}` at {} (foreground — Ctrl-C to unmount)",
                    "trove init:".bold(),
                    init.volume,
                    init.mountpoint.display()
                );
                trove::mount::mount_blocking(fs, registry, versions, embed_tx, &init.mountpoint)?;
            } else {
                // Install + start the per-vault boot agent. The mount runs in the
                // background and re-mounts at every login; you get your shell
                // back immediately. The set of agents IS this machine's vault
                // membership — removed only by `trove detach`.
                let mountpoint = init.mountpoint.to_string_lossy().into_owned();
                trove::platform::install_agent(&init.volume, &mountpoint)?;
                println!(
                    "{} `{}` will auto-mount at login and is mounting now at {} ({}).",
                    "trove init:".bold(),
                    init.volume,
                    mountpoint,
                    trove::platform::agent_status(&init.volume).label()
                );
                println!("  logs: {}", trove::platform::log_hint(&init.volume));
                println!(
                    "  {} `trove ls` · `trove unmount --volume {}` (down for now) · `trove detach --volume {}` (remove here)",
                    "·".dimmed(),
                    init.volume,
                    init.volume
                );
            }
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Unmount { volume } => {
            let name = volume_name(volume, &cfg)?;
            trove::commands::lifecycle::unmount(&name)?;
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Detach { volume } => {
            let name = volume_name(volume, &cfg)?;
            trove::commands::lifecycle::detach(&name)?;
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Ls => trove::commands::lifecycle::ls(),

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

        Command::Docs {
            page,
            all,
            serve,
            port,
        } => {
            if serve {
                trove::commands::docs::serve(port)?;
            } else {
                use std::io::Write;
                let out = if all {
                    trove::commands::docs::all_markdown()?
                } else if let Some(slug) = page {
                    trove::commands::docs::page_markdown(&slug)?
                } else {
                    trove::commands::docs::index_text()?
                };
                // Tolerate a closed pipe (`trove docs --all | head`) instead of
                // panicking on the broken-pipe write.
                let _ = std::io::stdout().write_all(out.as_bytes());
            }
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Mount {
            mountpoint,
            allow_non_empty,
            volume,
            meta,
            cache,
            types,
            versions_db,
            no_embed,
        } => {
            // No explicit <mountpoint>: resolve the whole vault (mountpoint,
            // schema, cache, creds) from its saved config — by `--volume <name>`
            // or, inside a vault folder, the current directory's vault. This is
            // the form each boot agent runs (it passes `--volume`; it has no cwd).
            let mountpoint = match mountpoint {
                Some(mp) => mp,
                None => {
                    let name = volume_name(volume.clone(), &cfg)?;
                    trove::commands::lifecycle::mount_volume(&name, no_embed)?;
                    return Ok(0);
                }
            };
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
                .filter(|e| e.file_name().to_string_lossy().chars().next() != Some('.'))
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
            // Prefer the vault's own `.types/` (schemas travel as vault content,
            // read through the volume); an explicit `--types <dir>` still
            // overrides for local/dev use.
            let registry = match &types {
                Some(dir) => trove::types::Registry::load(dir)?,
                None => trove::types::Registry::load_from_fs(&fs)?,
            };
            // Versioning is optional: resolve the URL without erroring (flag >
            // env > config); None anywhere = versioning off.
            let versions_url = versions_db
                .or_else(|| {
                    std::env::var("TROVE_VERSIONS_DB")
                        .ok()
                        .filter(|s| !s.is_empty())
                })
                .or_else(|| cfg.versions_db.clone());
            let schema = cfg.schema_name();
            let versions = match &versions_url {
                Some(url) => Some(trove::version::VersionStore::connect(
                    url,
                    schema.as_deref(),
                )?),
                None => None,
            };
            // On-commit embedding is ON by default whenever versioning is on.
            // Resolve the API key up front so a missing key fails the mount
            // immediately, not lazily on the first write. `--no-embed` opts out
            // for offline runs.
            let embed_tx = match (&versions_url, no_embed) {
                (Some(url), false) => {
                    let key = openai_key().map_err(|e| {
                        anyhow::anyhow!(
                        "{e}. Set OPENAI_API_KEY, or pass --no-embed to mount without embedding."
                    )
                    })?;
                    Some(trove::embed::spawn_embedder(url, key, schema.as_deref())?)
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
                    match &types {
                        Some(dir) => format!("validating via {}", dir.display()),
                        None => "validating via vault .types/".to_string(),
                    }
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
        Command::Import {
            path,
            types,
            volume,
            meta,
            cache,
            versions_db,
            no_embed,
            yes,
            force,
        } => {
            use trove::commands::import::ImportOptions;
            trove::commands::import::run(
                ImportOptions {
                    path,
                    types,
                    yes,
                    force,
                },
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
        Command::Embed {
            volume,
            meta,
            cache,
            versions_db,
            watch,
            remodel,
        } => {
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
                println!(
                    "{} re-embedded {n} blob(s) for model upgrade",
                    "trove:".bold()
                );
                return Ok(0);
            }
            match watch {
                Some(secs) => {
                    println!("{} embedding (watch, every {secs}s)…", "trove:".bold());
                    trove::embed::run_watch(
                        &fs,
                        &mut versions,
                        &api_key,
                        std::time::Duration::from_secs(secs),
                    )?;
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
        Command::Search {
            query,
            versions_db,
            top_k,
        } => {
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
        Command::Server {
            port,
            volume,
            meta,
            cache,
            versions_db,
        } => {
            let api_key = openai_key()?;
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            trove::commands::server::run(&fs, &mut versions, &api_key, port)?;
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Doctor {
            versions_db,
            volume,
            meta,
            cache,
            store,
        } => {
            let checks =
                trove::commands::doctor::run(&cfg, versions_db, volume, meta, cache, store);
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
                if rows.is_empty() {
                    continue;
                }
                if !printed_sections.is_empty() {
                    println!();
                }
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
        Command::Usage {
            volume,
            meta,
            cache,
            versions_db,
        } => {
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
            println!(
                "{} {} ({} revision(s))",
                "trove:".bold(),
                path,
                entries.len()
            );
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
        Command::Cat {
            path,
            rev,
            volume,
            meta,
            cache,
            versions_db,
        } => {
            use std::io::Write;
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            let bytes = trove::commands::history::cat(&fs, &mut versions, &path, rev)?;
            std::io::stdout().write_all(&bytes)?;
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Diff {
            path,
            rev_a,
            rev_b,
            volume,
            meta,
            cache,
            versions_db,
        } => {
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            let out = trove::commands::history::diff(&fs, &mut versions, &path, rev_a, rev_b)?;
            print!("{out}");
            Ok(0)
        }

        #[cfg(feature = "mount")]
        Command::Backup {
            dest,
            layout,
            dry_run,
            volume,
            meta,
            cache,
            versions_db,
        } => {
            use trove::commands::backup::{self, BackupOptions, Layout};
            let layout = Layout::parse(&layout)?;
            // Flag > config — backup_dir is purely optional, so failing-fast
            // here is cleaner than the `resolve` helper (no env var pairing).
            let dest = dest
                .or_else(|| cfg.backup_dir.as_deref().map(PathBuf::from))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no backup destination — pass --dest <dir>, or set `backup_dir` in config"
                    )
                })?;
            let fs = init_fs(volume, meta, cache, &cfg)?;
            let mut versions = connect_versions(versions_db, &cfg)?;
            let report = backup::run(
                &fs,
                &mut versions,
                &BackupOptions {
                    dest: dest.clone(),
                    layout,
                    since: None,
                    dry_run,
                },
            )?;
            let prefix = if dry_run {
                "trove backup (dry-run):"
            } else {
                "trove backup:"
            };
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
        Command::Restore {
            path,
            rev,
            volume,
            meta,
            cache,
            versions_db,
        } => {
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
