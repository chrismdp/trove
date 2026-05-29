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

// Hard-stop with a clear message on unsupported triples — otherwise the
// `extern "C"` block compiles but the binary won't link.
#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "aarch64"),
)))]
compile_error!(
    "trove `mount` feature: unsupported target. \
     Supported: linux/x86_64, linux/aarch64, macos/aarch64 (Apple Silicon). \
     Intel Macs are not supported — see docs/packaging.md."
);

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
    /// POSIX `S_ISLNK` on the mode bits.
    pub fn is_symlink(&self) -> bool {
        self.mode & 0o170000 == 0o120000
    }
}

/// One directory entry from `readdir` — enough for FUSE `readdir` to report a
/// name and whether it's a directory.
pub struct DirEntry {
    pub name: String,
    pub mode: u32,
}

impl DirEntry {
    pub fn is_dir(&self) -> bool {
        self.mode & 0o170000 == 0o040000
    }
    pub fn is_symlink(&self) -> bool {
        self.mode & 0o170000 == 0o120000
    }
}

// Platform-aware link selection. The rustc link name is shared across OSes
// (linker picks `.so` vs `.dylib`); see `build.rs` and `docs/packaging.md`
// for the full filename matrix.
#[cfg_attr(all(target_os = "linux", target_arch = "x86_64"), link(name = "jfs-amd64"))]
#[cfg_attr(all(target_os = "linux", target_arch = "aarch64"), link(name = "jfs-arm64"))]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), link(name = "jfs-arm64"))]
extern "C" {
    fn jfs_format(json_conf: *const c_char) -> c_int;
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
    fn jfs_lstat(pid: i64, h: i64, path: *const c_char, info: *mut FileInfo) -> c_int;
    fn jfs_rename(pid: i64, h: i64, oldpath: *const c_char, newpath: *const c_char) -> c_int;
    // Copy-on-write clone: new metadata sharing src's data blocks (refcount bump,
    // no byte copy). `preserve` keeps uid/gid/mode/times. This is how Trove
    // snapshots versions without duplicating content.
    fn jfs_clone(pid: i64, h: i64, src: *const c_char, dst: *const c_char, preserve: u8) -> c_int;
    fn jfs_chmod(pid: i64, h: i64, path: *const c_char, mode: u32) -> c_int;
    fn jfs_chown(pid: i64, h: i64, path: *const c_char, uid: u32, gid: u32) -> c_int;
    // mtime/atime in milliseconds; -1 leaves a field unchanged.
    fn jfs_utime(pid: i64, h: i64, path: *const c_char, mtime: i64, atime: i64) -> c_int;
    fn jfs_truncate(pid: i64, h: i64, path: *const c_char, length: u64) -> c_int;
    fn jfs_access(pid: i64, h: i64, path: *const c_char, flags: i64) -> c_int;
    fn jfs_symlink(pid: i64, h: i64, target: *const c_char, link: *const c_char) -> c_int;
    fn jfs_readlink(pid: i64, h: i64, link: *const c_char, buf: usize, bufsize: c_int) -> c_int;
    fn jfs_link(pid: i64, h: i64, src: *const c_char, dst: *const c_char) -> c_int;
    // Writes a 16-byte native-endian buffer: u64 total, u64 avail (bytes).
    fn jfs_statvfs(pid: i64, h: i64, buf: usize) -> c_int;
    // Allocates `*buf` with C `malloc` (caller frees). Per entry, big-endian:
    // u16 name_len, name bytes, then 44 bytes of stat beginning with u32 mode.
    fn jfs_listdir2(
        pid: i64,
        h: i64,
        cpath: *const c_char,
        plus: u8,
        buf: *mut *mut u8,
        size: *mut i64,
    ) -> c_int;

    // --- Extended attributes ---
    // Caller-allocated buffer: positive return = bytes written, -ERANGE = need bigger.
    fn jfs_setXattr(
        pid: i64,
        h: i64,
        path: *const c_char,
        name: *const c_char,
        value: usize,
        vlen: c_int,
        mode: c_int,
    ) -> c_int;
    fn jfs_getXattr(
        pid: i64,
        h: i64,
        path: *const c_char,
        name: *const c_char,
        buf: usize,
        bufsize: c_int,
    ) -> c_int;
    fn jfs_listXattr(pid: i64, h: i64, path: *const c_char, buf: usize, bufsize: c_int) -> c_int;
    fn jfs_removeXattr(pid: i64, h: i64, path: *const c_char, name: *const c_char) -> c_int;

    // --- POSIX advisory locks (added via libjfs patches/0002-add-locks.patch) ---
    // `ltype` uses fcntl numbering: F_RDLCK=0, F_WRLCK=1, F_UNLCK=2.
    fn jfs_flock(pid: i64, fd: c_int, owner: u64, ltype: c_int) -> c_int;
    fn jfs_setlk(
        pid: i64,
        fd: c_int,
        owner: u64,
        ltype: c_int,
        start: u64,
        end: u64,
        block: u8,
        lpid: u32,
    ) -> c_int;
    fn jfs_getlk(
        pid: i64,
        fd: c_int,
        owner: u64,
        ltype: c_int,
        start: u64,
        end: u64,
        out_type: *mut c_int,
        out_start: *mut u64,
        out_end: *mut u64,
        out_pid: *mut u32,
    ) -> c_int;
}

// POSIX fcntl lock types (Linux numbering; matches what libjfs/JuiceFS expects
// on the FFI). The mount layer accepts these directly from FUSE; the BSD
// flock(2) call path is converted by the kernel into whole-file setlk requests
// using F_RDLCK / F_WRLCK / F_UNLCK, so we never see raw LOCK_SH/LOCK_EX/LOCK_UN
// from the kernel. The flock() convenience methods on `File` translate libc's
// BSD constants into these on the way down.
pub const F_RDLCK: c_int = 0;
pub const F_WRLCK: c_int = 1;
pub const F_UNLCK: c_int = 2;

/// `getlk` result. `typ == F_UNLCK` means "no conflict — the proposed lock
/// would succeed". Anything else describes the conflicting lock holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockInfo {
    pub typ: c_int,
    pub start: u64,
    pub end: u64,
    pub pid: u32,
}

/// Distinguished error type for lock calls so the FUSE handler (and direct
/// callers) can map -EAGAIN cleanly without parsing an `anyhow` string. Carries
/// the raw positive errno value.
#[derive(Debug, Clone, Copy)]
pub struct LockErrno(pub c_int);

impl std::fmt::Display for LockErrno {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lock op failed: errno {}", self.0)
    }
}

impl std::error::Error for LockErrno {}

/// Convert a libjfs lock return to a typed errno. libjfs returns `< 0 = -errno`.
fn check_lock(ret: c_int) -> std::result::Result<(), LockErrno> {
    if ret < 0 {
        Err(LockErrno(-ret))
    } else {
        Ok(())
    }
}

// setXattr mode constants (libjfs follows Linux semantics).
/// Default — create if absent, replace if present.
pub const XATTR_CREATE_OR_REPLACE: c_int = 0;
/// Fail with EEXIST if the attribute already exists.
pub const XATTR_CREATE: c_int = 1;
/// Fail with ENODATA if the attribute does not already exist.
pub const XATTR_REPLACE: c_int = 2;

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

/// Format a JuiceFS volume in-process.
///
/// JSON keys (lowercase camelCase, matching the libjfs `formatConf` Go struct):
/// `meta` (postgres/sqlite URL — required), `name` (volume name — required),
/// `storage` (e.g. `"s3"`, `"file"`), `bucket`, `accessKey`, `secretKey`,
/// `blockSize` (KB, default 4096), `compress` (default `"none"`),
/// `trashDays` (default 1), `force` (default false).
///
/// This is the in-process equivalent of `juicefs format`. It runs the same
/// blob-store sanity probe (put + get + delete a tiny object) before persisting
/// the format row to the metadata DB. Returns once the volume is ready for
/// `Fs::init` to open.
pub fn format(config: &serde_json::Value) -> Result<()> {
    let conf_str = config.to_string();
    let cconf = cs(&conf_str)?;
    let ret = unsafe { jfs_format(cconf.as_ptr()) };
    if ret < 0 {
        bail!(
            "jfs_format failed (return {ret}, errno {}). \
             Check libjfs logs above for the specific cause \
             (auth, bucket, meta URL, etc.).",
            -ret
        );
    }
    Ok(())
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
        // maxUploads/maxDownloads are set explicitly: omitting them leaves
        // JuiceFS at 0, which its SelfCheck clamps to a single upload thread (a
        // real throughput cap), so we give it JuiceFS's normal defaults.
        let conf = serde_json::json!({
            "meta": meta,
            "cacheDir": cache_dir,
            "cacheSize": "1024",
            "memorySize": "300",
            "readahead": "0",
            "uploadLimit": "0",
            "downloadLimit": "0",
            "maxUploads": 20,
            "maxDownloads": 200,
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

    /// Like `stat`, but does NOT follow a final symlink (POSIX `lstat`). The
    /// FUSE layer reports attributes with this so a symlink shows as `S_IFLNK`
    /// (the kernel resolves links itself via `readlink`).
    pub fn lstat(&self, path: &str) -> Result<FileInfo> {
        let cpath = cs(path)?;
        let mut info = FileInfo::default();
        check(unsafe { jfs_lstat(PID, self.handle, cpath.as_ptr(), &mut info) }, "lstat")?;
        Ok(info)
    }

    /// Does a path exist in the volume?
    pub fn exists(&self, path: &str) -> bool {
        self.stat(path).is_ok()
    }

    /// Read a whole file into memory. The `mount` layer buffers the full
    /// contents on open so it can validate the file as a unit before it commits
    /// (markdown notes are small; whole-file buffering is the design).
    pub fn read_all(&self, path: &str) -> Result<Vec<u8>> {
        let info = self.stat(path)?;
        let f = self.open(path, 0)?;
        let mut buf = vec![0u8; info.length as usize];
        let (mut filled, mut off) = (0usize, 0i64);
        while filled < buf.len() {
            let n = f.read_at(&mut buf[filled..], off)?;
            if n == 0 {
                break;
            }
            filled += n;
            off += n as i64;
        }
        buf.truncate(filled);
        Ok(buf)
    }

    /// Write a whole file, replacing any existing contents (truncate semantics).
    /// This is the commit step on the write path: once a buffer has passed
    /// validation, its bytes land here atomically from the agent's point of view
    /// (a single close/fsync).
    ///
    /// **Mode preservation**: if the file already exists, we truncate + overwrite
    /// it in place, which keeps its inode and therefore its mode, uid, gid, and
    /// xattrs untouched. The `mode` argument is only used when creating a fresh
    /// file (where there's no existing metadata to preserve). This matters because
    /// validated writes through the mount mustn't reset `chmod +x` on an edit
    /// (commits, `trove restore`, etc.).
    pub fn write_all(&self, path: &str, bytes: &[u8], mode: u16) -> Result<()> {
        let f = if self.exists(path) {
            self.truncate(path, 0)?;
            // O_WRONLY = 1 on Linux + macOS. We use the literal because libjfs
            // takes a c_int and we don't want a libc dep just for the constant
            // in the core API.
            self.open(path, 1)?
        } else {
            self.create(path, mode)?
        };
        let mut off = 0i64;
        while (off as usize) < bytes.len() {
            let n = f.write_at(&bytes[off as usize..], off)?;
            if n == 0 {
                bail!("short write to {path}");
            }
            off += n as i64;
        }
        f.flush()?;
        f.fsync()?;
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Extended attributes
    //
    // libjfs / JuiceFS support xattrs natively (one row per (inode, name) in
    // Postgres), so we surface them as plain pass-through. Trove's substrate
    // doesn't touch xattrs — they're outside the validated file content, so
    // they sit alongside it the way they would on any POSIX filesystem.

    /// Set an extended attribute on `path`. `mode` controls create/replace
    /// semantics — see [`XATTR_CREATE_OR_REPLACE`], [`XATTR_CREATE`],
    /// [`XATTR_REPLACE`].
    pub fn set_xattr(&self, path: &str, name: &str, value: &[u8], mode: c_int) -> Result<()> {
        let (cpath, cname) = (cs(path)?, cs(name)?);
        let ret = unsafe {
            jfs_setXattr(
                PID,
                self.handle,
                cpath.as_ptr(),
                cname.as_ptr(),
                value.as_ptr() as usize,
                value.len() as c_int,
                mode,
            )
        };
        check(ret, "setxattr")?;
        Ok(())
    }

    /// Get an extended attribute's value. Returns `None` if the attribute is
    /// absent (-ENODATA, errno 61 on Linux / 93 on macOS — we treat any "no such
    /// attribute" return as `None` rather than an error, matching the POSIX shape
    /// callers expect).
    pub fn get_xattr(&self, path: &str, name: &str) -> Result<Option<Vec<u8>>> {
        let (cpath, cname) = (cs(path)?, cs(name)?);
        // libjfs's jfs_getXattr doesn't follow the POSIX "size=0 returns
        // needed length" convention reliably — passing bufsize=0 returned 0
        // here (not the needed size). So we grow on -ERANGE instead. Starts
        // at 256 (covers most metadata) and doubles to a sane cap.
        let mut cap = 256usize;
        loop {
            let mut buf = vec![0u8; cap];
            let got = unsafe {
                jfs_getXattr(
                    PID,
                    self.handle,
                    cpath.as_ptr(),
                    cname.as_ptr(),
                    buf.as_mut_ptr() as usize,
                    buf.len() as c_int,
                )
            };
            if got == -61 || got == -93 {
                // ENODATA on Linux / ENOATTR on macOS = attribute missing.
                return Ok(None);
            }
            if got == -34 {
                // ERANGE — buffer too small; grow and retry.
                if cap >= 1 << 22 {
                    bail!("getxattr value too large (> 4 MiB)");
                }
                cap *= 2;
                continue;
            }
            let n = check(got, "getxattr")? as usize;
            buf.truncate(n);
            return Ok(Some(buf));
        }
    }

    /// List the extended attribute names on `path`. The buffer is a sequence of
    /// NUL-terminated names (the POSIX format `listxattr` returns); we split it
    /// into a `Vec<String>` for convenience.
    pub fn list_xattr(&self, path: &str) -> Result<Vec<String>> {
        let buf = self.list_xattr_raw(path)?;
        Ok(buf
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect())
    }

    /// Returns the raw NUL-separated bytes that POSIX `listxattr(2)` would
    /// produce — the FUSE layer needs them in this exact form so it can hand
    /// them back to the kernel without re-encoding. Includes a trailing NUL
    /// after each name; an empty list returns an empty Vec.
    pub fn list_xattr_raw(&self, path: &str) -> Result<Vec<u8>> {
        let cpath = cs(path)?;
        // Same libjfs quirk as get_xattr: size probe with bufsize=0 isn't
        // reliable, so grow on -ERANGE instead.
        let mut cap = 256usize;
        loop {
            let mut buf = vec![0u8; cap];
            let got = unsafe {
                jfs_listXattr(
                    PID,
                    self.handle,
                    cpath.as_ptr(),
                    buf.as_mut_ptr() as usize,
                    buf.len() as c_int,
                )
            };
            if got == -34 {
                if cap >= 1 << 22 {
                    bail!("listxattr buffer would exceed 4 MiB");
                }
                cap *= 2;
                continue;
            }
            let n = check(got, "listxattr")? as usize;
            buf.truncate(n);
            return Ok(buf);
        }
    }

    /// Remove an extended attribute. Returns `Ok(false)` if the attribute was
    /// already absent; `Ok(true)` if it was removed.
    pub fn remove_xattr(&self, path: &str, name: &str) -> Result<bool> {
        let (cpath, cname) = (cs(path)?, cs(name)?);
        let ret = unsafe { jfs_removeXattr(PID, self.handle, cpath.as_ptr(), cname.as_ptr()) };
        if ret == -61 || ret == -93 {
            return Ok(false);
        }
        check(ret, "removexattr")?;
        Ok(true)
    }

    /// Rename/move within the volume.
    pub fn rename(&self, oldpath: &str, newpath: &str) -> Result<()> {
        let (old, new) = (cs(oldpath)?, cs(newpath)?);
        check(unsafe { jfs_rename(PID, self.handle, old.as_ptr(), new.as_ptr()) }, "rename")?;
        Ok(())
    }

    /// Copy-on-write clone of `src` to `dst` (no data copy — shares blocks via
    /// refcount). Trove uses this to snapshot a committed file into the version
    /// archive. `dst`'s parent directory must already exist.
    pub fn clone_file(&self, src: &str, dst: &str, preserve: bool) -> Result<()> {
        let (s, d) = (cs(src)?, cs(dst)?);
        check(
            unsafe { jfs_clone(PID, self.handle, s.as_ptr(), d.as_ptr(), preserve as u8) },
            "clone",
        )?;
        Ok(())
    }

    pub fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        let cpath = cs(path)?;
        check(unsafe { jfs_chmod(PID, self.handle, cpath.as_ptr(), mode) }, "chmod")?;
        Ok(())
    }

    pub fn chown(&self, path: &str, uid: u32, gid: u32) -> Result<()> {
        let cpath = cs(path)?;
        check(unsafe { jfs_chown(PID, self.handle, cpath.as_ptr(), uid, gid) }, "chown")?;
        Ok(())
    }

    /// Set mtime/atime in milliseconds; pass `-1` to leave a field unchanged.
    pub fn utime(&self, path: &str, mtime_ms: i64, atime_ms: i64) -> Result<()> {
        let cpath = cs(path)?;
        check(unsafe { jfs_utime(PID, self.handle, cpath.as_ptr(), mtime_ms, atime_ms) }, "utime")?;
        Ok(())
    }

    pub fn truncate(&self, path: &str, length: u64) -> Result<()> {
        let cpath = cs(path)?;
        check(unsafe { jfs_truncate(PID, self.handle, cpath.as_ptr(), length) }, "truncate")?;
        Ok(())
    }

    pub fn access(&self, path: &str, mask: i64) -> Result<()> {
        let cpath = cs(path)?;
        check(unsafe { jfs_access(PID, self.handle, cpath.as_ptr(), mask) }, "access")?;
        Ok(())
    }

    pub fn symlink(&self, target: &str, link: &str) -> Result<()> {
        let (t, l) = (cs(target)?, cs(link)?);
        check(unsafe { jfs_symlink(PID, self.handle, t.as_ptr(), l.as_ptr()) }, "symlink")?;
        Ok(())
    }

    pub fn link(&self, src: &str, dst: &str) -> Result<()> {
        let (s, d) = (cs(src)?, cs(dst)?);
        check(unsafe { jfs_link(PID, self.handle, s.as_ptr(), d.as_ptr()) }, "link")?;
        Ok(())
    }

    /// Read a symlink's target.
    pub fn readlink(&self, path: &str) -> Result<String> {
        let cpath = cs(path)?;
        let mut buf = vec![0u8; 4096];
        let n = check(
            unsafe { jfs_readlink(PID, self.handle, cpath.as_ptr(), buf.as_mut_ptr() as usize, buf.len() as c_int) },
            "readlink",
        )? as usize;
        buf.truncate(n);
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// `(total, available)` bytes for the volume.
    pub fn statvfs(&self) -> Result<(u64, u64)> {
        let mut buf = [0u8; 16];
        check(unsafe { jfs_statvfs(PID, self.handle, buf.as_mut_ptr() as usize) }, "statvfs")?;
        // Native-endian (little-endian on our target).
        let total = u64::from_ne_bytes(buf[0..8].try_into().unwrap());
        let avail = u64::from_ne_bytes(buf[8..16].try_into().unwrap());
        Ok((total, avail))
    }

    /// List a directory's entries (excludes `.`/`..`). Uses `jfs_listdir2`,
    /// which mallocs the result buffer; we copy what we need and free it.
    pub fn readdir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let cpath = cs(path)?;
        let mut buf: *mut u8 = std::ptr::null_mut();
        let mut size: i64 = 0;
        let ret = unsafe { jfs_listdir2(PID, self.handle, cpath.as_ptr(), 1, &mut buf, &mut size) };
        if ret < 0 {
            bail!("readdir {path}: errno {}", -ret);
        }
        if buf.is_null() || size <= 0 {
            return Ok(Vec::new());
        }
        let entries = {
            let bytes = unsafe { std::slice::from_raw_parts(buf, size as usize) };
            parse_listdir2(bytes)
        };
        unsafe { libc::free(buf as *mut std::ffi::c_void) };
        Ok(entries)
    }
}

/// Parse the big-endian `jfs_listdir2(plus=1)` buffer. Per entry: u16 name_len,
/// name bytes, then a 44-byte stat block whose first field is the u32 mode.
fn parse_listdir2(b: &[u8]) -> Vec<DirEntry> {
    const STAT_LEN: usize = 44; // mode(4)+inode(8)+nlink(4)+uid(4)+gid(4)+length(8)+atime+mtime+ctime(4*3)
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 2 <= b.len() {
        let name_len = u16::from_be_bytes([b[i], b[i + 1]]) as usize;
        i += 2;
        if i + name_len + STAT_LEN > b.len() {
            break; // truncated / malformed — stop rather than read OOB
        }
        let name = String::from_utf8_lossy(&b[i..i + name_len]).into_owned();
        i += name_len;
        let mode = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        i += STAT_LEN;
        if name != "." && name != ".." {
            out.push(DirEntry { name, mode });
        }
    }
    out
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

    /// Raw libjfs fd. Needed by the mount layer in cases where it wants to call
    /// the lock FFI directly without going through these wrappers (typed errno).
    pub fn raw_fd(&self) -> c_int {
        self.fd
    }

    /// BSD-style whole-file advisory lock (`flock(2)` equivalent). `ltype` is
    /// one of [`F_RDLCK`] (shared), [`F_WRLCK`] (exclusive), [`F_UNLCK`]
    /// (release). Non-blocking: on conflict returns [`LockErrno`] with EAGAIN.
    /// `owner` identifies the lock holder for conflict detection — use the
    /// process id or any stable u64.
    pub fn flock(&self, owner: u64, ltype: c_int) -> std::result::Result<(), LockErrno> {
        check_lock(unsafe { jfs_flock(PID, self.fd, owner, ltype) })
    }

    /// POSIX byte-range advisory lock (`fcntl(2)` F_SETLK / F_SETLKW). `end` of
    /// `u64::MAX` means "to EOF" (the Meta layer's convention). `block = true`
    /// is F_SETLKW (waits indefinitely); `block = false` is F_SETLK and returns
    /// EAGAIN on conflict. `lpid` is the holder pid recorded in the lock entry
    /// — `getlk` returns it to the caller.
    pub fn setlk(
        &self,
        owner: u64,
        ltype: c_int,
        start: u64,
        end: u64,
        block: bool,
        lpid: u32,
    ) -> std::result::Result<(), LockErrno> {
        check_lock(unsafe {
            jfs_setlk(PID, self.fd, owner, ltype, start, end, block as u8, lpid)
        })
    }

    /// POSIX lock query (`fcntl(2)` F_GETLK). Returns `LockInfo` describing
    /// whether a conflicting lock exists: `typ == F_UNLCK` (the response from
    /// libjfs) means no conflict — the proposed lock would succeed. Otherwise
    /// the returned range/pid describes the conflicting holder.
    pub fn getlk(
        &self,
        owner: u64,
        ltype: c_int,
        start: u64,
        end: u64,
    ) -> std::result::Result<LockInfo, LockErrno> {
        let mut out_type: c_int = 0;
        let mut out_start: u64 = 0;
        let mut out_end: u64 = 0;
        let mut out_pid: u32 = 0;
        check_lock(unsafe {
            jfs_getlk(
                PID,
                self.fd,
                owner,
                ltype,
                start,
                end,
                &mut out_type,
                &mut out_start,
                &mut out_end,
                &mut out_pid,
            )
        })?;
        Ok(LockInfo {
            typ: out_type,
            start: out_start,
            end: out_end,
            pid: out_pid,
        })
    }
}

impl Drop for File {
    fn drop(&mut self) {
        unsafe { jfs_close(PID, self.fd) };
    }
}
