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
        Command::Mount { mountpoint, volume, meta, cache } => {
            let cache = cache.to_string_lossy();
            let fs = trove::jfs::Fs::init(&volume, &meta, &cache)?;
            println!("{} mounting volume {volume:?} at {}", "trove:".bold(), mountpoint.display());
            trove::mount::mount_blocking(fs, &mountpoint)?;
            Ok(0)
        }
    }
}
