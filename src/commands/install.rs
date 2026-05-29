//! `trove install` — interactive setup that writes the config file AND
//! provisions both halves of the substrate (Postgres schema for the version
//! chain + embeddings, and the JuiceFS volume on the configured bucket).
//!
//! The decision-making half of this module is a **pure state machine**
//! ([`plan`]): given what's already in the DB and what flags the user passed,
//! it returns a [`Plan`] of what to do. The IO half ([`run`]) does the prompts,
//! talks to Postgres, and shells out to `juicefs format`. Splitting it this way
//! keeps the decision table fully covered by unit tests with no DB needed.
//!
//! Safety baked in:
//! - **Refuse to clobber a non-empty Trove DB** without `--reinstall` (and the
//!   `--reinstall` path still demands an explicit `destroy` confirmation).
//! - **Refuse to re-format a JuiceFS volume against a different bucket** — that
//!   would orphan the chunks under the recorded bucket. Same confirmation gate
//!   applies for `--reinstall`.
//!
//! The migration SQL is `include_str!`'d so the binary is self-contained; no
//! runtime file lookup, no broken installs from a missing `supabase/` tree.

use anyhow::{anyhow, bail, Context, Result};
use postgres::{Client, NoTls};
use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};

use crate::config::Config;

/// Bundled migration. Single-file by convention (the schema lint enforces this).
/// We `include_str!` so the binary carries its own schema — no runtime SQL file
/// lookup, no "where did supabase/ go?" surprises.
const MIGRATION_SQL: &str =
    include_str!("../../supabase/migrations/20260527172259_init_version_chain_and_embeddings.sql");

/// Flags that change the safety posture. The defaults refuse to touch anything
/// that already has content; `--reuse` accepts existing state; `--reinstall`
/// destroys it (after explicit confirmation).
#[derive(Debug, Default, Clone, Copy)]
pub struct InstallFlags {
    /// Accept existing Trove tables / volume; skip create steps.
    pub reuse: bool,
    /// DROP existing Trove tables + reformat the volume. DESTRUCTIVE; gated by
    /// a literal-`destroy` confirmation prompt at runtime.
    pub reinstall: bool,
}

/// Snapshot of what's already in the DB. Filled by the IO layer and fed to
/// [`plan`] so the decision logic stays pure.
#[derive(Debug, Clone)]
pub struct DbState {
    /// Which of `blobs`/`file_versions`/`blob_chunks` are present.
    pub tables_present: HashSet<String>,
    /// Of those that ARE present, which have at least one row.
    pub tables_with_rows: HashSet<String>,
    /// Whether any `jfs_*` table exists at all.
    pub jfs_present: bool,
    /// JuiceFS's recorded bucket for this metadata DB (the `Bucket` field of
    /// the `format` row in `jfs_setting`). `None` if not parsable / not set.
    pub recorded_bucket: Option<String>,
}

/// What [`run`] should do for the migration step.
#[derive(Debug, PartialEq, Eq)]
pub enum MigrationAction {
    /// No Trove tables exist — apply the migration cleanly.
    RunMigration,
    /// Tables exist but every one is empty — leave alone (re-running would
    /// hit `CREATE TABLE` errors).
    SkipExisting { reason: &'static str },
    /// Tables exist and at least one carries rows; `--reinstall` was given —
    /// confirm, drop, then run migration.
    DropAndRecreate { populated_table: String, row_count: i64 },
    /// Tables exist and at least one carries rows; `--reuse` was given —
    /// leave alone.
    ReuseExisting { populated_table: String, row_count: i64 },
    /// Tables exist and at least one carries rows; neither flag given — abort
    /// with a clear message.
    RefuseNonEmpty { populated_table: String, row_count: i64 },
}

/// What [`run`] should do for the JuiceFS format step.
#[derive(Debug, PartialEq, Eq)]
pub enum FormatAction {
    /// No `jfs_*` tables — format the volume.
    Format,
    /// `jfs_*` tables present, recorded bucket matches the requested one — skip.
    SkipSameBucket { bucket: String },
    /// `jfs_*` tables present, recorded bucket differs and `--reinstall` was
    /// given — confirm, drop jfs_* tables, then format.
    DropAndReformat { recorded: String, requested: String },
    /// `jfs_*` tables present and `--reuse` was given (with no bucket data we
    /// can verify, or bucket matches) — leave alone.
    ReuseExisting,
    /// `jfs_*` tables present, bucket differs, no `--reinstall` — abort:
    /// re-formatting would orphan the chunks under the recorded bucket.
    RefuseBucketMismatch { recorded: String, requested: String },
}

/// Combined plan for one install run.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub migration: MigrationAction,
    pub format: FormatAction,
}

const SCHEMA_TABLES: [&str; 3] = ["blobs", "file_versions", "blob_chunks"];

/// **Pure decision function.** Maps `(db_state, requested_bucket, flags)` to a
/// concrete plan. Unit-tested in isolation — no DB, no IO.
///
/// State table (DB side):
///
/// | tables present | any with rows | flags          | action                         |
/// |----------------|---------------|----------------|--------------------------------|
/// | none           | —             | any            | RunMigration                   |
/// | all, empty     | —             | any            | SkipExisting (empty)           |
/// | all, populated | yes           | (none)         | RefuseNonEmpty                 |
/// | all, populated | yes           | --reuse        | ReuseExisting                  |
/// | all, populated | yes           | --reinstall    | DropAndRecreate (gated by y/N) |
///
/// State table (JuiceFS side):
///
/// | jfs_* present | recorded bucket   | flags        | action                |
/// |---------------|-------------------|--------------|-----------------------|
/// | no            | —                 | any          | Format                |
/// | yes           | == requested      | any          | SkipSameBucket        |
/// | yes           | != requested      | (none)       | RefuseBucketMismatch  |
/// | yes           | != requested      | --reuse      | ReuseExisting         |
/// | yes           | != requested      | --reinstall  | DropAndReformat       |
/// | yes           | unknown           | (none)       | ReuseExisting (safe)  |
/// | yes           | unknown           | --reinstall  | DropAndReformat       |
pub fn plan(db: &DbState, requested_bucket: &str, flags: InstallFlags) -> Plan {
    Plan {
        migration: plan_migration(db, flags),
        format: plan_format(db, requested_bucket, flags),
    }
}

fn plan_migration(db: &DbState, flags: InstallFlags) -> MigrationAction {
    let present: Vec<&str> = SCHEMA_TABLES
        .iter()
        .filter(|t| db.tables_present.contains(**t))
        .copied()
        .collect();
    if present.is_empty() {
        return MigrationAction::RunMigration;
    }
    // At least one table present. Find any populated table to report.
    let populated = SCHEMA_TABLES
        .iter()
        .find(|t| db.tables_with_rows.contains(**t))
        .copied();
    match populated {
        None => MigrationAction::SkipExisting {
            reason: "schema already present (all empty)",
        },
        Some(table) => {
            // We don't carry the row count through the state; the IO layer
            // sets it. For pure-logic tests we treat any membership in
            // tables_with_rows as ">= 1 row".
            let row_count = 1;
            if flags.reinstall {
                MigrationAction::DropAndRecreate {
                    populated_table: table.to_string(),
                    row_count,
                }
            } else if flags.reuse {
                MigrationAction::ReuseExisting {
                    populated_table: table.to_string(),
                    row_count,
                }
            } else {
                MigrationAction::RefuseNonEmpty {
                    populated_table: table.to_string(),
                    row_count,
                }
            }
        }
    }
}

fn plan_format(db: &DbState, requested_bucket: &str, flags: InstallFlags) -> FormatAction {
    if !db.jfs_present {
        return FormatAction::Format;
    }
    match &db.recorded_bucket {
        Some(rec) if rec == requested_bucket => FormatAction::SkipSameBucket {
            bucket: rec.clone(),
        },
        Some(rec) => {
            if flags.reinstall {
                FormatAction::DropAndReformat {
                    recorded: rec.clone(),
                    requested: requested_bucket.to_string(),
                }
            } else if flags.reuse {
                FormatAction::ReuseExisting
            } else {
                FormatAction::RefuseBucketMismatch {
                    recorded: rec.clone(),
                    requested: requested_bucket.to_string(),
                }
            }
        }
        None => {
            // We can see jfs_* tables but couldn't parse the recorded bucket.
            // Be conservative: treat unknown as "do not touch" by default.
            // `--reinstall` still lets the user blow it away if they're sure.
            if flags.reinstall {
                FormatAction::DropAndReformat {
                    recorded: "<unknown>".to_string(),
                    requested: requested_bucket.to_string(),
                }
            } else {
                FormatAction::ReuseExisting
            }
        }
    }
}

// -- IO half --------------------------------------------------------------

/// Public entry point used by `main.rs`. Branches on whether stdin is a TTY:
///
/// - **Terminal** → [`run_interactive`]: a guided setup that explains what's
///   needed, prompts for the config, and reads any missing secrets without
///   echoing them.
/// - **No TTY** (an agent or script is driving us) → [`run_noninteractive`]:
///   read every setting from the environment and either provision straight
///   through, or — if something's missing — print a precise "set these
///   variables" guide instead of blocking forever on a dead stdin.
pub fn run(flags: InstallFlags) -> Result<()> {
    if io::stdin().is_terminal() {
        run_interactive(flags)
    } else {
        run_noninteractive(flags)
    }
}

/// An environment variable, treating empty-string as unset.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// True when both R2 credentials are present in the environment.
fn r2_keys_present() -> bool {
    env_nonempty("R2_ACCESS_KEY_ID").is_some() && env_nonempty("R2_SECRET_ACCESS_KEY").is_some()
}

/// Resolve the full config from the environment, layered over any existing
/// config file. Mirrors the precedence the rest of the CLI uses (env > config),
/// with the same defaults the interactive prompts offer (`volume = trove`,
/// `meta = versions_db`, `cache = /tmp/trove-cache`). `DATABASE_URL` is accepted
/// as an alias for `TROVE_VERSIONS_DB` because it's the near-universal name and
/// the value is persisted to config here anyway.
fn resolve_env_config(cur: &Config) -> Config {
    let versions_db = env_nonempty("TROVE_VERSIONS_DB")
        .or_else(|| env_nonempty("DATABASE_URL"))
        .or_else(|| cur.versions_db.clone());
    let meta = env_nonempty("TROVE_META")
        .or_else(|| cur.meta.clone())
        .or_else(|| versions_db.clone());
    Config {
        versions_db,
        volume: env_nonempty("TROVE_VOLUME")
            .or_else(|| cur.volume.clone())
            .or_else(|| Some("trove".to_string())),
        meta,
        cache: env_nonempty("TROVE_CACHE")
            .or_else(|| cur.cache.clone())
            .or_else(|| Some("/tmp/trove-cache".to_string())),
        r2_bucket: env_nonempty("TROVE_R2_BUCKET").or_else(|| cur.r2_bucket.clone()),
        store: env_nonempty("TROVE_STORE").or_else(|| cur.store.clone()),
        backup_dir: env_nonempty("TROVE_BACKUP_DIR").or_else(|| cur.backup_dir.clone()),
    }
}

/// No-TTY path: read everything from the environment. If the required pieces
/// are all present, provision with zero prompts; otherwise print the setup
/// guide and exit non-zero (rather than silently writing an empty config or
/// hanging on `read_line`).
fn run_noninteractive(flags: InstallFlags) -> Result<()> {
    let cur = Config::load();
    let new = resolve_env_config(&cur);
    let ready = new.versions_db.is_some() && new.r2_bucket.is_some() && r2_keys_present();
    if !ready {
        print_agent_guide(&new);
        bail!(
            "not enough configuration in the environment and no TTY to prompt — \
             set the variables listed above and re-run `trove install`"
        );
    }
    provision(new, flags)
}

/// Guided, interactive path for a human at a terminal.
fn run_interactive(flags: InstallFlags) -> Result<()> {
    use colored::Colorize;
    let cur = Config::load();
    let cfg_path = Config::path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.config/trove/config.toml".to_string());

    println!("{}", "trove install — let's set up your vault.".bold());
    println!(
        "Writes {cfg_path}, applies the DB migration, and formats your object-store volume.\n"
    );
    println!("You'll need:");
    println!(
        "  {} a Postgres URL (Supabase / Neon / RDS / local) — metadata, version history, embeddings",
        "•".dimmed()
    );
    println!(
        "  {} an S3-compatible bucket + keys (Cloudflare R2, MinIO, AWS S3) — the file data",
        "•".dimmed()
    );
    println!(
        "  {} optionally an OpenAI API key — semantic search (skip it and mount with {})",
        "•".dimmed(),
        "--no-embed".cyan()
    );
    println!("\nFull walkthrough any time: {}\n", "trove docs quickstart".cyan());

    // (a) non-secret config prompts
    let new = Config {
        versions_db: ask("versions_db (postgres URL)", cur.versions_db.as_deref())?,
        volume: ask("volume name", cur.volume.as_deref().or(Some("trove")))?,
        meta: ask("meta URL (blank = same as versions_db)", cur.meta.as_deref())?,
        store: ask(
            "store (vault path, used by `trove doctor`'s validation sweep)",
            cur.store.as_deref(),
        )?,
        cache: ask("cache dir", cur.cache.as_deref().or(Some("/tmp/trove-cache")))?,
        r2_bucket: ask(
            "r2 bucket endpoint URL (https://<bucket>.<acct>.r2.cloudflarestorage.com)",
            cur.r2_bucket.as_deref(),
        )?,
        backup_dir: ask(
            "backup mirror directory [optional \u{2014} write a local copy of every committed file]",
            cur.backup_dir.as_deref(),
        )?,
    };

    // (b) secrets — prompt for any not already exported, kept out of config.
    println!(
        "\n{}",
        "Secrets (kept in your environment, never written to config):".bold()
    );
    let entered = collect_secrets_interactive()?;

    // (c) provision (shared with the non-interactive path)
    provision(new, flags)?;

    // (d) remind the user to persist anything typed this run, since it only
    // lives in this process — future `trove mount` runs need it too.
    if !entered.is_empty() {
        println!(
            "\n{} the secrets you just entered live only in this process. Persist them so future runs find them:",
            "note:".yellow().bold()
        );
        for (name, val) in &entered {
            println!("  export {name}={}", shell_quote(val));
        }
        println!(
            "  {}",
            "(add to ~/.envrc, your shell rc, or wrap the mount in `op run` / `1password run`)".dimmed()
        );
    }
    Ok(())
}

/// Shared provisioning: persist config, then run the migration + volume format
/// behind all the safety gates. Identical work whether a human or an agent
/// supplied the settings — only the data-gathering differs.
fn provision(new: Config, flags: InstallFlags) -> Result<()> {
    use colored::Colorize;

    // secrets pre-flight (informational)
    if env_nonempty("OPENAI_API_KEY").is_none() {
        eprintln!(
            "{} OPENAI_API_KEY is not set — embed/search will be unavailable until you export it (mount with --no-embed to skip).",
            "warning:".yellow().bold()
        );
    }
    let r2_keys_set = r2_keys_present();

    // Save the config BEFORE we touch the DB — a failed migration shouldn't
    // wipe the answers we just gathered.
    let written = new.save()?;
    println!("\n{} wrote {}", "trove:".bold(), written.display());

    let versions_db = new
        .versions_db
        .as_deref()
        .ok_or_else(|| anyhow!("versions_db is required for the migration step"))?;
    let volume = new
        .volume
        .as_deref()
        .ok_or_else(|| anyhow!("volume is required for the format step"))?;
    let meta_url = new.meta.as_deref().unwrap_or(versions_db);
    let bucket = new.r2_bucket.as_deref().unwrap_or("");

    // DB pre-flight + migration
    let mut client = Client::connect(versions_db, NoTls).with_context(|| {
        format!(
            "couldn't connect to {versions_db} — set up Postgres + create the DB first, then re-run `trove install`"
        )
    })?;
    let db_state = inspect_db(&mut client, bucket)?;
    let p = plan(&db_state, bucket, flags);
    apply_migration(&mut client, &p.migration, flags)?;

    // JuiceFS volume pre-flight + format
    if !r2_keys_set {
        match p.format {
            FormatAction::Format | FormatAction::DropAndReformat { .. } => bail!(
                "R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY are required to format the JuiceFS volume — export them and re-run"
            ),
            _ => {}
        }
    }
    apply_format(&mut client, &p.format, volume, meta_url, bucket, flags)?;

    // summary
    let schema_summary = match p.migration {
        MigrationAction::RunMigration | MigrationAction::DropAndRecreate { .. } => {
            "Trove schema created (3 tables, pgvector)"
        }
        _ => "Trove schema already present (kept)",
    };
    let format_summary = match &p.format {
        FormatAction::Format | FormatAction::DropAndReformat { .. } => {
            format!("JuiceFS volume `{volume}` formatted on `{bucket}`")
        }
        FormatAction::SkipSameBucket { bucket } => {
            format!("JuiceFS volume `{volume}` already formatted on `{bucket}` (kept)")
        }
        _ => format!("JuiceFS volume `{volume}` already present (kept)"),
    };
    println!();
    println!("{} config saved at {}", "✓".green(), written.display());
    println!("{} {}", "✓".green(), schema_summary);
    println!("{} {}", "✓".green(), format_summary);
    println!(
        "{} trove install complete. Run `trove mount /mnt/trove` to use it.",
        "✓".green()
    );
    Ok(())
}

/// Each secret trove reads, with a one-line blurb and where to get it. Drives
/// both the interactive prompts and the non-interactive guide so the two never
/// drift apart.
const SECRETS: [(&str, &str, &str); 3] = [
    (
        "OPENAI_API_KEY",
        "embeddings + `trove search` (optional)",
        "https://platform.openai.com/api-keys",
    ),
    (
        "R2_ACCESS_KEY_ID",
        "object-store access key id (needed to format the volume)",
        "Cloudflare dashboard \u{2192} R2 \u{2192} Manage API Tokens",
    ),
    (
        "R2_SECRET_ACCESS_KEY",
        "object-store secret key",
        "shown once when you create the R2 API token",
    ),
];

/// Prompt for each secret not already in the environment. Sets entered values
/// in the process env (so the format step downstream can read them) and returns
/// them so the caller can remind the user to persist them.
fn collect_secrets_interactive() -> Result<Vec<(&'static str, String)>> {
    use colored::Colorize;
    let mut entered = Vec::new();
    for (name, blurb, where_) in SECRETS {
        if env_nonempty(name).is_some() {
            println!("  {} {name} already set", "✓".green());
            continue;
        }
        println!("  {name} — {blurb}");
        println!("    {} {where_}", "where:".dimmed());
        let val = prompt_secret(&format!("    paste {name} (blank to skip): "))?;
        if val.is_empty() {
            continue;
        }
        std::env::set_var(name, &val);
        entered.push((name, val));
    }
    Ok(entered)
}

/// Print + flush a label, then read one line without echoing it to the terminal
/// (where the platform supports it). Trims the trailing newline.
fn prompt_secret(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    Ok(read_secret_line()?.trim().to_string())
}

/// Read a line with terminal echo disabled, restoring the prior terminal state
/// afterwards. Only compiled with the `mount` feature, which is the one that
/// pulls `libc` — and the only build that can actually format a volume, so a
/// no-`mount` install never reaches a real secret prompt.
#[cfg(feature = "mount")]
fn read_secret_line() -> io::Result<String> {
    use std::os::unix::io::AsRawFd;
    let fd = io::stdin().as_raw_fd();
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    let have_term = unsafe { libc::tcgetattr(fd, &mut term) } == 0;
    let saved = term;
    if have_term {
        term.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
    }
    let mut line = String::new();
    let res = io::stdin().read_line(&mut line);
    if have_term {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
        // The Enter the user pressed wasn't echoed; emit the newline ourselves.
        println!();
    }
    res?;
    Ok(line)
}

/// Fallback when `libc` isn't linked (core-only build): read with echo on.
#[cfg(not(feature = "mount"))]
fn read_secret_line() -> io::Result<String> {
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line)
}

/// Single-quote a value for a copy-pasteable `export`, closing and reopening the
/// quote around any embedded single quote.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The non-interactive setup guide: every env var trove reads, which are set,
/// the current resolved values, and a copy-pasteable block to finish. This is
/// what an agent sees when it runs `trove install` with no TTY and a
/// not-yet-complete environment.
fn print_agent_guide(resolved: &Config) {
    use colored::Colorize;
    let mark = |present: bool| if present { "✓".green() } else { "✗".red() };
    let cur = |v: &Option<String>| {
        v.as_deref()
            .map(|s| format!(" (currently: {s})").dimmed().to_string())
            .unwrap_or_default()
    };

    println!("{}", "trove install — non-interactive (no TTY detected)".bold());
    println!(
        "Reading settings from the environment instead of prompting. Set the variables\n\
         below, then re-run `trove install` — it provisions end-to-end with no prompts.\n"
    );

    println!("{}", "Required".bold());
    println!(
        "  {} TROVE_VERSIONS_DB     Postgres URL — metadata, version history, embeddings.{}",
        mark(resolved.versions_db.is_some()),
        cur(&resolved.versions_db)
    );
    println!("                          Accepts DATABASE_URL too. Use the hosted *session*");
    println!("                          pooler (port 5432) — JuiceFS keeps session state, so");
    println!("                          pgbouncer transaction mode (6543) breaks it.");
    println!("                          e.g. postgres://user:pass@host:5432/postgres");
    println!(
        "  {} TROVE_R2_BUCKET       Full S3 endpoint URL of the bucket.{}",
        mark(resolved.r2_bucket.is_some()),
        cur(&resolved.r2_bucket)
    );
    println!("                          e.g. https://<bucket>.<accountid>.r2.cloudflarestorage.com");
    println!(
        "  {} R2_ACCESS_KEY_ID      Object-store access key id (Cloudflare R2 \u{2192} API Tokens).",
        mark(env_nonempty("R2_ACCESS_KEY_ID").is_some())
    );
    println!(
        "  {} R2_SECRET_ACCESS_KEY  Object-store secret key (shown once at token creation).",
        mark(env_nonempty("R2_SECRET_ACCESS_KEY").is_some())
    );

    println!("\n{}", "Optional".bold());
    println!(
        "  {} OPENAI_API_KEY        Embeddings + `trove search`. Omit \u{2192} mount with --no-embed.",
        mark(env_nonempty("OPENAI_API_KEY").is_some())
    );
    println!(
        "  {} TROVE_VOLUME          JuiceFS volume name (default: trove).{}",
        mark(env_nonempty("TROVE_VOLUME").is_some()),
        cur(&resolved.volume)
    );
    println!(
        "  {} TROVE_META            JuiceFS metadata URL (default: = TROVE_VERSIONS_DB).",
        mark(env_nonempty("TROVE_META").is_some())
    );
    println!(
        "  {} TROVE_STORE           Vault path for `trove doctor`'s validation sweep.{}",
        mark(env_nonempty("TROVE_STORE").is_some()),
        cur(&resolved.store)
    );
    println!(
        "  {} TROVE_CACHE           Local block-cache dir (default: /tmp/trove-cache).",
        mark(env_nonempty("TROVE_CACHE").is_some())
    );

    println!("\n{}", "Finish setup".bold());
    println!("  export TROVE_VERSIONS_DB='postgres://…'");
    println!("  export TROVE_R2_BUCKET='https://<bucket>.<acct>.r2.cloudflarestorage.com'");
    println!("  export R2_ACCESS_KEY_ID='…' R2_SECRET_ACCESS_KEY='…'");
    println!("  export OPENAI_API_KEY='sk-…'      # optional");
    println!("  trove install                     # migration + volume format, no prompts");
    println!("  trove doctor                      # confirm everything is green");

    println!("\n{}", "Docs (no server needed)".bold());
    println!("  trove docs quickstart   one page   ·   trove docs --all   whole manual   ·   trove docs   list pages");
    println!(
        "\nSafety flags: --reuse keeps existing data; --reinstall wipes it (refuses without a TTY)."
    );
}

fn ask(label: &str, current: Option<&str>) -> io::Result<Option<String>> {
    use colored::Colorize;
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
}

/// Build a [`DbState`] snapshot. Reads only — no schema changes.
pub fn inspect_db(client: &mut Client, _requested_bucket: &str) -> Result<DbState> {
    let mut tables_present = HashSet::new();
    let mut tables_with_rows = HashSet::new();
    for table in SCHEMA_TABLES {
        let present: bool = client
            .query_one("select to_regclass($1) is not null", &[&table])?
            .get(0);
        if present {
            tables_present.insert(table.to_string());
            // Use a parameterised count — safe because `table` comes from a
            // fixed allow-list, but `format!` keeps the table name out of the
            // user-controllable channel.
            let q = format!("select count(*) from {table}");
            let n: i64 = client.query_one(&q, &[])?.get(0);
            if n > 0 {
                tables_with_rows.insert(table.to_string());
            }
        }
    }
    // jfs_* probe (escape the underscore so it's matched literally).
    let jfs_count: i64 = client
        .query_one(
            "select count(*) from information_schema.tables \
             where table_schema = 'public' and table_name like 'jfs\\_%' escape '\\'",
            &[],
        )?
        .get(0);
    let jfs_present = jfs_count > 0;

    // Recorded bucket from jfs_setting (name='format', value is JSON).
    let recorded_bucket = if jfs_present {
        // jfs_setting may not exist if JuiceFS is mid-format / damaged.
        let setting_present: bool = client
            .query_one("select to_regclass('jfs_setting') is not null", &[])?
            .get(0);
        if setting_present {
            let row = client
                .query_opt(
                    "select value from jfs_setting where name = 'format' limit 1",
                    &[],
                )?
                .and_then(|r| r.try_get::<_, String>(0).ok());
            row.and_then(|v| parse_bucket_from_format_json(&v))
        } else {
            None
        }
    } else {
        None
    };

    Ok(DbState {
        tables_present,
        tables_with_rows,
        jfs_present,
        recorded_bucket,
    })
}

/// JuiceFS stores its format as a Go-struct JSON in `jfs_setting`. Pull out the
/// `Bucket` field (a URL like `https://<bucket>.<acct>.r2.cloudflarestorage.com`
/// or `s3://…`). Returns `None` if the JSON is unparseable or has no `Bucket`.
fn parse_bucket_from_format_json(s: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    // JuiceFS's Format struct uses `Bucket` (capitalised). Be tolerant of both.
    v.get("Bucket")
        .or_else(|| v.get("bucket"))
        .and_then(|b| b.as_str())
        .map(|s| s.to_string())
}

fn apply_migration(
    client: &mut Client,
    action: &MigrationAction,
    _flags: InstallFlags,
) -> Result<()> {
    use colored::Colorize;
    match action {
        MigrationAction::RunMigration => {
            println!("{} running migration…", "trove install:".bold());
            client
                .batch_execute(MIGRATION_SQL)
                .context("applying Trove migration")?;
            println!("  {} migration applied", "✓".green());
            Ok(())
        }
        MigrationAction::SkipExisting { reason } => {
            println!("{} {} — skipping migration", "trove install:".bold(), reason);
            Ok(())
        }
        MigrationAction::ReuseExisting { populated_table, row_count } => {
            println!(
                "{} schema present with {} row(s) in {} — keeping (`--reuse`)",
                "trove install:".bold(),
                row_count,
                populated_table
            );
            Ok(())
        }
        MigrationAction::DropAndRecreate { populated_table, row_count } => {
            println!(
                "{} schema present with {} row(s) in {} — `--reinstall` requested",
                "trove install:".bold(),
                row_count,
                populated_table
            );
            confirm_destroy("DROP and recreate the Trove schema? Existing data will be destroyed.")?;
            client
                .batch_execute(
                    "drop table if exists blob_chunks cascade; \
                     drop table if exists file_versions cascade; \
                     drop table if exists blobs cascade; \
                     drop extension if exists vector;",
                )
                .context("dropping old Trove schema")?;
            client
                .batch_execute(MIGRATION_SQL)
                .context("re-applying Trove migration")?;
            println!("  {} schema dropped and recreated", "✓".green());
            Ok(())
        }
        MigrationAction::RefuseNonEmpty { populated_table, row_count } => {
            bail!(
                "Trove tables already populated ({row_count} row(s) in `{populated_table}`). \
                 Pass `--reuse` to keep them, or `--reinstall` to wipe (destructive)."
            )
        }
    }
}

fn apply_format(
    client: &mut Client,
    action: &FormatAction,
    volume: &str,
    meta_url: &str,
    bucket: &str,
    _flags: InstallFlags,
) -> Result<()> {
    use colored::Colorize;
    match action {
        FormatAction::Format => {
            println!(
                "{} formatting JuiceFS volume `{volume}` on `{bucket}`…",
                "trove install:".bold()
            );
            run_juicefs_format(volume, meta_url, bucket)?;
            println!("  {} volume formatted", "✓".green());
            Ok(())
        }
        FormatAction::SkipSameBucket { bucket } => {
            println!(
                "{} JuiceFS volume already formatted on `{bucket}` — skipping format",
                "trove install:".bold()
            );
            Ok(())
        }
        FormatAction::ReuseExisting => {
            println!(
                "{} JuiceFS volume already present — keeping (`--reuse`)",
                "trove install:".bold()
            );
            Ok(())
        }
        FormatAction::DropAndReformat { recorded, requested } => {
            println!(
                "{} JuiceFS volume recorded on `{recorded}`, want `{requested}` — `--reinstall` requested",
                "trove install:".bold()
            );
            confirm_destroy(
                "DROP the JuiceFS metadata and reformat? Existing data in the old bucket will be orphaned.",
            )?;
            drop_jfs_tables(client)?;
            run_juicefs_format(volume, meta_url, bucket)?;
            println!("  {} volume reformatted", "✓".green());
            Ok(())
        }
        FormatAction::RefuseBucketMismatch { recorded, requested } => bail!(
            "JuiceFS metadata in this DB references bucket `{recorded}`, not `{requested}`. \
             Re-formatting against `{requested}` would orphan the existing chunks in `{recorded}`. \
             Refusing to continue. To proceed anyway: `--reinstall`."
        ),
    }
}

/// Drop every `jfs_*` table in the public schema. Used by `--reinstall` after
/// the user has typed `destroy`.
fn drop_jfs_tables(client: &mut Client) -> Result<()> {
    let rows = client.query(
        "select table_name from information_schema.tables \
         where table_schema = 'public' and table_name like 'jfs\\_%' escape '\\'",
        &[],
    )?;
    let mut stmt = String::new();
    for r in &rows {
        let name: String = r.get(0);
        // Schema-allow-list: only names matching `jfs_*` from information_schema.
        stmt.push_str(&format!("drop table if exists \"{name}\" cascade; "));
    }
    if !stmt.is_empty() {
        client
            .batch_execute(&stmt)
            .context("dropping JuiceFS metadata tables")?;
    }
    Ok(())
}

/// Prompt for an explicit `destroy` confirmation. Anything else aborts. With no
/// TTY (an agent / script is driving us) there's no safe way to confirm an
/// irreversible step, so we refuse outright rather than read a dead stdin.
fn confirm_destroy(prompt: &str) -> Result<()> {
    use colored::Colorize;
    if !io::stdin().is_terminal() {
        bail!(
            "{prompt}\nRefusing this destructive step without a TTY — can't take an \
             interactive confirmation. Re-run `trove install` in a terminal if you really mean it."
        );
    }
    println!("{}", prompt.yellow().bold());
    print!("Type 'destroy' to proceed (anything else aborts): ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    if line.trim() != "destroy" {
        bail!("aborted — destruction not confirmed");
    }
    Ok(())
}

/// Format a JuiceFS volume in-process via libjfs's `jfs_format` FFI entry —
/// no `juicefs` binary on PATH required. libjfs runs the same blob-store
/// sanity probe (put + get + delete a tiny object) that the CLI does before
/// persisting the format row, so misconfigured creds / missing bucket fail
/// fast with a useful errno in the logs.
#[cfg(feature = "mount")]
fn run_juicefs_format(volume: &str, meta_url: &str, bucket: &str) -> Result<()> {
    let access = std::env::var("R2_ACCESS_KEY_ID").unwrap_or_default();
    let secret = std::env::var("R2_SECRET_ACCESS_KEY").unwrap_or_default();
    let conf = serde_json::json!({
        "meta": meta_url,
        "name": volume,
        "storage": "s3",
        "bucket": bucket,
        "accessKey": access,
        "secretKey": secret,
    });
    crate::jfs::format(&conf).with_context(|| {
        format!("formatting JuiceFS volume `{volume}` on bucket `{bucket}` via libjfs")
    })
}

/// Stub when libjfs isn't linked in — the core crate (without `mount`) cannot
/// format a volume. `trove install` is itself currently mount-only on the IO
/// path (it talks to Postgres), so reaching this is a misconfiguration.
#[cfg(not(feature = "mount"))]
fn run_juicefs_format(_volume: &str, _meta_url: &str, _bucket: &str) -> Result<()> {
    bail!("trove was built without the `mount` feature — libjfs not linked, cannot format a volume")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(tables: &[&str], rows: &[&str], jfs: bool, bucket: Option<&str>) -> DbState {
        DbState {
            tables_present: tables.iter().map(|s| s.to_string()).collect(),
            tables_with_rows: rows.iter().map(|s| s.to_string()).collect(),
            jfs_present: jfs,
            recorded_bucket: bucket.map(|s| s.to_string()),
        }
    }

    // -- migration decisions --

    #[test]
    fn empty_db_runs_migration() {
        let p = plan(&st(&[], &[], false, None), "b", InstallFlags::default());
        assert_eq!(p.migration, MigrationAction::RunMigration);
        assert_eq!(p.format, FormatAction::Format);
    }

    #[test]
    fn schema_present_but_empty_skips() {
        let db = st(&["blobs", "file_versions", "blob_chunks"], &[], false, None);
        let p = plan(&db, "b", InstallFlags::default());
        matches!(p.migration, MigrationAction::SkipExisting { .. });
    }

    #[test]
    fn populated_no_flags_refuses() {
        let db = st(
            &["blobs", "file_versions", "blob_chunks"],
            &["blobs"],
            false,
            None,
        );
        let p = plan(&db, "b", InstallFlags::default());
        assert!(matches!(p.migration, MigrationAction::RefuseNonEmpty { .. }));
    }

    #[test]
    fn populated_with_reuse_keeps() {
        let db = st(
            &["blobs", "file_versions", "blob_chunks"],
            &["blobs"],
            false,
            None,
        );
        let p = plan(
            &db,
            "b",
            InstallFlags {
                reuse: true,
                reinstall: false,
            },
        );
        assert!(matches!(p.migration, MigrationAction::ReuseExisting { .. }));
    }

    #[test]
    fn populated_with_reinstall_drops_and_recreates() {
        let db = st(
            &["blobs", "file_versions", "blob_chunks"],
            &["file_versions"],
            false,
            None,
        );
        let p = plan(
            &db,
            "b",
            InstallFlags {
                reuse: false,
                reinstall: true,
            },
        );
        assert!(matches!(p.migration, MigrationAction::DropAndRecreate { .. }));
    }

    #[test]
    fn partial_table_presence_treated_as_present() {
        // Only `blobs` exists from a previous half-applied migration. Not
        // "empty schema" — treat as populated-or-half-built; the safe path is
        // to fall into the "present" branch rather than RunMigration (which
        // would error on re-creating `blobs`).
        let db = st(&["blobs"], &[], false, None);
        let p = plan(&db, "b", InstallFlags::default());
        // No rows anywhere, so SkipExisting is the safe default.
        assert!(matches!(p.migration, MigrationAction::SkipExisting { .. }));
    }

    // -- format decisions --

    #[test]
    fn no_jfs_tables_formats() {
        let db = st(&[], &[], false, None);
        let p = plan(&db, "https://x.r2", InstallFlags::default());
        assert_eq!(p.format, FormatAction::Format);
    }

    #[test]
    fn jfs_same_bucket_skips() {
        let db = st(&[], &[], true, Some("https://x.r2"));
        let p = plan(&db, "https://x.r2", InstallFlags::default());
        assert_eq!(
            p.format,
            FormatAction::SkipSameBucket {
                bucket: "https://x.r2".into()
            }
        );
    }

    #[test]
    fn jfs_different_bucket_refuses_without_flag() {
        let db = st(&[], &[], true, Some("https://old.r2"));
        let p = plan(&db, "https://new.r2", InstallFlags::default());
        assert_eq!(
            p.format,
            FormatAction::RefuseBucketMismatch {
                recorded: "https://old.r2".into(),
                requested: "https://new.r2".into(),
            }
        );
    }

    #[test]
    fn jfs_different_bucket_with_reuse_keeps() {
        let db = st(&[], &[], true, Some("https://old.r2"));
        let p = plan(
            &db,
            "https://new.r2",
            InstallFlags {
                reuse: true,
                reinstall: false,
            },
        );
        assert_eq!(p.format, FormatAction::ReuseExisting);
    }

    #[test]
    fn jfs_different_bucket_with_reinstall_drops_and_reformats() {
        let db = st(&[], &[], true, Some("https://old.r2"));
        let p = plan(
            &db,
            "https://new.r2",
            InstallFlags {
                reuse: false,
                reinstall: true,
            },
        );
        assert_eq!(
            p.format,
            FormatAction::DropAndReformat {
                recorded: "https://old.r2".into(),
                requested: "https://new.r2".into(),
            }
        );
    }

    #[test]
    fn jfs_unknown_bucket_defaults_to_reuse() {
        // We can see jfs_* tables but couldn't parse the format row. Be safe.
        let db = st(&[], &[], true, None);
        let p = plan(&db, "https://x.r2", InstallFlags::default());
        assert_eq!(p.format, FormatAction::ReuseExisting);
    }

    #[test]
    fn jfs_unknown_bucket_with_reinstall_still_reformats() {
        let db = st(&[], &[], true, None);
        let p = plan(
            &db,
            "https://x.r2",
            InstallFlags {
                reuse: false,
                reinstall: true,
            },
        );
        assert!(matches!(p.format, FormatAction::DropAndReformat { .. }));
    }

    // -- bucket JSON parsing --

    #[test]
    fn parses_bucket_from_juicefs_format_json() {
        let j = r#"{"Name":"trove","UUID":"abc","Storage":"s3","Bucket":"https://x.r2.cloudflarestorage.com","BlockSize":4096}"#;
        assert_eq!(
            parse_bucket_from_format_json(j).as_deref(),
            Some("https://x.r2.cloudflarestorage.com")
        );
    }

    #[test]
    fn missing_bucket_returns_none() {
        let j = r#"{"Name":"trove"}"#;
        assert_eq!(parse_bucket_from_format_json(j), None);
    }

    #[test]
    fn malformed_format_json_returns_none() {
        assert_eq!(parse_bucket_from_format_json("not json"), None);
    }

    // -- env resolution / helpers (non-interactive path) --

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote("simple"), "'simple'");
        // An embedded single quote closes, escapes, reopens.
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn env_nonempty_treats_empty_as_unset() {
        std::env::set_var("TROVE_TEST_NONEMPTY", "");
        assert_eq!(env_nonempty("TROVE_TEST_NONEMPTY"), None);
        std::env::set_var("TROVE_TEST_NONEMPTY", "x");
        assert_eq!(env_nonempty("TROVE_TEST_NONEMPTY").as_deref(), Some("x"));
        std::env::remove_var("TROVE_TEST_NONEMPTY");
    }

    #[test]
    fn resolve_env_config_applies_precedence_and_defaults() {
        // Start from a clean slate for the vars we assert on.
        for v in [
            "TROVE_VERSIONS_DB", "DATABASE_URL", "TROVE_VOLUME",
            "TROVE_META", "TROVE_CACHE", "TROVE_R2_BUCKET", "TROVE_STORE",
        ] {
            std::env::remove_var(v);
        }
        let cur = Config::default();

        // No env, empty config → volume/cache get defaults; meta mirrors db (None here).
        let c = resolve_env_config(&cur);
        assert_eq!(c.volume.as_deref(), Some("trove"));
        assert_eq!(c.cache.as_deref(), Some("/tmp/trove-cache"));
        assert_eq!(c.versions_db, None);

        // DATABASE_URL is accepted as an alias and feeds meta's default.
        std::env::set_var("DATABASE_URL", "postgres://db-alias");
        let c = resolve_env_config(&cur);
        assert_eq!(c.versions_db.as_deref(), Some("postgres://db-alias"));
        assert_eq!(c.meta.as_deref(), Some("postgres://db-alias"));

        // TROVE_VERSIONS_DB wins over DATABASE_URL.
        std::env::set_var("TROVE_VERSIONS_DB", "postgres://canonical");
        let c = resolve_env_config(&cur);
        assert_eq!(c.versions_db.as_deref(), Some("postgres://canonical"));

        for v in ["TROVE_VERSIONS_DB", "DATABASE_URL"] {
            std::env::remove_var(v);
        }
    }
}
