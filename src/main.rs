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
        /// Postgres URL for the version DB (Supabase). When set, every validated
        /// write is recorded (history + embedding metadata) best-effort — bytes
        /// to R2 (creds from the environment), metadata here. Omit to disable.
        #[arg(long)]
        versions_db: Option<String>,
        /// Local write-ahead log directory for pending version records.
        #[arg(long, default_value = "/tmp/trove-wal")]
        wal: PathBuf,
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
        Command::Mount { mountpoint, volume, meta, cache, types, versions_db, wal } => {
            let cache = cache.to_string_lossy();
            let fs = trove::jfs::Fs::init(&volume, &meta, &cache)?;
            let registry = match &types {
                Some(dir) => trove::types::Registry::load(dir)?,
                None => trove::types::Registry::empty(),
            };
            // Optional version recording: connect the DB + R2 and start the
            // background WAL drainer. Off when --versions-db is omitted.
            let recorder = match &versions_db {
                Some(url) => {
                    let vs = trove::version::VersionStore::connect(url)?;
                    let bs = trove::blobstore::BlobStore::from_env()?;
                    let rec = trove::recorder::Recorder::new(vs, bs, wal.clone())?;
                    rec.start_draining(std::time::Duration::from_secs(2));
                    Some(rec)
                }
                None => None,
            };
            println!(
                "{} mounting volume {volume:?} at {} ({}; versioning {})",
                "trove:".bold(),
                mountpoint.display(),
                if registry.is_empty() {
                    "no validation".to_string()
                } else {
                    format!("validating via {}", types.as_ref().unwrap().display())
                },
                if recorder.is_some() { "on" } else { "off" },
            );
            trove::mount::mount_blocking(fs, registry, recorder, &mountpoint)?;
            Ok(0)
        }
    }
}
