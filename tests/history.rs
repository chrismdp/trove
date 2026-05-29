//! e2e for the version-history commands (`log`/`cat`/`diff`/`restore`). Drives
//! the `commands::history` functions directly against a real JuiceFS volume +
//! the version DB. Run with `--features mount` and `source ~/.secret_env`.
#![cfg(feature = "mount")]

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use trove::commands::history;
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
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    )
}
fn fresh_fs() -> Fs {
    let dir = std::env::temp_dir().join(format!("trove-hist-{}", uniq()));
    std::fs::create_dir_all(dir.join("store")).unwrap();
    let name = format!("vol{}", uniq().replace('-', ""));
    let meta = format!("sqlite3://{}/meta.db", dir.display());
    let out = Command::new(juicefs_bin())
        .args(["format", "--storage", "file", "--bucket", &format!("{}/store/", dir.display()), &meta, &name])
        .output()
        .expect("run juicefs format");
    assert!(out.status.success(), "format failed: {}", String::from_utf8_lossy(&out.stderr));
    Fs::init(&name, &meta, &format!("{}/cache", dir.display())).unwrap()
}

#[test]
fn log_cat_diff_restore_round_trip() {
    let fs = fresh_fs();
    let mut vs = VersionStore::connect(&db_url(), None).expect("version DB up? (`supabase start`)");
    let path = format!("/hist-{}.md", uniq());

    // Three edits, each written live + versioned (what the mount's commit does).
    let v1 = "# Doc\nalpha\n".to_string();
    let v2 = "# Doc\nbeta\n".to_string();
    let v3 = "# Doc\ngamma\n".to_string();
    for v in [&v1, &v2, &v3] {
        fs.write_all(&path, v.as_bytes(), 0o644).unwrap();
        record_version(&fs, &mut vs, &path, v.as_bytes(), Some("tester")).unwrap();
    }

    // log: newest first, 3 entries.
    let entries = history::log(&mut vs, &path).unwrap();
    assert_eq!(entries.iter().map(|e| e.rev).collect::<Vec<_>>(), vec![3, 2, 1]);

    // cat: each revision's exact bytes.
    assert_eq!(history::cat(&fs, &mut vs, &path, 1).unwrap(), v1.as_bytes());
    assert_eq!(history::cat(&fs, &mut vs, &path, 3).unwrap(), v3.as_bytes());
    assert!(history::cat(&fs, &mut vs, &path, 99).is_err(), "unknown rev errors");

    // diff: rev1 -> rev3 drops "alpha", adds "gamma"; "# Doc" unchanged.
    let d = history::diff(&fs, &mut vs, &path, 1, 3).unwrap();
    assert!(d.contains("-alpha"), "diff shows the removed line:\n{d}");
    assert!(d.contains("+gamma"), "diff shows the added line:\n{d}");
    assert!(d.contains(" # Doc"), "diff shows the unchanged line:\n{d}");

    // restore: bring back rev1 as a NEW rev (4), head content == v1.
    let new_rev = history::restore(&fs, &mut vs, &path, 1).unwrap();
    assert_eq!(new_rev, 4, "restore appends a new revision");
    assert_eq!(fs.read_all(&path).unwrap(), v1.as_bytes(), "live file is back to v1");
    assert_eq!(history::cat(&fs, &mut vs, &path, 4).unwrap(), v1.as_bytes());
    // The chain now has 4 entries, and rev4 reuses rev1's blob (content dedup).
    let after = history::log(&mut vs, &path).unwrap();
    assert_eq!(after.len(), 4);
    assert_eq!(after[0].blob_hash, after[3].blob_hash, "restored rev shares rev1's blob");
}
