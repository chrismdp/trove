//! Tests for `trove doctor`'s checks against the live version DB (local
//! supabase). Run with `--features mount` and `source ~/.secret_env`. We assert
//! the DB/pgvector/schema checks pass when the stack is up, and that a bad DB
//! URL is reported as a failure rather than panicking.
#![cfg(feature = "mount")]

use trove::commands::doctor;
use trove::config::Config;

fn db_url() -> String {
    std::env::var("TROVE_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54322/postgres".to_string())
}

fn find<'a>(checks: &'a [doctor::Check], name: &str) -> &'a doctor::Check {
    checks.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no check named {name}"))
}

#[test]
fn reports_db_pgvector_and_schema_ok_when_stack_is_up() {
    let cfg = Config::default();
    let checks = doctor::run(&cfg, Some(db_url()), None, None, None);

    assert!(find(&checks, "versions DB").ok, "DB should be reachable");
    assert!(find(&checks, "pgvector").ok, "pgvector should be installed");
    assert!(
        find(&checks, "schema tables").ok,
        "schema tables should be present: {}",
        find(&checks, "schema tables").detail
    );
}

#[test]
fn unreachable_db_is_a_failure_not_a_panic() {
    let cfg = Config::default();
    // A port nothing listens on — connect must fail gracefully.
    let bad = "postgres://postgres:postgres@127.0.0.1:5/postgres".to_string();
    let checks = doctor::run(&cfg, Some(bad), None, None, None);
    assert!(!find(&checks, "versions DB").ok, "an unreachable DB is a failed check");
}

#[test]
fn missing_versions_db_is_reported_not_panicked() {
    // No flag, no env, empty config — the DB check should fail cleanly.
    std::env::remove_var("TROVE_VERSIONS_DB");
    let checks = doctor::run(&Config::default(), None, None, None, None);
    assert!(!find(&checks, "versions DB").ok);
}
