//! Safe Rust wrapper over JuiceFS's `libjfs` C ABI.
//!
//! This is how Trove embeds a full distributed filesystem (R2 blobs + Postgres
//! metadata, strong consistency, caching) *in-process* — no kernel JuiceFS
//! mount. The `trove mount` FUSE layer (see `mount.rs`) calls these methods
//! from its handlers; storage is JuiceFS, the validation/versioning policy is
//! Trove's.
//!
//! Convention from libjfs (`sdk/java/libjfs/main.go`): functions return a
//! non-negative value on success (a file handle, or a byte count, or 0) and a
//! **negative errno** on failure.

use anyhow::{bail, Result};
use std::ffi::CString;
use std::os::raw::{c_char, c_int};

/// Mirrors the cgo `fileInfo` struct in `libjfs-amd64.h`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FileInfo {
    pub inode: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    pub nlink: u32,
    pub length: u64,
}

impl FileInfo {
    /// POSIX `S_ISDIR` on the mode bits.
    pub fn is_dir(&self) -> bool {
        self.mode & 0o170000 == 0o040000
    }
}

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
    fn jfs_fsync(pid: i64, fd: c_int) -> c_int;
    fn jfs_close(pid: i64, fd: c_int) -> c_int;
    fn jfs_mkdir(pid: i64, h: i64, path: *const c_char, mode: u16, umask: u16) -> c_int;
    fn jfs_rmdir(pid: i64, h: i64, path: *const c_char) -> c_int;
    fn jfs_unlink(pid: i64, h: i64, path: *const c_char) -> c_int;
    fn jfs_stat(pid: i64, h: i64, path: *const c_char, info: *mut FileInfo) -> c_int;
}

/// libjfs takes a per-call pid for permission context; 0 is fine for our
/// single-identity use.
const PID: i64 = 0;

fn cs(s: &str) -> Result<CString> {
    Ok(CString::new(s)?)
}

/// Map a libjfs return code: `< 0` is `-errno`.
fn check(ret: c_int, op: &str) -> Result<c_int> {
    if ret < 0 {
        bail!("{op}: errno {}", -ret);
    }
    Ok(ret)
}

/// An open JuiceFS filesystem handle. Drop order matters: keep `Fs` alive for
/// as long as any `File` from it (enforced via the borrow on `File`).
pub struct Fs {
    handle: i64,
}

impl Fs {
    /// Open an already-formatted JuiceFS volume named `name`, whose metadata
    /// lives at `meta` (e.g. `postgres://…`, `sqlite3://…`). `cache_dir` is a
    /// local scratch dir for the block cache.
    pub fn init(name: &str, meta: &str, cache_dir: &str) -> Result<Fs> {
        // The byte-string fields MUST be non-empty or libjfs panics the whole
        // process in ParseBytesStr/ParseMbpsStr (learned in the spike).
        let conf = serde_json::json!({
            "meta": meta,
            "cacheDir": cache_dir,
            "cacheSize": "1024",
            "memorySize": "300",
            "readahead": "0",
            "uploadLimit": "0",
            "downloadLimit": "0",
            "autoCreate": true,
            "noUsageReport": true,
            "caller": 1,
        })
        .to_string();

        let (cname, cconf) = (cs(name)?, cs(&conf)?);
        let root = cs("root")?;
        let handle = unsafe {
            jfs_init(
                0,
                0,
                cname.as_ptr(),
                cconf.as_ptr(),
                root.as_ptr(),
                root.as_ptr(),
                root.as_ptr(),
                root.as_ptr(),
            )
        };
        if handle <= 0 {
            bail!("jfs_init failed for volume {name:?} (handle {handle})");
        }
        Ok(Fs { handle })
    }

    /// Create a file (O_CREAT|O_WRONLY semantics) and return an open handle.
    pub fn create(&self, path: &str, mode: u16) -> Result<File> {
        let cpath = cs(path)?;
        let fd = unsafe { jfs_create(PID, self.handle, cpath.as_ptr(), mode, 0) };
        Ok(File { fd: check(fd, "create")? })
    }

    /// Open an existing file. `flags` are POSIX open flags (0 = O_RDONLY).
    pub fn open(&self, path: &str, flags: c_int) -> Result<File> {
        let cpath = cs(path)?;
        let fd = unsafe { jfs_open(PID, self.handle, cpath.as_ptr(), 0, flags) };
        Ok(File { fd: check(fd, "open")? })
    }

    pub fn mkdir(&self, path: &str, mode: u16) -> Result<()> {
        let cpath = cs(path)?;
        check(unsafe { jfs_mkdir(PID, self.handle, cpath.as_ptr(), mode, 0) }, "mkdir")?;
        Ok(())
    }

    pub fn rmdir(&self, path: &str) -> Result<()> {
        let cpath = cs(path)?;
        check(unsafe { jfs_rmdir(PID, self.handle, cpath.as_ptr()) }, "rmdir")?;
        Ok(())
    }

    pub fn unlink(&self, path: &str) -> Result<()> {
        let cpath = cs(path)?;
        check(unsafe { jfs_unlink(PID, self.handle, cpath.as_ptr()) }, "unlink")?;
        Ok(())
    }

    pub fn stat(&self, path: &str) -> Result<FileInfo> {
        let cpath = cs(path)?;
        let mut info = FileInfo::default();
        check(unsafe { jfs_stat(PID, self.handle, cpath.as_ptr(), &mut info) }, "stat")?;
        Ok(info)
    }
}

/// An open file handle. Closes on drop. Must not outlive its `Fs` (the libjfs
/// volume handle); the `mount` layer guarantees this by owning both together.
pub struct File {
    fd: c_int,
}

impl File {
    /// Write `buf` at `offset`; returns bytes written.
    pub fn write_at(&self, buf: &[u8], offset: i64) -> Result<usize> {
        let n = unsafe { jfs_pwrite(PID, self.fd, buf.as_ptr() as usize, buf.len() as c_int, offset) };
        Ok(check(n, "pwrite")? as usize)
    }

    /// Read up to `buf.len()` bytes at `offset`; returns bytes read.
    pub fn read_at(&self, buf: &mut [u8], offset: i64) -> Result<usize> {
        let n = unsafe { jfs_pread(PID, self.fd, buf.as_mut_ptr() as usize, buf.len() as c_int, offset) };
        Ok(check(n, "pread")? as usize)
    }

    pub fn flush(&self) -> Result<()> {
        check(unsafe { jfs_flush(PID, self.fd) }, "flush")?;
        Ok(())
    }

    pub fn fsync(&self) -> Result<()> {
        check(unsafe { jfs_fsync(PID, self.fd) }, "fsync")?;
        Ok(())
    }
}

impl Drop for File {
    fn drop(&mut self) {
        unsafe { jfs_close(PID, self.fd) };
    }
}
