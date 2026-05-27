//! e2e for the versioning module: COW clone + chain + historical read, against
//! a real JuiceFS volume + the version DB. Run with `--features mount` and
//! `source ~/.secret_env` (libjfs/juicefs + the supabase stack). This drives
//! `record_version`/`cat` directly (independent of the mount's governed-vs-
//! pass-through routing), proving history accumulates and every revision's exact
//! bytes are recoverable.
#![cfg(feature = "mount")]

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use trove::jfs::Fs;
use trove::version::VersionStore;
use trove::versioning::{cat, record_version};

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
    let dir = std::env::temp_dir().join(format!("trove-ver-{}", uniq()));
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
fn history_accumulates_and_every_revision_is_recoverable() {
    let fs = fresh_fs();
    let mut vs = VersionStore::connect(&db_url()).expect("version DB up? (`supabase start`)");
    let path = format!("/hist-{}.md", uniq());

    // Three edits. Each mirrors what commit() does: write the live file, then
    // record the version (which COW-clones the just-written content).
    let v1 = format!("# one {}\nalpha\n", uniq());
    let v2 = format!("# two {}\nbeta\n", uniq());
    let v3 = format!("# three {}\ngamma\n", uniq());
    for v in [&v1, &v2, &v3] {
        fs.write_all(&path, v.as_bytes(), 0o644).unwrap();
        record_version(&fs, &mut vs, &path, v.as_bytes(), Some("tester")).unwrap();
    }

    // The chain is a monotonic, parent-linked history, newest first.
    let log = vs.log(&path).unwrap();
    assert_eq!(log.iter().map(|x| x.rev).collect::<Vec<_>>(), vec![3, 2, 1]);
    assert_eq!(
        log.iter().map(|x| x.parent_rev).collect::<Vec<_>>(),
        vec![Some(2), Some(1), None]
    );
    assert_eq!(log[0].author.as_deref(), Some("tester"));

    // Every revision's exact bytes come back via the COW clone — overwriting the
    // live file did NOT clobber older versions (this is the restore data path).
    assert_eq!(cat(&fs, &mut vs, &path, 1).unwrap().as_deref(), Some(v1.as_bytes()));
    assert_eq!(cat(&fs, &mut vs, &path, 2).unwrap().as_deref(), Some(v2.as_bytes()));
    assert_eq!(cat(&fs, &mut vs, &path, 3).unwrap().as_deref(), Some(v3.as_bytes()));
    assert_eq!(cat(&fs, &mut vs, &path, 99).unwrap(), None);
}

#[test]
fn identical_content_dedups_one_clone_across_revisions() {
    let fs = fresh_fs();
    let mut vs = VersionStore::connect(&db_url()).unwrap();
    let path = format!("/dedup-{}.md", uniq());
    let same = format!("# unchanged {}\nbody\n", uniq());

    // Write the same content twice — two revs, one content hash, one clone.
    fs.write_all(&path, same.as_bytes(), 0o644).unwrap();
    let r1 = record_version(&fs, &mut vs, &path, same.as_bytes(), None).unwrap();
    let r2 = record_version(&fs, &mut vs, &path, same.as_bytes(), None).unwrap();
    assert_eq!((r1, r2), (1, 2));

    let log = vs.log(&path).unwrap();
    assert_eq!(log[0].blob_hash, log[1].blob_hash, "same content => same blob hash");
    // Both revisions read back the identical bytes from the one shared clone.
    assert_eq!(cat(&fs, &mut vs, &path, 1).unwrap(), cat(&fs, &mut vs, &path, 2).unwrap());
}
