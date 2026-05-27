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
