//! Integration test for `trove backup` against a real version DB + a freshly
//! formatted JuiceFS volume (SQLite meta + local file store, same pattern as
//! `tests/versioning.rs`). Three revisions are recorded for one path; we then
//! mirror to a tmp directory and assert each rev's bytes land on disk; rerun
//! and assert nothing is rewritten; rerun with `dry_run` and assert the report
//! agrees but nothing was touched.
#![cfg(feature = "mount")]

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use trove::commands::backup::{self, BackupOptions, Layout};
use trove::jfs::Fs;
use trove::version::VersionStore;
use trove::versioning::record_version;

fn juicefs_bin() -> String {
    std::env::var("JUICEFS_BIN")
        .unwrap_or_else(|_| "/home/cp/code/trove/spike/juicefs/juicefs".to_string())
}

fn db_url() -> String {
    std::env::var("TROVE_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54322/postgres".to_string())
}

fn uniq() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn fresh_fs() -> Fs {
    let dir = std::env::temp_dir().join(format!("trove-backup-{}", uniq()));
    std::fs::create_dir_all(dir.join("store")).unwrap();
    let name = format!("vol{}", uniq().replace('-', ""));
    let meta = format!("sqlite3://{}/meta.db", dir.display());
    let out = Command::new(juicefs_bin())
        .args([
            "format",
            "--storage",
            "file",
            "--bucket",
            &format!("{}/store/", dir.display()),
            &meta,
            &name,
        ])
        .output()
        .expect("run juicefs format");
    assert!(
        out.status.success(),
        "format failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Fs::init(&name, &meta, &format!("{}/cache", dir.display())).unwrap()
}

#[test]
fn backup_mirrors_every_revision_then_no_ops_on_rerun() {
    let fs = fresh_fs();
    let Ok(mut versions) = VersionStore::connect(&db_url()) else {
        eprintln!("skipping: no DB reachable at {}", db_url());
        return;
    };

    // A unique path with three distinct revisions.
    let path = format!("/backup-{}.md", uniq());
    let v1 = format!("# v1 {}\nalpha\n", uniq());
    let v2 = format!("# v2 {}\nbeta\n", uniq());
    let v3 = format!("# v3 {}\ngamma\n", uniq());
    for body in [&v1, &v2, &v3] {
        fs.write_all(&path, body.as_bytes(), 0o644).unwrap();
        record_version(&fs, &mut versions, &path, body.as_bytes(), Some("tester")).unwrap();
    }

    // Mirror into a fresh tmp dir. The DB is shared with the suite, so
    // assertions key off our unique `path` rather than total counts.
    let dest = std::env::temp_dir().join(format!("trove-backup-out-{}", uniq()));
    let opts = BackupOptions {
        dest: dest.clone(),
        layout: Layout::ByPath,
        since: None,
        dry_run: false,
    };

    let report = backup::run(&fs, &mut versions, &opts).expect("backup run");
    // Other concurrent tests may have written paths too, so paths/written are
    // lower-bounded, not equal. What we CAN assert exactly: our three revs
    // are on disk with the right bytes.
    assert!(report.paths >= 1);
    assert!(report.revisions_written >= 3);

    let trimmed = path.trim_start_matches('/');
    let rev1 = dest.join(".versions").join(trimmed).join("rev-1");
    let rev2 = dest.join(".versions").join(trimmed).join("rev-2");
    let rev3 = dest.join(".versions").join(trimmed).join("rev-3");
    let live = dest.join(trimmed);
    assert_eq!(std::fs::read(&rev1).unwrap(), v1.as_bytes());
    assert_eq!(std::fs::read(&rev2).unwrap(), v2.as_bytes());
    assert_eq!(std::fs::read(&rev3).unwrap(), v3.as_bytes());
    assert_eq!(
        std::fs::read(&live).unwrap(),
        v3.as_bytes(),
        "live tree copy is the latest rev"
    );

    // Re-run: every revision should be skipped-unchanged.
    let again = backup::run(&fs, &mut versions, &opts).expect("backup rerun");
    assert_eq!(
        again.revisions_written, 0,
        "no bytes should be re-written on a clean rerun"
    );
    assert!(again.skipped_unchanged >= 3);

    // Dry-run: counts still come back, but nothing must change on disk.
    // Snapshot the mtimes of our three revs before, then assert they
    // didn't move after dry_run.
    let mtimes_before: Vec<_> = [&rev1, &rev2, &rev3]
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().modified().unwrap())
        .collect();
    let dry_opts = BackupOptions { dry_run: true, ..BackupOptions {
        dest: dest.clone(),
        layout: Layout::ByPath,
        since: None,
        dry_run: true,
    }};
    let dry = backup::run(&fs, &mut versions, &dry_opts).expect("backup dry-run");
    // Same path/rev counts as the non-dry rerun.
    assert!(dry.skipped_unchanged >= 3);
    let mtimes_after: Vec<_> = [&rev1, &rev2, &rev3]
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().modified().unwrap())
        .collect();
    assert_eq!(mtimes_before, mtimes_after, "dry-run must not touch disk");

    let _ = std::fs::remove_dir_all(&dest);
}

#[test]
fn backup_by_rev_layout_groups_full_trees_per_rev() {
    let fs = fresh_fs();
    let Ok(mut versions) = VersionStore::connect(&db_url()) else {
        eprintln!("skipping: no DB reachable at {}", db_url());
        return;
    };

    let path = format!("/byrev-{}.md", uniq());
    let v1 = format!("# v1 {}\n", uniq());
    let v2 = format!("# v2 {}\n", uniq());
    for body in [&v1, &v2] {
        fs.write_all(&path, body.as_bytes(), 0o644).unwrap();
        record_version(&fs, &mut versions, &path, body.as_bytes(), None).unwrap();
    }

    let dest = std::env::temp_dir().join(format!("trove-backup-byrev-{}", uniq()));
    let report = backup::run(
        &fs,
        &mut versions,
        &BackupOptions {
            dest: dest.clone(),
            layout: Layout::ByRev,
            since: None,
            dry_run: false,
        },
    )
    .expect("backup by-rev");
    assert!(report.revisions_written >= 2);

    let trimmed = path.trim_start_matches('/');
    assert_eq!(
        std::fs::read(dest.join("rev-1").join(trimmed)).unwrap(),
        v1.as_bytes()
    );
    assert_eq!(
        std::fs::read(dest.join("rev-2").join(trimmed)).unwrap(),
        v2.as_bytes()
    );
    // ByRev does NOT write a live-tree copy at <dest>/<path>.
    assert!(!dest.join(trimmed).exists());

    let _ = std::fs::remove_dir_all(&dest);
}
