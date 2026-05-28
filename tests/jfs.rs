//! e2e tests for the libjfs wrapper. Each test formats a real JuiceFS volume
//! (SQLite metadata + local-file object store — fast, isolated; R2 is proven
//! separately) and drives it through the safe wrapper. Requires the built
//! `libjfs`; run with `--features mount`. No `juicefs` binary needed — format
//! goes through the `jfs_format` FFI entry the same way `trove install` does.
#![cfg(feature = "mount")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use trove::jfs::{self, Fs};

/// A freshly formatted throwaway volume: unique dir, SQLite meta, file store.
struct TestVol {
    dir: PathBuf,
    name: String,
    meta: String,
}

impl TestVol {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let uniq = format!(
            "{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(format!("trove-jfs-{tag}-{uniq}"));
        std::fs::create_dir_all(dir.join("store")).unwrap();
        let name = format!("vol{}", uniq.replace('-', ""));
        let meta = format!("sqlite3://{}/meta.db", dir.display());

        let conf = serde_json::json!({
            "meta": meta,
            "name": name,
            "storage": "file",
            "bucket": format!("{}/store/", dir.display()),
        });
        jfs::format(&conf).expect("jfs::format (FFI) failed");
        TestVol { dir, name, meta }
    }

    fn open(&self) -> Fs {
        Fs::init(&self.name, &self.meta, &format!("{}/cache", self.dir.display())).unwrap()
    }
}

impl Drop for TestVol {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn init_opens_a_formatted_volume() {
    let v = TestVol::new("init");
    let _fs = v.open(); // panics if jfs_init fails
}

#[test]
fn jfs_format_ffi_creates_volume_that_can_be_opened() {
    // Targeted test for the `jfs_format` FFI entry (the one that replaced the
    // `juicefs` CLI shell-out in `trove install`). Asserts the round-trip:
    // format → open. If libjfs ever stops accepting our JSON shape, or the
    // newly-created volume can't be opened, this fails loudly.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let uniq = format!(
        "{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(format!("trove-ffi-format-{uniq}"));
    std::fs::create_dir_all(dir.join("store")).unwrap();
    let name = format!("ffi{}", uniq.replace('-', ""));
    let meta = format!("sqlite3://{}/meta.db", dir.display());

    let conf = serde_json::json!({
        "meta": meta,
        "name": name,
        "storage": "file",
        "bucket": format!("{}/store/", dir.display()),
    });
    jfs::format(&conf).expect("FFI format failed");

    // Verify the volume is openable with the safe wrapper.
    let fs = Fs::init(&name, &meta, &format!("{}/cache", dir.display()))
        .expect("Fs::init after FFI format failed");
    // And functional — minimal write/read round-trip.
    let f = fs.create("/probe.md", 0o644).unwrap();
    f.write_at(b"ffi format works", 0).unwrap();
    f.fsync().unwrap();
    drop(f);
    assert_eq!(fs.read_all("/probe.md").unwrap(), b"ffi format works");

    // Cleanup
    drop(fs);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_read_roundtrip_through_storage() {
    let v = TestVol::new("roundtrip");
    let fs = v.open();
    let payload = b"trove jfs wrapper: write -> storage -> read";

    let f = fs.create("/note.md", 0o644).unwrap();
    assert_eq!(f.write_at(payload, 0).unwrap(), payload.len());
    f.fsync().unwrap();
    drop(f); // close

    let f = fs.open("/note.md", 0).unwrap();
    let mut buf = vec![0u8; payload.len()];
    assert_eq!(f.read_at(&mut buf, 0).unwrap(), payload.len());
    assert_eq!(&buf, payload);
}

#[test]
fn mkdir_then_create_inside_it() {
    let v = TestVol::new("mkdir");
    let fs = v.open();
    fs.mkdir("/people", 0o755).unwrap();
    let f = fs.create("/people/rebekah.md", 0o644).unwrap();
    f.write_at(b"---\ntype: person\n---\n", 0).unwrap();
    drop(f);
    let st = fs.stat("/people").unwrap();
    assert!(st.is_dir(), "/people should be a directory");
}

#[test]
fn stat_reports_length() {
    let v = TestVol::new("stat");
    let fs = v.open();
    let body = b"0123456789";
    let f = fs.create("/len.txt", 0o644).unwrap();
    f.write_at(body, 0).unwrap();
    f.fsync().unwrap();
    drop(f);
    let st = fs.stat("/len.txt").unwrap();
    assert_eq!(st.length, body.len() as u64);
    assert!(!st.is_dir());
}

#[test]
fn unlink_removes_a_file() {
    let v = TestVol::new("unlink");
    let fs = v.open();
    let f = fs.create("/tmp.txt", 0o644).unwrap();
    f.write_at(b"bye", 0).unwrap();
    drop(f);
    fs.stat("/tmp.txt").unwrap(); // exists
    fs.unlink("/tmp.txt").unwrap();
    assert!(fs.stat("/tmp.txt").is_err(), "stat should fail after unlink");
}

#[test]
fn open_missing_file_errors() {
    let v = TestVol::new("missing");
    let fs = v.open();
    assert!(fs.open("/does-not-exist.md", 0).is_err());
}

#[test]
fn rename_moves_a_file() {
    let v = TestVol::new("rename");
    let fs = v.open();
    let f = fs.create("/from.md", 0o644).unwrap();
    f.write_at(b"movable", 0).unwrap();
    f.fsync().unwrap();
    drop(f);
    fs.rename("/from.md", "/to.md").unwrap();
    assert!(fs.stat("/from.md").is_err(), "source gone after rename");
    assert_eq!(fs.stat("/to.md").unwrap().length, 7);
}

#[test]
fn clone_is_a_copy_on_write_snapshot() {
    let v = TestVol::new("clone");
    let fs = v.open();
    fs.mkdir("/.trove", 0o755).unwrap();

    // Original content, then clone it.
    let f = fs.create("/live.md", 0o644).unwrap();
    f.write_at(b"version one", 0).unwrap();
    f.fsync().unwrap();
    drop(f);
    fs.clone_file("/live.md", "/.trove/snap1", true).unwrap();

    // Overwrite the original — the clone must keep the OLD bytes (COW snapshot).
    fs.write_all("/live.md", b"version two!", 0o644).unwrap();

    assert_eq!(fs.read_all("/.trove/snap1").unwrap(), b"version one");
    assert_eq!(fs.read_all("/live.md").unwrap(), b"version two!");
}

#[test]
fn chmod_truncate_symlink_statvfs() {
    let v = TestVol::new("posix");
    let fs = v.open();
    let f = fs.create("/x.md", 0o644).unwrap();
    f.write_at(b"hello world", 0).unwrap();
    f.fsync().unwrap();
    drop(f);

    fs.chmod("/x.md", 0o600).unwrap();
    assert_eq!(fs.stat("/x.md").unwrap().mode & 0o777, 0o600);

    fs.truncate("/x.md", 5).unwrap();
    assert_eq!(fs.stat("/x.md").unwrap().length, 5);

    fs.symlink("/x.md", "/lnk").unwrap();
    // JuiceFS normalises the stored target (drops the leading slash).
    assert!(fs.readlink("/lnk").unwrap().ends_with("x.md"));

    let (total, avail) = fs.statvfs().unwrap();
    assert!(total > 0, "statvfs total should be positive, got {total}");
    let _ = avail;
}

// --- Concurrency spike (step 5) -------------------------------------------
//
// The `mount` layer currently serialises every libjfs call behind one global
// `Mutex`, holding it across the whole commit (validate + write). Before
// versioning piles more work onto that same barrier, we need to know whether
// the lock is load-bearing or can drop to per-inode. libjfs is the same C ABI
// the Java/Python SDKs drive concurrently, so it *should* be safe under
// parallel callers — these tests prove it against the real volume and lock in
// the property `mount` will rely on: parallel ops on DISTINCT files are safe
// and data-correct from many OS threads sharing one `Fs` handle.

use std::sync::Arc;
use std::thread;

#[test]
fn concurrent_distinct_files_are_safe_and_correct() {
    let v = TestVol::new("conc-distinct");
    let fs = Arc::new(v.open());

    // Many threads, each hammering its own files for several iterations. If
    // libjfs were unsafe under parallel callers this crashes (process abort)
    // or returns corrupt/short data; if data is correct on every round-trip,
    // distinct-file parallelism is safe.
    const THREADS: usize = 16;
    const ITERS: usize = 25;

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let fs = Arc::clone(&fs);
            thread::spawn(move || {
                for i in 0..ITERS {
                    let path = format!("/t{t}-{i}.md");
                    // Distinct payload per (thread, iter) so a cross-file leak
                    // or torn write would show as a mismatch.
                    let payload = format!("thread {t} iter {i} {}", "x".repeat(t * 7 + i));
                    let bytes = payload.as_bytes();

                    let f = fs.create(&path, 0o644).unwrap();
                    let mut off = 0i64;
                    while (off as usize) < bytes.len() {
                        off += f.write_at(&bytes[off as usize..], off).unwrap() as i64;
                    }
                    f.fsync().unwrap();
                    drop(f);

                    let got = fs.read_all(&path).unwrap();
                    assert_eq!(got, bytes, "round-trip mismatch on {path}");
                    fs.unlink(&path).unwrap();
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("worker thread panicked — libjfs not safe under parallel callers");
    }
}

#[test]
fn concurrent_readers_of_one_file_see_consistent_data() {
    let v = TestVol::new("conc-readers");
    let fs = Arc::new(v.open());

    let payload = b"the same bytes seen by every concurrent reader".to_vec();
    let f = fs.create("/shared.md", 0o644).unwrap();
    f.write_at(&payload, 0).unwrap();
    f.fsync().unwrap();
    drop(f);

    // Many threads reading the same committed file in parallel must each see
    // the full, identical contents (read-only sharing — what `mount` does for
    // ungoverned/read opens with no lock).
    let handles: Vec<_> = (0..24)
        .map(|_| {
            let fs = Arc::clone(&fs);
            let expect = payload.clone();
            thread::spawn(move || {
                for _ in 0..50 {
                    assert_eq!(fs.read_all("/shared.md").unwrap(), expect);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("reader thread panicked");
    }
}

#[test]
fn same_file_writers_need_per_inode_serialisation_then_last_writer_wins() {
    // SPIKE FINDING (pinned here): `Fs::write_all` is NOT internally safe for
    // concurrent writers to the SAME path. Its unlink-then-create sequence
    // races — two unlocked writers interleave as unlink/unlink/create/create
    // and the second `create` fails with EEXIST (errno 17). The `mount`
    // layer's global `Mutex` currently masks this; once that lock relaxes to
    // per-inode (justified by `concurrent_distinct_files_*`), the per-inode
    // lock STILL serialises same-path writers, so the race cannot occur and
    // the commit barrier remains correct. This test models that per-inode lock
    // and asserts the property mount then guarantees: clean last-writer-wins,
    // never a torn interleave.
    let v = TestVol::new("conc-samefile");
    let fs = Arc::new(v.open());
    // Stands in for mount's per-inode lock: one lock per path.
    let inode_lock = Arc::new(std::sync::Mutex::new(()));

    const THREADS: usize = 8;
    // Equal-length payloads so a torn write would still be utf-8 but fail the
    // exact-membership check below.
    let payloads: Vec<String> = (0..THREADS).map(|t| format!("writer-{t:03}-payload-body")).collect();

    let handles: Vec<_> = payloads
        .iter()
        .cloned()
        .map(|p| {
            let fs = Arc::clone(&fs);
            let lock = Arc::clone(&inode_lock);
            thread::spawn(move || {
                for _ in 0..30 {
                    let _g = lock.lock().unwrap(); // per-inode serialisation
                    fs.write_all("/contended.md", p.as_bytes(), 0o644).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("writer thread panicked under per-inode serialisation");
    }

    // Whatever survived must be exactly one writer's payload — not a splice.
    let final_bytes = fs.read_all("/contended.md").unwrap();
    let final_str = String::from_utf8(final_bytes).expect("committed file is valid utf-8");
    assert!(
        payloads.contains(&final_str),
        "committed file is a torn interleave, not a single writer's payload: {final_str:?}"
    );
}

#[test]
fn readdir_lists_entries_with_types() {
    let v = TestVol::new("readdir");
    let fs = v.open();
    fs.mkdir("/people", 0o755).unwrap();
    let f = fs.create("/a.md", 0o644).unwrap();
    f.write_at(b"x", 0).unwrap();
    drop(f);

    let mut names: Vec<(String, bool)> = fs
        .readdir("/")
        .unwrap()
        .into_iter()
        .map(|d| (d.name.clone(), d.is_dir()))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![("a.md".to_string(), false), ("people".to_string(), true)]
    );
}

#[test]
fn write_all_preserves_mode_on_existing_file() {
    // The validation gate (and `trove restore`) use `fs.write_all` to commit
    // the bytes. An edit must preserve a `chmod +x` — and any other mode bits.
    // Regression test for the 0o644-hardcoded-on-every-write bug.
    let v = TestVol::new("mode-pres");
    let fs = v.open();

    // Create a 0o755 executable file, then write_all over it with mode 0o644.
    // Mode must remain 0o755 because write_all should truncate in place.
    let f = fs.create("/hook.sh", 0o755).unwrap();
    f.write_at(b"#!/bin/sh\necho v1\n", 0).unwrap();
    drop(f);
    assert_eq!(fs.lstat("/hook.sh").unwrap().mode & 0o777, 0o755);

    fs.write_all("/hook.sh", b"#!/bin/sh\necho v2\n", 0o644).unwrap();
    assert_eq!(
        fs.lstat("/hook.sh").unwrap().mode & 0o777,
        0o755,
        "edit through write_all must NOT reset mode"
    );
    assert_eq!(fs.read_all("/hook.sh").unwrap(), b"#!/bin/sh\necho v2\n");
}

#[test]
fn write_all_uses_passed_mode_for_fresh_file() {
    // The other side of the contract: on a fresh file, the mode argument IS
    // honoured (because there is no existing inode to preserve).
    let v = TestVol::new("mode-fresh");
    let fs = v.open();
    fs.write_all("/new.md", b"hi\n", 0o600).unwrap();
    assert_eq!(fs.lstat("/new.md").unwrap().mode & 0o777, 0o600);
}

#[test]
fn xattrs_round_trip() {
    // Set / get / list / remove an extended attribute end-to-end through
    // libjfs. Pure POSIX-compat — no Trove substrate involvement.
    let v = TestVol::new("xattr");
    let fs = v.open();
    let f = fs.create("/note.md", 0o644).unwrap();
    f.write_at(b"hello\n", 0).unwrap();
    drop(f);

    let key = "user.trove.example";
    let val = b"the answer is 42";

    // Initially: no attribute.
    assert_eq!(fs.get_xattr("/note.md", key).unwrap(), None);
    assert!(!fs.list_xattr("/note.md").unwrap().contains(&key.to_string()));

    // Set, then read it back.
    fs.set_xattr("/note.md", key, val, trove::jfs::XATTR_CREATE_OR_REPLACE).unwrap();
    let got = fs.get_xattr("/note.md", key).unwrap();
    assert_eq!(got.as_deref(), Some(&val[..]));
    assert!(fs.list_xattr("/note.md").unwrap().contains(&key.to_string()));

    // Replace it.
    fs.set_xattr("/note.md", key, b"updated", trove::jfs::XATTR_REPLACE).unwrap();
    assert_eq!(
        fs.get_xattr("/note.md", key).unwrap().as_deref(),
        Some(&b"updated"[..])
    );

    // Remove.
    assert!(fs.remove_xattr("/note.md", key).unwrap());
    assert_eq!(fs.get_xattr("/note.md", key).unwrap(), None);
    // Removing again returns false, not error.
    assert!(!fs.remove_xattr("/note.md", key).unwrap());
}

// ---------------------------------------------------------------------------
// POSIX advisory locks: jfs_flock / jfs_setlk / jfs_getlk pass-through.
//
// libjfs/JuiceFS owns lock state at the Meta layer; these tests prove the FFI
// wrapper round-trips conflict detection and unlock correctly. Lock-type
// constants are fcntl values (F_RDLCK=0, F_WRLCK=1, F_UNLCK=2) at this layer,
// matching the upstream Setlk/Flock API. EAGAIN = 11 on Linux.

const EAGAIN: i32 = 11;

#[test]
fn flock_exclusive_blocks_second_holder() {
    let v = TestVol::new("flock-ex");
    let fs = v.open();
    fs.create("/lock.md", 0o644).unwrap().fsync().unwrap();

    let a = fs.open("/lock.md", 0).unwrap();
    let b = fs.open("/lock.md", 0).unwrap();

    a.flock(1, jfs::F_WRLCK).expect("first writer takes exclusive lock");
    let err = b.flock(2, jfs::F_WRLCK).expect_err("second exclusive must conflict");
    assert_eq!(err.0, EAGAIN, "expected EAGAIN, got errno {}", err.0);

    // After unlock, the second holder can take it.
    a.flock(1, jfs::F_UNLCK).expect("unlock");
    b.flock(2, jfs::F_WRLCK).expect("second now succeeds after first releases");
}

#[test]
fn flock_shared_locks_coexist_but_block_exclusive() {
    let v = TestVol::new("flock-sh");
    let fs = v.open();
    fs.create("/r.md", 0o644).unwrap().fsync().unwrap();

    let a = fs.open("/r.md", 0).unwrap();
    let b = fs.open("/r.md", 0).unwrap();
    let c = fs.open("/r.md", 0).unwrap();

    a.flock(1, jfs::F_RDLCK).expect("a takes shared");
    b.flock(2, jfs::F_RDLCK).expect("b takes shared concurrently");

    let err = c.flock(3, jfs::F_WRLCK).expect_err("exclusive vs shared must conflict");
    assert_eq!(err.0, EAGAIN);

    a.flock(1, jfs::F_UNLCK).unwrap();
    b.flock(2, jfs::F_UNLCK).unwrap();
    c.flock(3, jfs::F_WRLCK).expect("after all shared released, exclusive succeeds");
}

#[test]
fn setlk_byte_range_disjoint_ranges_dont_conflict() {
    let v = TestVol::new("setlk-disjoint");
    let fs = v.open();
    fs.create("/br.md", 0o644).unwrap().fsync().unwrap();

    let a = fs.open("/br.md", 0).unwrap();
    let b = fs.open("/br.md", 0).unwrap();

    // Two exclusive byte-range locks on non-overlapping ranges should both succeed.
    a.setlk(1, jfs::F_WRLCK, 0, 100, false, 1234)
        .expect("range [0,100] for owner 1");
    b.setlk(2, jfs::F_WRLCK, 200, 300, false, 5678)
        .expect("range [200,300] for owner 2");
}

#[test]
fn setlk_byte_range_overlapping_ranges_conflict() {
    let v = TestVol::new("setlk-overlap");
    let fs = v.open();
    fs.create("/br2.md", 0o644).unwrap().fsync().unwrap();

    let a = fs.open("/br2.md", 0).unwrap();
    let b = fs.open("/br2.md", 0).unwrap();

    a.setlk(1, jfs::F_WRLCK, 0, 100, false, 1234).expect("a locks [0,100]");
    let err = b
        .setlk(2, jfs::F_WRLCK, 50, 150, false, 5678)
        .expect_err("b's [50,150] overlaps — must EAGAIN");
    assert_eq!(err.0, EAGAIN);

    // Release a's range; b can then take its (overlapping) range.
    a.setlk(1, jfs::F_UNLCK, 0, 100, false, 1234).unwrap();
    b.setlk(2, jfs::F_WRLCK, 50, 150, false, 5678)
        .expect("after a unlocks, b takes its range");
}

#[test]
fn getlk_reports_no_conflict_on_clean_file() {
    let v = TestVol::new("getlk-clean");
    let fs = v.open();
    fs.create("/g.md", 0o644).unwrap().fsync().unwrap();

    let f = fs.open("/g.md", 0).unwrap();
    let info = f
        .getlk(1, jfs::F_WRLCK, 0, u64::MAX)
        .expect("getlk on a clean file returns Ok");
    assert_eq!(
        info.typ,
        jfs::F_UNLCK,
        "no conflict — typ should be F_UNLCK; got {:?}",
        info
    );
}

#[test]
fn getlk_reports_conflicting_holder() {
    let v = TestVol::new("getlk-conflict");
    let fs = v.open();
    fs.create("/gc.md", 0o644).unwrap().fsync().unwrap();

    let a = fs.open("/gc.md", 0).unwrap();
    let b = fs.open("/gc.md", 0).unwrap();

    // owner 1 holds an exclusive range; pid 99 recorded on it.
    a.setlk(1, jfs::F_WRLCK, 0, 100, false, 99).unwrap();

    // owner 2 asks: would my exclusive lock on the whole file succeed?
    let info = b.getlk(2, jfs::F_WRLCK, 0, u64::MAX).expect("getlk ok");
    assert_ne!(info.typ, jfs::F_UNLCK, "expected conflict, got {:?}", info);
    assert_eq!(info.pid, 99, "conflicting holder pid not reported");
}


