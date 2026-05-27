//! e2e tests for the best-effort recorder: WAL -> R2 (bytes) + Postgres
//! (metadata). Needs the local Supabase stack and real R2 creds — run with
//! `--features mount` and `source ~/.secret_env`. Unique paths + WAL temp dirs
//! isolate runs against the shared backends.
#![cfg(feature = "mount")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use trove::blobstore::BlobStore;
use trove::recorder::{DrainStats, Recorder};
use trove::version::VersionStore;

fn db_url() -> String {
    std::env::var("TROVE_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54322/postgres".to_string())
}

fn unique(tag: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{tag}-{}-{n}-{nanos}", std::process::id())
}

fn wal_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("trove-wal-{}", unique("d")));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn recorder(wal: PathBuf) -> Recorder {
    let vs = VersionStore::connect(&db_url()).expect("version DB up? (`supabase start`)");
    let bs = BlobStore::from_env().expect("R2 creds in env");
    Recorder::new(vs, bs, wal).unwrap()
}

/// A fresh connection for assertions (the recorder owns its own).
fn checker() -> VersionStore {
    VersionStore::connect(&db_url()).unwrap()
}

#[test]
fn record_only_touches_the_wal_then_drain_applies_to_pg_and_r2() {
    let rec = recorder(wal_dir());
    let path = format!("/test/{}.md", unique("rec"));
    let content = b"---\ntype: note\n---\nrecorded via the WAL";

    rec.record(&path, content, Some("agent-x")).unwrap();

    // Nothing in Postgres yet — record() is WAL-only.
    assert!(
        checker().log(&path).unwrap().is_empty(),
        "record() must not write to Postgres directly"
    );

    // Drain applies it: metadata to PG, bytes to R2.
    assert_eq!(rec.drain_once().unwrap(), DrainStats { applied: 1, remaining: 0 });

    let log = checker().log(&path).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].rev, 1);
    assert_eq!(log[0].author.as_deref(), Some("agent-x"));
    assert_eq!(rec.cat(&path, 1).unwrap().as_deref(), Some(&content[..]));

    // Idempotent: nothing left to drain.
    assert_eq!(rec.drain_once().unwrap(), DrainStats::default());
}

#[test]
fn drain_preserves_per_path_revision_order() {
    let rec = recorder(wal_dir());
    let path = format!("/test/{}.md", unique("order"));

    rec.record(&path, b"first", None).unwrap();
    rec.record(&path, b"second", None).unwrap();
    rec.record(&path, b"third", None).unwrap();

    let stats = rec.drain_once().unwrap();
    assert_eq!(stats.applied, 3);

    // Newest first; revs assigned in WAL (FIFO) order.
    let log = checker().log(&path).unwrap();
    assert_eq!(log.iter().map(|v| v.rev).collect::<Vec<_>>(), vec![3, 2, 1]);
    assert_eq!(rec.cat(&path, 1).unwrap().as_deref(), Some(&b"first"[..]));
    assert_eq!(rec.cat(&path, 3).unwrap().as_deref(), Some(&b"third"[..]));
}

#[test]
fn wal_survives_a_crash_and_drains_on_restart() {
    let dir = wal_dir();
    let path = format!("/test/{}.md", unique("crash"));

    // First "process" records, then dies before draining.
    {
        let rec = recorder(dir.clone());
        rec.record(&path, b"survived the crash", None).unwrap();
    } // recorder dropped — nothing drained

    assert!(checker().log(&path).unwrap().is_empty(), "not yet applied");

    // A new process on the same WAL dir picks it up.
    let rec2 = recorder(dir);
    assert_eq!(rec2.drain_once().unwrap().applied, 1);
    assert_eq!(rec2.cat(&path, 1).unwrap().as_deref(), Some(&b"survived the crash"[..]));
}
