//! e2e test for `trove mount`: mount a real JuiceFS-backed Trove filesystem and
//! drive it with ordinary std::fs syscalls *through the kernel* — proving the
//! whole stack (kernel → fuser → libjfs → JuiceFS storage). Requires
//! `--features mount`, the built libjfs, the `juicefs` binary, and /dev/fuse.
#![cfg(feature = "mount")]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use trove::jfs::Fs;
use trove::mount;
use trove::types::Registry;

fn juicefs_bin() -> String {
    std::env::var("JUICEFS_BIN")
        .unwrap_or_else(|_| "/home/cp/code/trove/spike/juicefs/juicefs".to_string())
}

fn uniq(tag: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{tag}-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// Format a throwaway volume and return an opened Fs + its scratch dir.
fn fresh_fs(tag: &str) -> (Fs, PathBuf) {
    let u = uniq(tag);
    let dir = std::env::temp_dir().join(format!("trove-mnt-{u}"));
    std::fs::create_dir_all(dir.join("store")).unwrap();
    let name = format!("vol{}", u.replace('-', ""));
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
    assert!(out.status.success(), "format failed: {}", String::from_utf8_lossy(&out.stderr));
    let fs = Fs::init(&name, &meta, &format!("{}/cache", dir.display())).unwrap();
    (fs, dir)
}

/// Wait until the mountpoint is serving (first op succeeds).
fn wait_mounted(mountpoint: &PathBuf) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::fs::metadata(mountpoint).is_ok() && std::fs::read_dir(mountpoint).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("mount did not become ready");
}

#[test]
fn write_and_read_a_file_through_the_kernel() {
    let (fs, dir) = fresh_fs("rw");
    let mountpoint = dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).unwrap();

    let session = mount::spawn(fs, Registry::empty(), &mountpoint).expect("mount");
    wait_mounted(&mountpoint);

    let file = mountpoint.join("note.md");
    let body = "---\ntype: person\n---\nhello through FUSE\n";
    std::fs::write(&file, body).expect("write via kernel");

    let back = std::fs::read_to_string(&file).expect("read via kernel");
    assert_eq!(back, body);

    let meta = std::fs::metadata(&file).unwrap();
    assert_eq!(meta.len(), body.len() as u64);
    assert!(meta.is_file());

    drop(session); // unmounts
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mkdir_and_nested_file_through_the_kernel() {
    let (fs, dir) = fresh_fs("mkdir");
    let mountpoint = dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).unwrap();

    let session = mount::spawn(fs, Registry::empty(), &mountpoint).expect("mount");
    wait_mounted(&mountpoint);

    std::fs::create_dir(mountpoint.join("people")).expect("mkdir via kernel");
    assert!(std::fs::metadata(mountpoint.join("people")).unwrap().is_dir());

    let nested = mountpoint.join("people/rebekah.md");
    std::fs::write(&nested, "---\ntype: person\n---\n").expect("nested write");
    assert_eq!(
        std::fs::read_to_string(&nested).unwrap(),
        "---\ntype: person\n---\n"
    );

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unlink_through_the_kernel() {
    let (fs, dir) = fresh_fs("unlink");
    let mountpoint = dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).unwrap();

    let session = mount::spawn(fs, Registry::empty(), &mountpoint).expect("mount");
    wait_mounted(&mountpoint);

    let f = mountpoint.join("tmp.md");
    std::fs::write(&f, "bye").unwrap();
    assert!(f.exists());
    std::fs::remove_file(&f).expect("unlink via kernel");
    assert!(!f.exists());

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn readdir_lists_real_entries() {
    let (fs, dir) = fresh_fs("ls");
    let mountpoint = dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).unwrap();
    let session = mount::spawn(fs, Registry::empty(), &mountpoint).expect("mount");
    wait_mounted(&mountpoint);

    std::fs::write(mountpoint.join("a.md"), "alpha").unwrap();
    std::fs::write(mountpoint.join("b.md"), "bravo").unwrap();
    std::fs::create_dir(mountpoint.join("sub")).unwrap();
    std::fs::write(mountpoint.join("sub/c.md"), "charlie").unwrap();

    let mut names: Vec<String> = std::fs::read_dir(&mountpoint)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["a.md", "b.md", "sub"]);

    // The directory entry is reported as a directory (file_type comes from readdir).
    let sub_entry = std::fs::read_dir(&mountpoint)
        .unwrap()
        .map(|e| e.unwrap())
        .find(|e| e.file_name() == "sub")
        .unwrap();
    assert!(sub_entry.file_type().unwrap().is_dir());

    // Nested listing works.
    let nested: Vec<String> = std::fs::read_dir(mountpoint.join("sub"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(nested, vec!["c.md"]);

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rename_moves_a_file() {
    let (fs, dir) = fresh_fs("mv");
    let mountpoint = dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).unwrap();
    let session = mount::spawn(fs, Registry::empty(), &mountpoint).expect("mount");
    wait_mounted(&mountpoint);

    let src = mountpoint.join("from.md");
    let dst = mountpoint.join("to.md");
    std::fs::write(&src, "movable").unwrap();
    std::fs::rename(&src, &dst).expect("rename via kernel");

    assert!(!src.exists());
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), "movable");

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Atomic-save-via-rename must not smuggle invalid content past the gate: a
/// rename whose destination is governed validates the *moved bytes* first.
#[test]
fn rename_onto_governed_path_is_gated() {
    let (fs, dir) = fresh_fs("mvgate");
    let schema_root = dir.join("schemas");
    std::fs::create_dir_all(schema_root.join(".types")).unwrap();
    std::fs::write(schema_root.join(".types/person.json"), PERSON_SCHEMA).unwrap();
    let registry = Registry::load(&schema_root).unwrap();

    let mountpoint = dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).unwrap();
    let session = mount::spawn(fs, registry, &mountpoint).expect("mount");
    wait_mounted(&mountpoint);
    std::fs::create_dir(mountpoint.join("people")).unwrap();

    let dst = mountpoint.join("people/p.md");

    // A temp file at an ungoverned name (no validation on write) holding BAD
    // content — the shape an editor's atomic save produces.
    let tmp_bad = mountpoint.join("people/.tmp-bad");
    std::fs::write(&tmp_bad, "---\ntype: person\nage: nope\n---\n").unwrap();
    assert!(
        std::fs::rename(&tmp_bad, &dst).is_err(),
        "renaming invalid content onto a governed path must be rejected"
    );
    assert!(!dst.exists(), "rejected rename must not create the destination");
    assert!(std::fs::read_to_string(mountpoint.join("people/p.md.errors"))
        .unwrap()
        .contains("age"));
    assert!(tmp_bad.exists(), "the source survives a rejected rename");

    // Valid content renames through cleanly and clears the stale sidecar.
    let tmp_good = mountpoint.join("people/.tmp-good");
    let body = "---\ntype: person\nage: 41\n---\n";
    std::fs::write(&tmp_good, body).unwrap();
    std::fs::rename(&tmp_good, &dst).expect("valid content should rename onto a governed path");
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), body);
    assert!(!mountpoint.join("people/p.md.errors").exists());

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn posix_passthrough_chmod_truncate_symlink_rmdir() {
    use std::os::unix::fs::PermissionsExt;
    let (fs, dir) = fresh_fs("posix");
    let mountpoint = dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).unwrap();
    let session = mount::spawn(fs, Registry::empty(), &mountpoint).expect("mount");
    wait_mounted(&mountpoint);

    // chmod
    let f = mountpoint.join("f.md");
    std::fs::write(&f, "data").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    assert_eq!(
        std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // truncate (ungoverned → straight through)
    let fh = std::fs::OpenOptions::new().write(true).open(&f).unwrap();
    fh.set_len(2).expect("truncate");
    drop(fh);
    assert_eq!(std::fs::read(&f).unwrap(), b"da");

    // rmdir
    std::fs::create_dir(mountpoint.join("d")).unwrap();
    std::fs::remove_dir(mountpoint.join("d")).expect("rmdir");
    assert!(!mountpoint.join("d").exists());

    // symlink + readlink (lstat reports S_IFLNK; the kernel resolves the link)
    std::os::unix::fs::symlink("f.md", mountpoint.join("link")).expect("symlink");
    let lmeta = std::fs::symlink_metadata(mountpoint.join("link")).unwrap();
    assert!(lmeta.file_type().is_symlink(), "entry must report as a symlink");
    assert_eq!(
        std::fs::read_link(mountpoint.join("link")).expect("readlink"),
        std::path::Path::new("f.md")
    );
    // following the link reads the target's content
    assert_eq!(std::fs::read(mountpoint.join("link")).unwrap(), b"da");

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `truncate` on a governed file is a write in disguise — if the truncated
/// content no longer validates, it must be rejected like any other bad write.
#[test]
fn truncate_on_governed_file_is_gated() {
    let (fs, dir) = fresh_fs("trunc");
    let schema_root = dir.join("schemas");
    std::fs::create_dir_all(schema_root.join(".types")).unwrap();
    std::fs::write(schema_root.join(".types/person.json"), PERSON_SCHEMA).unwrap();
    let registry = Registry::load(&schema_root).unwrap();

    let mountpoint = dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).unwrap();
    let session = mount::spawn(fs, registry, &mountpoint).expect("mount");
    wait_mounted(&mountpoint);
    std::fs::create_dir(mountpoint.join("people")).unwrap();

    let p = mountpoint.join("people/p.md");
    std::fs::write(&p, "---\ntype: person\nage: 30\n---\nbody\n").unwrap();

    // Truncating mid-frontmatter leaves an unclosed `---` fence — a parse error
    // on a governed path, i.e. invalid. Must be rejected. (Truncating to 0 would
    // be *allowed*: an empty file is merely untyped, not an invalid person.)
    let fh = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
    assert!(
        fh.set_len(20).is_err(),
        "a truncate that invalidates a governed file must be rejected"
    );
    drop(fh);
    // Original content survives the rejected truncate.
    assert!(std::fs::read_to_string(&p).unwrap().contains("age: 30"));

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Concurrency regression: many writers at once must not deadlock the mount.
/// With fuser's default single event-loop thread this hung indefinitely (the
/// lone worker blocks in a handler while the kernel needs it for a dependent
/// request). Multi-threaded dispatch (see `mount::config`) fixes it. The
/// channel + `recv_timeout` makes a regression FAIL cleanly instead of hanging
/// the whole test binary.
#[test]
fn concurrent_writers_do_not_deadlock() {
    let (fs, dir) = fresh_fs("concurrent");
    let mountpoint = dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).unwrap();
    let session = mount::spawn(fs, Registry::empty(), &mountpoint).expect("mount");
    wait_mounted(&mountpoint);
    std::fs::create_dir(mountpoint.join("d")).unwrap();

    const N: usize = 24;
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let p = mountpoint.join(format!("d/f{i}.md"));
            std::thread::spawn(move || std::fs::write(&p, format!("file {i}\n")).expect("write"))
        })
        .collect();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for h in handles {
            h.join().expect("a writer thread panicked");
        }
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(30))
        .expect("concurrent writers deadlocked — is the mount single-threaded again?");

    let count = std::fs::read_dir(mountpoint.join("d")).unwrap().count();
    assert_eq!(count, N, "every concurrent write should have committed");

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

const PERSON_SCHEMA: &str = r#"{
    "globs": ["people/*.md"],
    "type": "object",
    "required": ["type"],
    "properties": {
        "type": { "const": "person" },
        "age": { "type": "integer" }
    }
}"#;

/// The money-shot: a schema-violating write is rejected on the write path (the
/// fsync errors, nothing persists, the previous contents survive) and a
/// `.errors` sidecar explains why; a valid write commits cleanly.
#[test]
fn validation_gate_rejects_bad_write_and_commits_good() {
    use std::io::Write;

    let (fs, dir) = fresh_fs("gate");

    // A local schema registry mounted as the validator.
    let schema_root = dir.join("schemas");
    std::fs::create_dir_all(schema_root.join(".types")).unwrap();
    std::fs::write(schema_root.join(".types/person.json"), PERSON_SCHEMA).unwrap();
    let registry = Registry::load(&schema_root).unwrap();

    let mountpoint = dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).unwrap();
    let session = mount::spawn(fs, registry, &mountpoint).expect("mount");
    wait_mounted(&mountpoint);

    std::fs::create_dir(mountpoint.join("people")).expect("mkdir people");

    // --- bad write: age is a string, schema wants an integer ---
    let bad = mountpoint.join("people/bad.md");
    {
        let mut f = std::fs::File::create(&bad).unwrap();
        f.write_all(b"---\ntype: person\nage: oops\n---\nbody\n").unwrap();
        // fsync runs the validator; the violation must surface as an error.
        assert!(
            f.sync_all().is_err(),
            "schema-violating write must be rejected at the commit barrier"
        );
    }
    // Rejected content must not persist. Ground truth is the backing store: a
    // fresh read goes through `open`, which misses in jfs because nothing was
    // committed. The `create` reply used a zero TTL, so no phantom positive
    // dentry lingers either — `exists()` is already false.
    assert!(!bad.exists(), "a rejected create must leave no phantom dentry");
    match std::fs::read(&bad) {
        Err(e) => assert_eq!(
            e.kind(),
            std::io::ErrorKind::NotFound,
            "rejected file should not be readable"
        ),
        Ok(bytes) => panic!("rejected content must not persist, but read back {} bytes", bytes.len()),
    }
    // The sidecar tells the agent exactly what was wrong.
    let errs = std::fs::read_to_string(mountpoint.join("people/bad.md.errors"))
        .expect("a rejected write writes a .errors sidecar");
    assert!(
        errs.contains("age"),
        "sidecar should name the offending field, got: {errs}"
    );

    // --- good write: conforms to the schema ---
    let good = mountpoint.join("people/alice.md");
    let body = "---\ntype: person\nage: 30\n---\nhi\n";
    std::fs::write(&good, body).expect("valid write should commit");
    assert_eq!(std::fs::read_to_string(&good).unwrap(), body);
    assert!(
        !mountpoint.join("people/alice.md.errors").exists(),
        "a valid write leaves no .errors sidecar"
    );

    // --- ungoverned path: no schema globs it, so anything goes ---
    let free = mountpoint.join("scratch.md");
    std::fs::write(&free, "no frontmatter here").expect("ungoverned write should pass");
    assert_eq!(std::fs::read_to_string(&free).unwrap(), "no frontmatter here");

    // --- binary file: streams straight through, never buffered or validated ---
    let bin = mountpoint.join("image.png");
    let bytes: Vec<u8> = (0u16..512).map(|b| (b % 256) as u8).collect(); // non-UTF-8
    std::fs::write(&bin, &bytes).expect("binary write should pass through");
    assert_eq!(std::fs::read(&bin).unwrap(), bytes);
    assert!(!mountpoint.join("image.png.errors").exists());

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}

/// End-to-end versioning: a valid write through the kernel is captured as a
/// version (best-effort, via the WAL) and is recoverable as history. Exercises
/// mount -> commit barrier -> recorder -> WAL -> drain -> R2 + Postgres.
#[test]
fn a_committed_write_is_recorded_as_a_version() {
    let (fs, dir) = fresh_fs("ver");

    let schema_root = dir.join("schemas");
    std::fs::create_dir_all(schema_root.join(".types")).unwrap();
    std::fs::write(schema_root.join(".types/person.json"), PERSON_SCHEMA).unwrap();
    let registry = Registry::load(&schema_root).unwrap();

    // Recorder over the live backends (local Supabase + real R2).
    let db = std::env::var("TROVE_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54322/postgres".to_string());
    let vs = trove::version::VersionStore::connect(&db).expect("version DB up? (`supabase start`)");
    let bs = trove::blobstore::BlobStore::from_env().expect("R2 creds in env");
    let rec = trove::recorder::Recorder::new(vs, bs, dir.join("wal")).unwrap();

    let mountpoint = dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).unwrap();
    let session =
        mount::spawn_with_recorder(fs, registry, Some(rec.clone()), &mountpoint).expect("mount");
    wait_mounted(&mountpoint);

    std::fs::create_dir(mountpoint.join("people")).expect("mkdir people");
    // Unique name so the shared version DB doesn't collide across runs.
    let rel = format!("people/{}.md", uniq("p"));
    let body = "---\ntype: person\nage: 30\n---\nrecorded\n";
    std::fs::write(mountpoint.join(&rel), body).expect("valid write commits");

    // The write returned without touching the DB (WAL-only); draining applies it.
    assert_eq!(rec.drain_once().unwrap().applied, 1, "one version queued for the write");

    // History is now queryable, and the recorded bytes round-trip from R2.
    let jfs_path = format!("/{rel}");
    let log = trove::version::VersionStore::connect(&db).unwrap().log(&jfs_path).unwrap();
    assert_eq!(log.len(), 1, "exactly one version (no double-commit on close)");
    assert_eq!(log[0].rev, 1);
    assert_eq!(rec.cat(&jfs_path, 1).unwrap().as_deref(), Some(body.as_bytes()));

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}
