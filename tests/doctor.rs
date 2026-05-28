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
    let checks = doctor::run(&cfg, Some(db_url()), None, None, None, None);

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
    let checks = doctor::run(&cfg, Some(bad), None, None, None, None);
    assert!(!find(&checks, "versions DB").ok, "an unreachable DB is a failed check");
}

#[test]
fn missing_versions_db_is_reported_not_panicked() {
    // No flag, no env, empty config — the DB check should fail cleanly.
    std::env::remove_var("TROVE_VERSIONS_DB");
    let checks = doctor::run(&Config::default(), None, None, None, None, None);
    assert!(!find(&checks, "versions DB").ok);
}

#[test]
fn broken_schema_fails_lint_and_skips_store_validation() {
    // Build a tmp store with one broken .types/*.json. The lint row should fail
    // and the store-validation row should report "skipped".
    let store = std::env::temp_dir().join(format!(
        "trove-doctor-broken-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let types = store.join(".types");
    std::fs::create_dir_all(&types).unwrap();
    std::fs::write(types.join("broken.json"), "this is not json {").unwrap();

    let checks = doctor::run(&Config::default(), None, None, None, None, Some(store.clone()));

    let lint = find(&checks, "schema lint");
    assert!(!lint.ok, "lint row should fail: {}", lint.detail);
    assert!(
        lint.detail.contains("errors") || lint.detail.contains("error"),
        "lint detail should mention errors: {}",
        lint.detail
    );

    let val = find(&checks, "store validation");
    assert!(!val.ok, "store validation should be marked failed when lint fails");
    assert!(
        val.detail.contains("skipped"),
        "store validation detail should say skipped: {}",
        val.detail
    );

    let _ = std::fs::remove_dir_all(&store);
}
