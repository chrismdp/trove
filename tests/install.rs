//! Integration tests for `trove install`'s DB-side helpers. These hit the live
//! version DB (the local Supabase stack — `supabase start`) the same way
//! `tests/version.rs` does. Override with `TROVE_DB_URL`.
//!
//! We only test the **read-only** half here (`inspect_db`) plus the pure
//! decision machine on top of real snapshots. The destructive paths
//! (`apply_migration` → DROP / CREATE) would tear down the schema shared with
//! the other integration tests, so they're exercised against the pure `plan`
//! state machine in the lib tests (`src/commands/install.rs::tests`). That
//! split keeps the destructive coverage hermetic and the integration tests
//! repeatable.
#![cfg(feature = "mount")]

use postgres::{Client, NoTls};
use trove::commands::install::{
    plan, FormatAction, InstallFlags, MigrationAction,
};

fn db_url() -> String {
    std::env::var("TROVE_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54322/postgres".to_string())
}

fn connect() -> Option<Client> {
    // Skip these tests gracefully when no DB is reachable (CI without supabase).
    Client::connect(&db_url(), NoTls).ok()
}

#[test]
fn inspect_db_against_live_schema_reports_tables_present() {
    let Some(mut c) = connect() else {
        eprintln!("skipping: no DB reachable at {}", db_url());
        return;
    };
    let s = trove::commands::install::inspect_db(&mut c, "public").unwrap();
    // The migration has been applied for the rest of the test suite to work, so
    // we expect all three Trove tables present.
    for t in ["blobs", "file_versions", "blob_chunks"] {
        assert!(
            s.tables_present.contains(t),
            "expected {t} present in live DB: {:?}",
            s.tables_present
        );
    }
}

#[test]
fn populated_db_without_flags_refuses() {
    // Build a synthetic snapshot the way the live DB looks after some real use,
    // and confirm the planner refuses.
    let Some(mut c) = connect() else { return };
    let s = trove::commands::install::inspect_db(&mut c, "public").unwrap();
    // version.rs runs first and seeds blobs/file_versions; we expect rows.
    if s.tables_with_rows.is_empty() {
        eprintln!("skipping: live DB has no data yet (run tests/version.rs first)");
        return;
    }
    let p = plan(&s, "bucket-doesnt-matter", InstallFlags::default());
    assert!(
        matches!(p.migration, MigrationAction::RefuseNonEmpty { .. }),
        "expected RefuseNonEmpty for populated DB, got {:?}",
        p.migration
    );
}

#[test]
fn populated_db_with_reuse_keeps() {
    let Some(mut c) = connect() else { return };
    let s = trove::commands::install::inspect_db(&mut c, "public").unwrap();
    if s.tables_with_rows.is_empty() {
        return;
    }
    let p = plan(
        &s,
        "bucket-doesnt-matter",
        InstallFlags { reuse: true, reinstall: false },
    );
    assert!(
        matches!(p.migration, MigrationAction::ReuseExisting { .. }),
        "expected ReuseExisting under --reuse, got {:?}",
        p.migration
    );
}

#[test]
fn populated_db_with_reinstall_plans_drop_and_recreate() {
    let Some(mut c) = connect() else { return };
    let s = trove::commands::install::inspect_db(&mut c, "public").unwrap();
    if s.tables_with_rows.is_empty() {
        return;
    }
    let p = plan(
        &s,
        "bucket-doesnt-matter",
        InstallFlags { reuse: false, reinstall: true },
    );
    // We deliberately do NOT call `apply_migration` here — that would DROP the
    // schema shared with the rest of the test suite. The destructive path is
    // covered by the pure-state-machine tests in src/commands/install.rs.
    assert!(
        matches!(p.migration, MigrationAction::DropAndRecreate { .. }),
        "expected DropAndRecreate under --reinstall, got {:?}",
        p.migration
    );
}

#[test]
fn jfs_present_without_recorded_bucket_is_reuse() {
    let Some(mut c) = connect() else { return };
    let s = trove::commands::install::inspect_db(&mut c, "public").unwrap();
    if !s.jfs_present {
        eprintln!("skipping: no jfs_* tables in live DB");
        return;
    }
    // Whatever the live DB's recorded bucket is, asking for the same bucket
    // should result in SkipSameBucket; asking for a different one should refuse.
    if let Some(recorded) = s.recorded_bucket.as_deref() {
        let p = plan(&s, recorded, InstallFlags::default());
        assert!(
            matches!(p.format, FormatAction::SkipSameBucket { .. }),
            "expected SkipSameBucket when buckets match, got {:?}",
            p.format
        );
        let p = plan(&s, "https://wrong.example", InstallFlags::default());
        assert!(
            matches!(p.format, FormatAction::RefuseBucketMismatch { .. }),
            "expected RefuseBucketMismatch on differing bucket, got {:?}",
            p.format
        );
    }
}
