//! Trove libjfs FFI spike.
//!
//! Proves: a Rust process can drive a JuiceFS volume directly via libjfs's
//! C ABI — init → create → write → close → open → read → verify — with NO
//! kernel FUSE mount. This is the load-bearing unknown for the single-mount
//! Trove design (Rust `fuser` handlers calling libjfs in-process).
//!
//! Signatures mirror the cgo `//export jfs_*` functions in
//! juicefs/sdk/java/libjfs/main.go. cgo maps Go int64->c_longlong,
//! int32->c_int, uintptr->usize, *C.char->*const c_char.

use std::ffi::CString;
use std::os::raw::{c_char, c_int};

#[link(name = "jfs-amd64")]
extern "C" {
    fn jfs_init(
        credential_ptr: usize,
        count: c_int,
        name: *const c_char,
        json_conf: *const c_char,
        user: *const c_char,
        group: *const c_char,
        superuser: *const c_char,
        supergroup: *const c_char,
    ) -> i64;

    fn jfs_create(pid: i64, h: i64, path: *const c_char, mode: u16, umask: u16) -> c_int;
    fn jfs_open(pid: i64, h: i64, path: *const c_char, len_ptr: usize, flags: c_int) -> c_int;
    fn jfs_pwrite(pid: i64, fd: c_int, buf: usize, count: c_int, offset: i64) -> c_int;
    fn jfs_pread(pid: i64, fd: c_int, buf: usize, count: c_int, offset: i64) -> c_int;
    fn jfs_flush(pid: i64, fd: c_int) -> c_int;
    fn jfs_close(pid: i64, fd: c_int) -> c_int;
}

const PID: i64 = 0;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn main() {
    let meta = std::env::var("TROVE_META")
        .unwrap_or_else(|_| "postgres://cp:spike@127.0.0.1:5432/trove_spike?sslmode=disable".into());
    let vol = std::env::var("TROVE_VOL").unwrap_or_else(|_| "trovespike".into());

    // Minimal client config — storage/bucket were set at format time and live
    // in the metadata; here we only point libjfs at the metadata engine.
    // byte-string fields (cacheSize/memorySize/readahead) must be non-empty —
    // libjfs's ParseBytesStr panics on "".
    let conf = format!(
        r#"{{"meta":"{meta}","cacheDir":"/tmp/trove-spike-cache","cacheSize":"1024","memorySize":"300","readahead":"0","uploadLimit":"0","downloadLimit":"0","autoCreate":true,"noUsageReport":true,"caller":1}}"#
    );

    println!("→ jfs_init(vol={vol})");
    let (cname, cconf) = (cs(&vol), cs(&conf));
    let (cuser, cgroup, csu, csg) = (cs("root"), cs("root"), cs("root"), cs("root"));
    let h = unsafe {
        jfs_init(
            0,
            0,
            cname.as_ptr(),
            cconf.as_ptr(),
            cuser.as_ptr(),
            cgroup.as_ptr(),
            csu.as_ptr(),
            csg.as_ptr(),
        )
    };
    assert!(h > 0, "jfs_init failed, returned handle {h}");
    println!("  ✓ handle = {h}");

    let path = cs("/trove-ffi-proof.txt");
    let payload = b"trove proves: rust -> libjfs -> object store, no fuse mount\n";

    // create + write
    let fd = unsafe { jfs_create(PID, h, path.as_ptr(), 0o644, 0) };
    assert!(fd >= 0, "jfs_create failed: {fd}");
    println!("  ✓ create fd = {fd}");

    let n = unsafe { jfs_pwrite(PID, fd, payload.as_ptr() as usize, payload.len() as c_int, 0) };
    assert_eq!(n, payload.len() as c_int, "short write: {n}");
    let fl = unsafe { jfs_flush(PID, fd) };
    assert_eq!(fl, 0, "flush errno {fl}");
    assert_eq!(unsafe { jfs_close(PID, fd) }, 0, "close failed");
    println!("  ✓ wrote {n} bytes, flushed, closed");

    // reopen + read back (O_RDONLY = 0)
    let rfd = unsafe { jfs_open(PID, h, path.as_ptr(), 0, 0) };
    assert!(rfd >= 0, "jfs_open failed: {rfd}");
    let mut buf = vec![0u8; payload.len()];
    let rn = unsafe { jfs_pread(PID, rfd, buf.as_mut_ptr() as usize, buf.len() as c_int, 0) };
    assert_eq!(rn, payload.len() as c_int, "short read: {rn}");
    unsafe { jfs_close(PID, rfd) };

    assert_eq!(&buf, payload, "round-trip mismatch!");
    println!("  ✓ read {rn} bytes, round-trip identical");
    println!("\nSPIKE PASSED — Rust drove a JuiceFS volume via libjfs with no FUSE mount.");
    println!("content: {:?}", String::from_utf8_lossy(&buf));
}
