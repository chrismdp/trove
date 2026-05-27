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

    let session = mount::spawn(fs, &mountpoint).expect("mount");
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

    let session = mount::spawn(fs, &mountpoint).expect("mount");
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

    let session = mount::spawn(fs, &mountpoint).expect("mount");
    wait_mounted(&mountpoint);

    let f = mountpoint.join("tmp.md");
    std::fs::write(&f, "bye").unwrap();
    assert!(f.exists());
    std::fs::remove_file(&f).expect("unlink via kernel");
    assert!(!f.exists());

    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
}
