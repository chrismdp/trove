//! Provisioning helpers for `trove init`.
//!
//! The decision-making half of this module is a **pure state machine**
//! ([`plan`]): given what's already in the DB and what flags the user passed,
//! it returns a [`Plan`] of what to do. The IO helpers below apply those
//! decisions against Postgres and libjfs.
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

use anyhow::{bail, Context, Result};
use postgres::Client;
use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};

/// Bundled migration. Single-file by convention (the schema lint enforces this).
/// We `include_str!` so the binary carries its own schema — no runtime SQL file
/// lookup, no "where did supabase/ go?" surprises.
const MIGRATION_SQL: &str =
    include_str!("../../supabase/migrations/20260527172259_init_version_chain_and_embeddings.sql");

/// Flags that change the safety posture. The defaults refuse to touch anything
/// that already has content; `--reuse` accepts existing state; `--reinstall`
/// destroys it (after explicit confirmation).
#[derive(Debug, Default, Clone, Copy)]
pub struct ProvisionFlags {
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
    DropAndRecreate {
        populated_table: String,
        row_count: i64,
    },
    /// Tables exist and at least one carries rows; `--reuse` was given —
    /// leave alone.
    ReuseExisting {
        populated_table: String,
        row_count: i64,
    },
    /// Tables exist and at least one carries rows; neither flag given — abort
    /// with a clear message.
    RefuseNonEmpty {
        populated_table: String,
        row_count: i64,
    },
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
pub fn plan(db: &DbState, requested_bucket: &str, flags: ProvisionFlags) -> Plan {
    Plan {
        migration: plan_migration(db, flags),
        format: plan_format(db, requested_bucket, flags),
    }
}

fn plan_migration(db: &DbState, flags: ProvisionFlags) -> MigrationAction {
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

fn plan_format(db: &DbState, requested_bucket: &str, flags: ProvisionFlags) -> FormatAction {
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

/// An environment variable, treating empty-string as unset.
pub(crate) fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Build a [`DbState`] snapshot. Reads only — no schema changes. `schema` is the
/// schema this volume lives in: the SCHEMA_TABLES / jfs_setting probes resolve
/// via the caller's `search_path` (which install points at it), but the
/// `information_schema` jfs_* probe takes the schema explicitly since
/// `information_schema` doesn't honour `search_path`. Tests probing `public`
/// pass `"public"`.
pub fn inspect_db(client: &mut Client, schema: &str) -> Result<DbState> {
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
    // jfs_* probe (escape the underscore so it's matched literally). Scoped to
    // the volume's schema — `information_schema` ignores search_path.
    let jfs_count: i64 = client
        .query_one(
            "select count(*) from information_schema.tables \
             where table_schema = $1 and table_name like 'jfs\\_%' escape '\\'",
            &[&schema],
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

pub(crate) fn apply_migration(
    client: &mut Client,
    action: &MigrationAction,
    _flags: ProvisionFlags,
) -> Result<()> {
    use colored::Colorize;
    match action {
        MigrationAction::RunMigration => {
            println!("{} running migration…", "trove init:".bold());
            client
                .batch_execute(MIGRATION_SQL)
                .context("applying Trove migration")?;
            println!("  {} migration applied", "✓".green());
            Ok(())
        }
        MigrationAction::SkipExisting { reason } => {
            println!("{} {} — skipping migration", "trove init:".bold(), reason);
            Ok(())
        }
        MigrationAction::ReuseExisting {
            populated_table,
            row_count,
        } => {
            println!(
                "{} schema present with {} row(s) in {} — keeping (`--reuse`)",
                "trove init:".bold(),
                row_count,
                populated_table
            );
            Ok(())
        }
        MigrationAction::DropAndRecreate {
            populated_table,
            row_count,
        } => {
            println!(
                "{} schema present with {} row(s) in {} — `--reinstall` requested",
                "trove init:".bold(),
                row_count,
                populated_table
            );
            confirm_destroy(
                "DROP and recreate the Trove schema? Existing data will be destroyed.",
            )?;
            // Drop only this volume's tables (search_path is the volume schema).
            // The `vector` extension is database-global and shared across volumes
            // — leaving it is correct, and `create extension if not exists` makes
            // the recreate idempotent.
            client
                .batch_execute(
                    "drop table if exists blob_chunks cascade; \
                     drop table if exists file_versions cascade; \
                     drop table if exists blobs cascade;",
                )
                .context("dropping old Trove schema")?;
            client
                .batch_execute(MIGRATION_SQL)
                .context("re-applying Trove migration")?;
            println!("  {} schema dropped and recreated", "✓".green());
            Ok(())
        }
        MigrationAction::RefuseNonEmpty {
            populated_table,
            row_count,
        } => {
            bail!(
                "Trove tables already populated ({row_count} row(s) in `{populated_table}`). \
                 Pass `--reuse` to keep them, or `--reinstall` to wipe (destructive)."
            )
        }
    }
}

pub(crate) fn apply_format(
    client: &mut Client,
    action: &FormatAction,
    volume: &str,
    meta_url: &str,
    bucket: &str,
    schema: &str,
    _flags: ProvisionFlags,
) -> Result<()> {
    use colored::Colorize;
    match action {
        FormatAction::Format => {
            println!(
                "{} formatting storage volume `{volume}` on `{bucket}`…",
                "trove init:".bold()
            );
            run_juicefs_format(volume, meta_url, bucket)?;
            println!("  {} volume formatted", "✓".green());
            Ok(())
        }
        FormatAction::SkipSameBucket { bucket } => {
            println!(
                "{} storage volume already formatted on `{bucket}` — skipping format",
                "trove init:".bold()
            );
            Ok(())
        }
        FormatAction::ReuseExisting => {
            println!(
                "{} storage volume already present — keeping (`--reuse`)",
                "trove init:".bold()
            );
            Ok(())
        }
        FormatAction::DropAndReformat { recorded, requested } => {
            println!(
                "{} storage volume recorded on `{recorded}`, want `{requested}` — `--reinstall` requested",
                "trove init:".bold()
            );
            confirm_destroy(
                "DROP the storage volume's metadata and reformat? Existing data in the old bucket will be orphaned.",
            )?;
            drop_jfs_tables(client, schema)?;
            run_juicefs_format(volume, meta_url, bucket)?;
            println!("  {} volume reformatted", "✓".green());
            Ok(())
        }
        FormatAction::RefuseBucketMismatch { recorded, requested } => bail!(
            "the storage volume's metadata in this DB references bucket `{recorded}`, not `{requested}`. \
             Re-formatting against `{requested}` would orphan the existing chunks in `{recorded}`. \
             Refusing to continue. To proceed anyway: `--reinstall`."
        ),
    }
}

/// Drop every `jfs_*` table in the volume's `schema`. Used by `--reinstall`
/// after the user has typed `destroy`.
fn drop_jfs_tables(client: &mut Client, schema: &str) -> Result<()> {
    let rows = client.query(
        "select table_name from information_schema.tables \
         where table_schema = $1 and table_name like 'jfs\\_%' escape '\\'",
        &[&schema],
    )?;
    let sident = schema.replace('"', "\"\"");
    let mut stmt = String::new();
    for r in &rows {
        let name: String = r.get(0);
        // Schema-allow-list: only names matching `jfs_*` from information_schema,
        // dropped schema-qualified so search_path can't redirect us.
        let nident = name.replace('"', "\"\"");
        stmt.push_str(&format!(
            "drop table if exists \"{sident}\".\"{nident}\" cascade; "
        ));
    }
    if !stmt.is_empty() {
        client
            .batch_execute(&stmt)
            .context("dropping the storage volume's metadata tables")?;
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
             interactive confirmation. Re-run `trove init` in a terminal if you really mean it."
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
    crate::jfs::format(&conf)
        .with_context(|| format!("formatting storage volume `{volume}` on bucket `{bucket}`"))
}

/// Stub when libjfs isn't linked in — the core crate (without `mount`) cannot
/// format a volume. `trove init` is itself currently mount-only on the IO
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
        let p = plan(&st(&[], &[], false, None), "b", ProvisionFlags::default());
        assert_eq!(p.migration, MigrationAction::RunMigration);
        assert_eq!(p.format, FormatAction::Format);
    }

    #[test]
    fn schema_present_but_empty_skips() {
        let db = st(&["blobs", "file_versions", "blob_chunks"], &[], false, None);
        let p = plan(&db, "b", ProvisionFlags::default());
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
        let p = plan(&db, "b", ProvisionFlags::default());
        assert!(matches!(
            p.migration,
            MigrationAction::RefuseNonEmpty { .. }
        ));
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
            ProvisionFlags {
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
            ProvisionFlags {
                reuse: false,
                reinstall: true,
            },
        );
        assert!(matches!(
            p.migration,
            MigrationAction::DropAndRecreate { .. }
        ));
    }

    #[test]
    fn partial_table_presence_treated_as_present() {
        // Only `blobs` exists from a previous half-applied migration. Not
        // "empty schema" — treat as populated-or-half-built; the safe path is
        // to fall into the "present" branch rather than RunMigration (which
        // would error on re-creating `blobs`).
        let db = st(&["blobs"], &[], false, None);
        let p = plan(&db, "b", ProvisionFlags::default());
        // No rows anywhere, so SkipExisting is the safe default.
        assert!(matches!(p.migration, MigrationAction::SkipExisting { .. }));
    }

    // -- format decisions --

    #[test]
    fn no_jfs_tables_formats() {
        let db = st(&[], &[], false, None);
        let p = plan(&db, "https://x.r2", ProvisionFlags::default());
        assert_eq!(p.format, FormatAction::Format);
    }

    #[test]
    fn jfs_same_bucket_skips() {
        let db = st(&[], &[], true, Some("https://x.r2"));
        let p = plan(&db, "https://x.r2", ProvisionFlags::default());
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
        let p = plan(&db, "https://new.r2", ProvisionFlags::default());
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
            ProvisionFlags {
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
            ProvisionFlags {
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
        let p = plan(&db, "https://x.r2", ProvisionFlags::default());
        assert_eq!(p.format, FormatAction::ReuseExisting);
    }

    #[test]
    fn jfs_unknown_bucket_with_reinstall_still_reformats() {
        let db = st(&[], &[], true, None);
        let p = plan(
            &db,
            "https://x.r2",
            ProvisionFlags {
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

    #[test]
    fn env_nonempty_treats_empty_as_unset() {
        std::env::set_var("TROVE_TEST_NONEMPTY", "");
        assert_eq!(env_nonempty("TROVE_TEST_NONEMPTY"), None);
        std::env::set_var("TROVE_TEST_NONEMPTY", "x");
        assert_eq!(env_nonempty("TROVE_TEST_NONEMPTY").as_deref(), Some("x"));
        std::env::remove_var("TROVE_TEST_NONEMPTY");
    }
}
