//! Thin CLI shell over the `trove` library. All logic lives in the lib so it
//! can be tested directly (see `tests/`).

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

    /// Mount a JuiceFS-backed Trove filesystem at <mountpoint> (foreground).
    #[cfg(feature = "mount")]
    Mount {
        /// Where to mount (an existing empty directory).
        mountpoint: PathBuf,
        /// JuiceFS volume name (must already be formatted).
        #[arg(long)]
        volume: String,
        /// Metadata engine URL, e.g. postgres://… or sqlite3://…
        #[arg(long)]
        meta: String,
        /// Local block-cache directory.
        #[arg(long, default_value = "/tmp/trove-cache")]
        cache: PathBuf,
        /// Directory containing a `.types/` schema registry. When set, writes
        /// are validated against it (the "filesystem that talks back"); when
        /// omitted the mount is a plain pass-through.
        #[arg(long)]
        types: Option<PathBuf>,
        /// Postgres URL for the version chain (the SAME Supabase Postgres as
        /// `--meta`). When set, every validated write is versioned best-effort:
        /// a COW clone into the archive + a chain row. Omit to disable versioning.
        #[arg(long)]
        versions_db: Option<String>,
        /// Embed each committed file as it's written (self-triggering, no cron):
        /// a background thread embeds straight from the write buffer. Requires
        /// --versions-db and OPENAI_API_KEY. Run this on the box that holds the key.
        #[arg(long)]
        embed: bool,
    },

    /// Embed un-embedded version blobs into `blob_chunks` for search. Reads
    /// content via libjfs, chunks by header, calls OpenAI. Needs OPENAI_API_KEY.
    #[cfg(feature = "mount")]
    Embed {
        /// JuiceFS volume name (must already be formatted).
        #[arg(long)]
        volume: String,
        /// Metadata engine URL (same Postgres as --versions-db in production).
        #[arg(long)]
        meta: String,
        /// Local block-cache directory.
        #[arg(long, default_value = "/tmp/trove-cache")]
        cache: PathBuf,
        /// Postgres URL for the version chain + embeddings.
        #[arg(long)]
        versions_db: String,
        /// Run forever, sweeping every N seconds, instead of a single pass.
        #[arg(long)]
        watch: Option<u64>,
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

/// Returns the number of failed files (0 = clean).
fn run() -> Result<usize> {
    let cli = Cli::parse();
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

        #[cfg(feature = "mount")]
        Command::Mount { mountpoint, volume, meta, cache, types, versions_db, embed } => {
            let cache = cache.to_string_lossy();
            let fs = trove::jfs::Fs::init(&volume, &meta, &cache)?;
            let registry = match &types {
                Some(dir) => trove::types::Registry::load(dir)?,
                None => trove::types::Registry::empty(),
            };
            // Optional version capture: connect the chain DB. Off when omitted.
            let versions = match &versions_db {
                Some(url) => Some(trove::version::VersionStore::connect(url)?),
                None => None,
            };
            // Optional self-triggering embedding: spawn the background embed
            // thread; commit() pushes to it. Needs versioning + the OpenAI key.
            let embed_tx = if embed {
                let url = versions_db
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("--embed requires --versions-db"))?;
                let key = std::env::var("OPENAI_API_KEY")
                    .map_err(|_| anyhow::anyhow!("--embed requires OPENAI_API_KEY"))?;
                Some(trove::embed::spawn_embedder(url, key)?)
            } else {
                None
            };
            println!(
                "{} mounting volume {volume:?} at {} ({}; versioning {}; embed {})",
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
            let api_key = std::env::var("OPENAI_API_KEY")
                .map_err(|_| anyhow::anyhow!("OPENAI_API_KEY not set"))?;
            let fs = trove::jfs::Fs::init(&volume, &meta, &cache.to_string_lossy())?;
            let mut versions = trove::version::VersionStore::connect(&versions_db)?;
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
    }
}
