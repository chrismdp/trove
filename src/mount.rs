//! `trove mount` — a FUSE filesystem backed by JuiceFS (via libjfs, in-process).
//!
//! **The validation gate ("a filesystem that talks back").** An open file is
//! buffered whole in its handle. Writes splice into that buffer; nothing reaches
//! JuiceFS until the commit barrier (`flush` on close, or `fsync`). At the
//! barrier the buffer is validated against the schema its path + `type` select
//! from the registry. Valid → committed to `jfs` and any stale `.errors` sidecar
//! cleared. Invalid → **rejected** (the syscall gets `EINVAL`, nothing persists,
//! the previous contents survive) and a `<path>.errors` sidecar is written
//! explaining why. The agent reads the sidecar to learn what it got wrong.
//!
//! FUSE addresses files by inode; JuiceFS by path, so we keep an inode↔path map.
//! fuser 0.17 calls handlers through `&self`, so all mutable state lives behind
//! a `Mutex`. Inode/handle ids and flags are newtypes (`INodeNo`, `FileHandle`,
//! `OpenFlags`, …) — we convert at the boundary.

use crate::jfs::{FileInfo, Fs};
use crate::types::Registry;
use fuser::{
    BackgroundSession, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
    WriteFlags,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);
const ROOT_INO: u64 = 1;

/// An open file handle. The full contents are buffered here so the file can be
/// validated as a unit at the commit barrier. `dirty` means the buffer has been
/// modified (or the file is freshly created / truncated) and has not yet been
/// committed to `jfs`.
struct OpenFile {
    path: String,
    buf: Vec<u8>,
    dirty: bool,
}

struct Inner {
    fs: Fs,
    registry: Registry,
    ino_to_path: HashMap<u64, String>,
    path_to_ino: HashMap<String, u64>,
    next_ino: u64,
    open_files: HashMap<u64, OpenFile>,
    next_fh: u64,
}

impl Inner {
    fn intern(&mut self, path: &str) -> u64 {
        if let Some(&ino) = self.path_to_ino.get(path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.ino_to_path.insert(ino, path.to_string());
        self.path_to_ino.insert(path.to_string(), ino);
        ino
    }

    fn path_of(&self, ino: u64) -> Option<String> {
        self.ino_to_path.get(&ino).cloned()
    }

    fn child_path(&self, parent: u64, name: &OsStr) -> Option<String> {
        let parent = self.path_of(parent)?;
        let name = name.to_str()?;
        Some(if parent == "/" {
            format!("/{name}")
        } else {
            format!("{parent}/{name}")
        })
    }

    fn forget_path(&mut self, path: &str) {
        if let Some(ino) = self.path_to_ino.remove(path) {
            self.ino_to_path.remove(&ino);
        }
    }

    fn new_fh(&mut self, file: OpenFile) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.open_files.insert(fh, file);
        fh
    }

    /// Size of an as-yet-uncommitted buffer for `path`, if one is open. Lets
    /// `lookup`/`getattr` answer for a freshly created file that has not been
    /// committed to `jfs` yet (it lives only in a handle's buffer).
    fn inflight_size(&self, path: &str) -> Option<u64> {
        self.open_files
            .values()
            .find(|of| of.dirty && of.path == path)
            .map(|of| of.buf.len() as u64)
    }

    /// Validate a buffer destined for `path`. `Ok(())` means it may persist;
    /// `Err(report)` carries the human-readable reason(s) for the `.errors`
    /// sidecar. Ungoverned paths (no schema selects them) always pass.
    fn validate(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        // The sidecars themselves are never gated.
        if path.ends_with(".errors") {
            return Ok(());
        }
        let rel = Path::new(path.strip_prefix('/').unwrap_or(path));

        let text = match std::str::from_utf8(bytes) {
            Ok(t) => t,
            Err(_) => return Err("file is not valid UTF-8".to_string()),
        };

        let doc = match crate::frontmatter::parse(text) {
            Ok(d) => d,
            Err(e) => {
                // A parse error is only a violation on a governed path; an
                // ungoverned file (template, vendored dir) is free to be junk.
                return if self.registry.path_is_governed(rel) {
                    Err(format!("frontmatter parse error: {e}"))
                } else {
                    Ok(())
                };
            }
        };

        let ftype = doc.frontmatter.get("type").and_then(|v| v.as_str());
        let schemas = self.registry.select(rel, ftype);
        if schemas.is_empty() {
            return Ok(());
        }

        let mut report = String::new();
        for schema in schemas {
            if let Err(violations) = crate::validate::validate_against(&doc.frontmatter, schema) {
                for v in violations {
                    report.push_str(&format!("{}: {}\n", v.instance_path, v.message));
                }
            }
        }
        if report.is_empty() {
            Ok(())
        } else {
            Err(report)
        }
    }

    /// Validate + persist the buffer for `fh` (the commit barrier). A clean
    /// (non-dirty) handle is a no-op. On rejection nothing is written to `jfs`,
    /// a `.errors` sidecar is (re)written, and `EINVAL` is returned so the
    /// failing close/fsync surfaces the rejection to the agent.
    fn commit(&mut self, fh: u64) -> Result<(), Errno> {
        let (path, buf) = match self.open_files.get(&fh) {
            Some(of) if of.dirty => (of.path.clone(), of.buf.clone()),
            Some(_) => return Ok(()),
            None => return Err(Errno::EBADF),
        };

        match self.validate(&path, &buf) {
            Ok(()) => {
                self.fs.write_all(&path, &buf, 0o644).map_err(|_| Errno::EIO)?;
                let _ = self.fs.unlink(&format!("{path}.errors")); // clear stale sidecar
                if let Some(of) = self.open_files.get_mut(&fh) {
                    of.dirty = false;
                }
                Ok(())
            }
            Err(report) => {
                let _ = self
                    .fs
                    .write_all(&format!("{path}.errors"), report.as_bytes(), 0o644);
                Err(Errno::EINVAL)
            }
        }
    }
}

pub struct TroveFs {
    inner: Mutex<Inner>,
}

impl TroveFs {
    pub fn new(fs: Fs, registry: Registry) -> Self {
        let mut ino_to_path = HashMap::new();
        let mut path_to_ino = HashMap::new();
        ino_to_path.insert(ROOT_INO, "/".to_string());
        path_to_ino.insert("/".to_string(), ROOT_INO);
        TroveFs {
            inner: Mutex::new(Inner {
                fs,
                registry,
                ino_to_path,
                path_to_ino,
                next_ino: 2,
                open_files: HashMap::new(),
                next_fh: 1,
            }),
        }
    }
}

/// Synthesise attributes for a regular file of `size` bytes that exists only in
/// a handle buffer (created but not yet committed to `jfs`).
fn synth_attr(ino: u64, size: u64) -> FileAttr {
    let now = SystemTime::now();
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    FileAttr {
        ino: INodeNo(ino),
        size,
        blocks: size.div_ceil(512),
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: FileType::RegularFile,
        perm: 0o644,
        nlink: 1,
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn to_attr(ino: u64, fi: &FileInfo) -> FileAttr {
    let when = |secs: u32| UNIX_EPOCH + Duration::from_secs(secs as u64);
    // Report the mounting user as owner so a single-user mount has access
    // without default_permissions games (jfs stores uid 0).
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    FileAttr {
        ino: INodeNo(ino),
        size: fi.length,
        blocks: fi.length.div_ceil(512),
        atime: when(fi.atime),
        mtime: when(fi.mtime),
        ctime: when(fi.ctime),
        crtime: when(fi.ctime),
        kind: if fi.is_dir() {
            FileType::Directory
        } else {
            FileType::RegularFile
        },
        perm: (fi.mode & 0o7777) as u16,
        nlink: fi.nlink.max(1),
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

impl Filesystem for TroveFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let mut g = self.inner.lock().unwrap();
        let Some(path) = g.child_path(parent.0, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        match g.fs.stat(&path) {
            Ok(fi) => {
                let ino = g.intern(&path);
                reply.entry(&TTL, &to_attr(ino, &fi), Generation(0));
            }
            Err(_) => match g.inflight_size(&path) {
                Some(size) => {
                    let ino = g.intern(&path);
                    reply.entry(&TTL, &synth_attr(ino, size), Generation(0));
                }
                None => reply.error(Errno::ENOENT),
            },
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let g = self.inner.lock().unwrap();
        let Some(path) = g.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match g.fs.stat(&path) {
            Ok(fi) => reply.attr(&TTL, &to_attr(ino.0, &fi)),
            Err(_) => match g.inflight_size(&path) {
                Some(size) => reply.attr(&TTL, &synth_attr(ino.0, size)),
                None => reply.error(Errno::ENOENT),
            },
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let mut g = self.inner.lock().unwrap();
        let Some(path) = g.child_path(parent.0, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        // Nothing touches jfs yet: a new file lives only in its buffer until it
        // validates at the commit barrier. `mode` is advisory for now (the
        // committed file is written 0o644).
        let _ = mode;
        let ino = g.intern(&path);
        let fh = g.new_fh(OpenFile {
            path,
            buf: Vec::new(),
            dirty: true,
        });
        reply.created(
            &TTL,
            &synth_attr(ino, 0),
            Generation(0),
            FileHandle(fh),
            FopenFlags::empty(),
        );
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let mut g = self.inner.lock().unwrap();
        let Some(path) = g.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        // Buffer the current contents so reads are served from memory and the
        // validator sees the whole file at commit. O_TRUNC starts empty.
        let truncating = flags.0 & libc::O_TRUNC != 0;
        let buf = if truncating {
            Vec::new()
        } else {
            match g.fs.read_all(&path) {
                Ok(b) => b,
                Err(_) => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        let fh = g.new_fh(OpenFile {
            path,
            buf,
            dirty: truncating, // truncation is itself a pending change
        });
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let g = self.inner.lock().unwrap();
        let Some(of) = g.open_files.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let start = offset as usize;
        if start >= of.buf.len() {
            reply.data(&[]);
            return;
        }
        let end = (start + size as usize).min(of.buf.len());
        reply.data(&of.buf[start..end]);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut g = self.inner.lock().unwrap();
        let Some(of) = g.open_files.get_mut(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let start = offset as usize;
        let end = start + data.len();
        if of.buf.len() < end {
            of.buf.resize(end, 0);
        }
        of.buf[start..end].copy_from_slice(data);
        of.dirty = true;
        reply.written(data.len() as u32);
    }

    /// `flush` runs on every `close()`; its return value reaches the closing
    /// syscall, so this is where a rejection becomes visible. Committing here
    /// means a plain `cp`/editor-save persists (or is rejected) on close.
    fn flush(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _lock: LockOwner, reply: ReplyEmpty) {
        let mut g = self.inner.lock().unwrap();
        match g.commit(fh.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let mut g = self.inner.lock().unwrap();
        match g.commit(fh.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let mut g = self.inner.lock().unwrap();
        // Best-effort final commit. The kernel ignores release's return value,
        // so a still-dirty buffer here (never flushed) gets one last attempt;
        // an invalid one leaves its .errors sidecar and is dropped.
        let _ = g.commit(fh.0);
        g.open_files.remove(&fh.0);
        reply.ok();
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let mut g = self.inner.lock().unwrap();
        let Some(path) = g.child_path(parent.0, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        match g.fs.mkdir(&path, (mode & 0o7777) as u16) {
            Ok(()) => {
                let ino = g.intern(&path);
                let fi = g.fs.stat(&path).unwrap_or_default();
                reply.entry(&TTL, &to_attr(ino, &fi), Generation(0));
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut g = self.inner.lock().unwrap();
        let Some(path) = g.child_path(parent.0, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        match g.fs.unlink(&path) {
            Ok(()) => {
                g.forget_path(&path);
                reply.ok();
            }
            Err(_) => reply.error(Errno::ENOENT),
        }
    }

    /// Minimal: report `.` and `..` so the mount is navigable. Full directory
    /// listing (jfs_listdir buffer protocol) is the next increment.
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if offset == 0 {
            let _ = reply.add(ino, 1, FileType::Directory, ".");
            let _ = reply.add(ino, 2, FileType::Directory, "..");
        }
        reply.ok();
    }

    /// Minimal setattr: we don't yet apply mode/size changes; report current
    /// attrs so tools that probe attributes don't fail.
    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let g = self.inner.lock().unwrap();
        let Some(path) = g.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match g.fs.stat(&path) {
            Ok(fi) => reply.attr(&TTL, &to_attr(ino.0, &fi)),
            Err(_) => match g.inflight_size(&path) {
                Some(size) => reply.attr(&TTL, &synth_attr(ino.0, size)),
                None => reply.error(Errno::ENOENT),
            },
        }
    }
}

fn config() -> fuser::Config {
    // Config is #[non_exhaustive]: build via default(), then set fields.
    let mut cfg = fuser::Config::default();
    // NB: AutoUnmount requires a non-Owner ACL (AllowOther), which needs
    // user_allow_other in /etc/fuse.conf. We keep the default Owner ACL; the
    // BackgroundSession unmounts on drop, and the foreground mount unmounts on
    // exit. (Reconsider AutoUnmount + AllowOther for the long-running daemon.)
    cfg.mount_options = vec![MountOption::FSName("trove".to_string())];
    cfg
}

/// Mount in the background; the returned session unmounts on drop. For tests.
pub fn spawn(fs: Fs, registry: Registry, mountpoint: &Path) -> io::Result<BackgroundSession> {
    fuser::spawn_mount2(TroveFs::new(fs, registry), mountpoint, &config())
}

/// Mount in the foreground; blocks until unmounted. For the `trove mount` CLI.
pub fn mount_blocking(fs: Fs, registry: Registry, mountpoint: &Path) -> io::Result<()> {
    fuser::mount2(TroveFs::new(fs, registry), mountpoint, &config())
}
