//! Config-level lifecycle integration: volume listing, name-based resolution
//! through credential profiles, and `detach`'s config removal — all without a
//! backend. Runs as its own test binary so its `XDG_CONFIG_HOME` override is
//! isolated from the lib unit tests.
//!
//! Mount/unmount themselves need libjfs + a real object store + Postgres, so
//! they're exercised by the containerised e2e harness, not here.

#![cfg(feature = "mount")]

use std::fs;
use std::path::PathBuf;

use trove::config::{Config, ResolvedVolume};

/// Point trove's config dir at a throwaway tempdir for this process. Single
/// test in this binary, so the process-global env mutation can't race.
fn seed_config(tmp: &PathBuf) {
    std::env::set_var("XDG_CONFIG_HOME", tmp);
    // Clear any ambient creds so resolution is purely file-driven.
    for k in [
        "TROVE_VERSIONS_DB",
        "DATABASE_URL",
        "TROVE_R2_ENDPOINT",
        "R2_ACCESS_KEY_ID",
        "R2_SECRET_ACCESS_KEY",
    ] {
        std::env::remove_var(k);
    }
    let cfg = tmp.join("trove");
    fs::create_dir_all(cfg.join("volumes")).unwrap();

    // Shared default creds + an independent `work` profile (multi-account).
    fs::write(
        cfg.join("credentials.toml"),
        "versions_db = \"postgres://default-db\"\n\
         r2_endpoint = \"https://acctA.r2.cloudflarestorage.com\"\n\
         r2_access_key_id = \"akA\"\n\
         r2_secret_access_key = \"skA\"\n\
         \n\
         [profiles.work]\n\
         versions_db = \"postgres://work-db\"\n\
         r2_endpoint = \"https://acctB.r2.cloudflarestorage.com\"\n\
         r2_access_key_id = \"akB\"\n\
         r2_secret_access_key = \"skB\"\n",
    )
    .unwrap();

    // A fleet volume on the default creds...
    fs::write(
        cfg.join("volumes/notes.toml"),
        "bucket = \"https://acctA.r2.cloudflarestorage.com/trove-notes\"\n\
         schema = \"trove_notes\"\n\
         mountpoint = \"/home/u/notes\"\n\
         cache = \"/tmp/trove-cache\"\n",
    )
    .unwrap();
    // ...and one on the independent `work` account.
    fs::write(
        cfg.join("volumes/work.toml"),
        "bucket = \"https://acctB.r2.cloudflarestorage.com/trove-work\"\n\
         schema = \"trove_work\"\n\
         mountpoint = \"/home/u/work\"\n\
         cache = \"/tmp/trove-cache\"\n\
         credentials = \"work\"\n",
    )
    .unwrap();
}

#[test]
fn lists_resolves_and_detaches() {
    let tmp = std::env::temp_dir().join(format!("trove-lifecycle-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    seed_config(&tmp);

    // -- list_volumes returns both, sorted --
    let vols = Config::list_volumes();
    let names: Vec<&str> = vols.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["notes", "work"], "both volumes, sorted by name");

    // -- name-based resolution routes each volume to its own credentials --
    let notes = ResolvedVolume::load("notes").unwrap();
    assert_eq!(notes.creds.versions_db.as_deref(), Some("postgres://default-db"));
    assert_eq!(notes.volume.schema, "trove_notes");
    // meta URL carries the scheme + search_path fixups.
    assert!(notes
        .meta_url()
        .unwrap()
        .contains("search_path=trove_notes"));

    let work = ResolvedVolume::load("work").unwrap();
    assert_eq!(
        work.creds.versions_db.as_deref(),
        Some("postgres://work-db"),
        "work volume resolves the independent work profile, not the default"
    );
    assert_eq!(work.creds.r2_access_key_id.as_deref(), Some("akB"));

    // export_r2_env pushes the resolved keys where libjfs reads them.
    work.export_r2_env();
    assert_eq!(std::env::var("R2_ACCESS_KEY_ID").unwrap(), "akB");

    // -- an unmounted vault reads as not-mounted --
    assert!(!trove::commands::lifecycle::is_mounted("/home/u/work"));
    assert!(!trove::commands::lifecycle::is_mounted("/no/such/path"));

    // -- detach removes the local config but leaves the sibling intact --
    // (Unmount + agent removal are no-ops here: nothing is mounted and there's
    // no boot agent for this throwaway volume.)
    trove::commands::lifecycle::detach("work").unwrap();
    let after: Vec<String> = Config::list_volumes().into_iter().map(|(n, _)| n).collect();
    assert_eq!(after, vec!["notes".to_string()], "only `work` was detached");
    assert!(ResolvedVolume::load("work").is_err(), "work config is gone");

    let _ = fs::remove_dir_all(&tmp);
}
