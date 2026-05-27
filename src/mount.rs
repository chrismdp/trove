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

use crate::jfs::{File, FileInfo, Fs};
use crate::types::Registry;
use fuser::{
    BackgroundSession, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    RenameFlags, Request, TimeOrNow, WriteFlags,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);
/// A freshly `create`d file exists only in its handle buffer, not yet in jfs.
/// Replying with a zero TTL stops the kernel caching a *positive* dentry for it,
/// so a rejected write leaves no phantom: the next access re-looks-up and misses.
const NO_CACHE: Duration = Duration::ZERO;
const ROOT_INO: u64 = 1;

/// An open file handle. Reads and writes take different routes so we only pay
/// the buffering cost (and the validation gate) when something is actually being
/// written — the read-heavy path stays a cheap, coherent pass-through.
enum OpenFile {
    /// Read-only: stream straight from jfs. No buffering, no validation, full
    /// coherence with concurrent writers.
    Read { reader: File },
    /// Ungoverned writable file (binary, or any path no schema can claim):
    /// writes stream straight to jfs. No buffering — keeps large/binary files
    /// cheap — and nothing to validate, so no gate.
    PassThrough { writer: File },
    /// Governed writable file: the whole proposed file is buffered so it can be
    /// validated as a unit at the commit barrier. `dirty` means uncommitted
    /// changes (or a fresh create / truncate) are pending. `rejected` means the
    /// last commit attempt failed validation — the buffer won't persist, so it's
    /// hidden from `lookup`/`getattr` immediately (FUSE `release` is async, so we
    /// can't wait for the handle to drop to stop reporting a phantom).
    Write {
        path: String,
        buf: Vec<u8>,
        dirty: bool,
        rejected: bool,
    },
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

    /// Remap `old`'s inode to `new` after a rename. The kernel keeps using the
    /// source's inode for the destination, so we must preserve that inode→path
    /// mapping (POSIX semantics) — forgetting it would make the next op on the
    /// renamed file fail to resolve a path. Any inode `new` previously held is
    /// dropped.
    fn rename_path(&mut self, old: &str, new: &str) {
        match self.path_to_ino.remove(old) {
            Some(ino) => {
                self.ino_to_path.insert(ino, new.to_string());
                if let Some(stale) = self.path_to_ino.insert(new.to_string(), ino) {
                    if stale != ino {
                        self.ino_to_path.remove(&stale);
                    }
                }
            }
            None => self.forget_path(new),
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
        self.open_files.values().find_map(|of| match of {
            // A rejected buffer won't persist, so it must not look like it exists.
            OpenFile::Write { path: p, buf, dirty, rejected }
                if *dirty && !*rejected && p == path =>
            {
                Some(buf.len() as u64)
            }
            _ => None,
        })
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
            Some(OpenFile::Write { path, buf, dirty: true, .. }) => (path.clone(), buf.clone()),
            // read / pass-through handle, or a clean write handle: nothing to commit
            Some(_) => return Ok(()),
            None => return Err(Errno::EBADF),
        };

        match self.validate(&path, &buf) {
            Ok(()) => {
                self.fs.write_all(&path, &buf, 0o644).map_err(|_| Errno::EIO)?;
                let _ = self.fs.unlink(&format!("{path}.errors")); // clear stale sidecar
                if let Some(OpenFile::Write { dirty, rejected, .. }) = self.open_files.get_mut(&fh) {
                    *dirty = false;
                    *rejected = false;
                }
                Ok(())
            }
            Err(report) => {
                let _ = self
                    .fs
                    .write_all(&format!("{path}.errors"), report.as_bytes(), 0o644);
                // Mark rejected so the phantom file disappears immediately, even
                // before the async `release` drops the handle.
                if let Some(OpenFile::Write { rejected, .. }) = self.open_files.get_mut(&fh) {
                    *rejected = true;
                }
                Err(Errno::EINVAL)
            }
        }
    }

    /// The commit barrier (`flush`/`fsync`): pass-through handles sync their jfs
    /// writer directly; governed handles validate + commit their buffer.
    fn barrier(&mut self, fh: u64) -> Result<(), Errno> {
        if let Some(OpenFile::PassThrough { writer }) = self.open_files.get(&fh) {
            return writer
                .flush()
                .and_then(|_| writer.fsync())
                .map_err(|_| Errno::EIO);
        }
        self.commit(fh)
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

/// Epoch milliseconds for a FUSE time-set request (`libjfs` wants millis).
fn to_millis(t: TimeOrNow) -> i64 {
    let st = match t {
        TimeOrNow::SpecificTime(s) => s,
        TimeOrNow::Now => SystemTime::now(),
    };
    st.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
        } else if fi.is_symlink() {
            FileType::Symlink
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
        // lstat, not stat: report a symlink as a symlink (the kernel follows it).
        match g.fs.lstat(&path) {
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
        match g.fs.lstat(&path) {
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
        let rel = Path::new(path.strip_prefix('/').unwrap_or(&path));
        if !g.registry.may_govern(rel) {
            // Ungoverned (binary, or a path no schema can claim): create in jfs
            // now and stream writes straight through. Normal TTL — it persists.
            match g.fs.create(&path, (mode & 0o7777) as u16) {
                Ok(writer) => {
                    let ino = g.intern(&path);
                    let fi = g.fs.stat(&path).unwrap_or_default();
                    let fh = g.new_fh(OpenFile::PassThrough { writer });
                    reply.created(
                        &TTL,
                        &to_attr(ino, &fi),
                        Generation(0),
                        FileHandle(fh),
                        FopenFlags::empty(),
                    );
                }
                Err(_) => reply.error(Errno::EIO),
            }
            return;
        }

        // Governed: nothing touches jfs yet — the file lives only in its buffer
        // until it validates at the commit barrier. `mode` is advisory for now
        // (the committed file is written 0o644). NO_CACHE TTL so a rejected
        // create leaves no phantom dentry.
        let ino = g.intern(&path);
        let fh = g.new_fh(OpenFile::Write {
            path,
            buf: Vec::new(),
            dirty: true,
            rejected: false,
        });
        reply.created(
            &NO_CACHE,
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

        // Read-only opens stream straight from jfs — no buffering, full
        // coherence. So do writable opens of ungoverned (e.g. binary) files:
        // there's nothing to validate, so no reason to buffer them.
        let writable = flags.0 & libc::O_ACCMODE != libc::O_RDONLY;
        let rel = Path::new(path.strip_prefix('/').unwrap_or(&path));
        if !writable {
            match g.fs.open(&path, flags.0) {
                Ok(reader) => {
                    let fh = g.new_fh(OpenFile::Read { reader });
                    reply.opened(FileHandle(fh), FopenFlags::empty());
                }
                Err(_) => reply.error(Errno::ENOENT),
            }
            return;
        }
        if !g.registry.may_govern(rel) {
            match g.fs.open(&path, flags.0) {
                Ok(writer) => {
                    let fh = g.new_fh(OpenFile::PassThrough { writer });
                    reply.opened(FileHandle(fh), FopenFlags::empty());
                }
                Err(_) => reply.error(Errno::ENOENT),
            }
            return;
        }

        // Governed writable: buffer current contents so read-modify-write works
        // and the validator sees the whole file at commit. O_TRUNC starts empty.
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
        let fh = g.new_fh(OpenFile::Write {
            path,
            buf,
            dirty: truncating, // truncation is itself a pending change
            rejected: false,
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
        match g.open_files.get(&fh.0) {
            // Read / pass-through handle: pread straight from jfs.
            Some(OpenFile::Read { reader } | OpenFile::PassThrough { writer: reader }) => {
                let mut tmp = vec![0u8; size as usize];
                match reader.read_at(&mut tmp, offset as i64) {
                    Ok(n) => {
                        tmp.truncate(n);
                        reply.data(&tmp);
                    }
                    Err(_) => reply.error(Errno::EIO),
                }
            }
            // Write handle: serve from the in-memory buffer (this fd's view).
            Some(OpenFile::Write { buf, .. }) => {
                let start = offset as usize;
                if start >= buf.len() {
                    reply.data(&[]);
                    return;
                }
                let end = (start + size as usize).min(buf.len());
                reply.data(&buf[start..end]);
            }
            None => reply.error(Errno::EBADF),
        }
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
        match g.open_files.get_mut(&fh.0) {
            // Governed: splice into the buffer; commit/validate happens later.
            Some(OpenFile::Write { buf, dirty, rejected, .. }) => {
                let start = offset as usize;
                let end = start + data.len();
                if buf.len() < end {
                    buf.resize(end, 0);
                }
                buf[start..end].copy_from_slice(data);
                *dirty = true;
                *rejected = false; // fresh bytes — give it another chance at commit
                reply.written(data.len() as u32);
            }
            // Ungoverned: write straight through to jfs.
            Some(OpenFile::PassThrough { writer }) => match writer.write_at(data, offset as i64) {
                Ok(n) => reply.written(n as u32),
                Err(_) => reply.error(Errno::EIO),
            },
            // A write on a read-only handle: the kernel shouldn't issue this,
            // but report the standard error rather than corrupting anything.
            Some(OpenFile::Read { .. }) => reply.error(Errno::EBADF),
            None => reply.error(Errno::EBADF),
        }
    }

    /// `flush` runs on every `close()`; its return value reaches the closing
    /// syscall, so this is where a rejection becomes visible. Committing here
    /// means a plain `cp`/editor-save persists (or is rejected) on close.
    fn flush(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _lock: LockOwner, reply: ReplyEmpty) {
        let mut g = self.inner.lock().unwrap();
        match g.barrier(fh.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let mut g = self.inner.lock().unwrap();
        match g.barrier(fh.0) {
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

    /// List a directory: `.`, `..`, then the real entries from jfs. `offset` is
    /// a resume cookie — the kernel re-calls with the last value we handed back
    /// when its buffer fills, so we emit `(index + 1)` as each entry's cookie.
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let mut g = self.inner.lock().unwrap();
        let Some(path) = g.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let des = match g.fs.readdir(&path) {
            Ok(d) => d,
            Err(_) => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // Build the full listing first (interning child inodes), then emit from
        // `offset`. `..` reuses this inode since we don't track parents — benign.
        let mut listing: Vec<(u64, FileType, String)> = Vec::with_capacity(des.len() + 2);
        listing.push((ino.0, FileType::Directory, ".".to_string()));
        listing.push((ino.0, FileType::Directory, "..".to_string()));
        for d in des {
            let kind = if d.is_dir() {
                FileType::Directory
            } else if d.is_symlink() {
                FileType::Symlink
            } else {
                FileType::RegularFile
            };
            let child = if path == "/" {
                format!("/{}", d.name)
            } else {
                format!("{}/{}", path, d.name)
            };
            let child_ino = g.intern(&child);
            listing.push((child_ino, kind, d.name));
        }

        for (i, (e_ino, kind, name)) in listing.iter().enumerate().skip(offset as usize) {
            // add() returns true when the reply buffer is full.
            if reply.add(INodeNo(*e_ino), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    /// Rename within the volume. **This is a gate boundary:** if the destination
    /// is a governed path, the *moved content* is validated against the
    /// destination's schema before the move — otherwise atomic-save-via-rename
    /// (vim, `sed -i`, …) would smuggle invalid content past the write gate.
    /// Invalid → the rename is rejected (`EINVAL`) and a `.errors` sidecar is
    /// written at the destination; the source is left untouched.
    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let mut g = self.inner.lock().unwrap();
        let (Some(oldpath), Some(newpath)) =
            (g.child_path(parent.0, name), g.child_path(newparent.0, newname))
        else {
            reply.error(Errno::EINVAL);
            return;
        };

        let new_rel = Path::new(newpath.strip_prefix('/').unwrap_or(&newpath));
        if g.registry.may_govern(new_rel) {
            // Validate the bytes being moved against the destination schema.
            let content = match g.fs.read_all(&oldpath) {
                Ok(c) => c,
                Err(_) => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            if let Err(report) = g.validate(&newpath, &content) {
                let _ = g
                    .fs
                    .write_all(&format!("{newpath}.errors"), report.as_bytes(), 0o644);
                reply.error(Errno::EINVAL);
                return;
            }
        }

        match g.fs.rename(&oldpath, &newpath) {
            Ok(()) => {
                // Preserve the source inode at the destination (the kernel keeps
                // using it), then clear any stale sidecar at the destination.
                g.rename_path(&oldpath, &newpath);
                let _ = g.fs.unlink(&format!("{newpath}.errors"));
                reply.ok();
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    /// Apply attribute changes by delegating to `jfs` — `chmod`, `chown`,
    /// `utimens`, and `truncate`. The one gate concern: **`truncate` on a
    /// governed file is a write in disguise**, so the resulting (shrunk/extended)
    /// content is validated first; invalid → `EINVAL` + `.errors` sidecar, no
    /// change. Everything else is a faithful pass-through.
    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
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

        if let Some(mode) = mode {
            if g.fs.chmod(&path, mode & 0o7777).is_err() {
                reply.error(Errno::EIO);
                return;
            }
        }
        if uid.is_some() || gid.is_some() {
            // jfs_chown sets both; fill the unspecified half from current attrs.
            let cur = g.fs.lstat(&path).ok();
            let u = uid.or(cur.map(|f| f.uid)).unwrap_or(0);
            let gg = gid.or(cur.map(|f| f.gid)).unwrap_or(0);
            let _ = g.fs.chown(&path, u, gg);
        }
        if atime.is_some() || mtime.is_some() {
            let m = mtime.map(to_millis).unwrap_or(-1);
            let a = atime.map(to_millis).unwrap_or(-1);
            let _ = g.fs.utime(&path, m, a);
        }
        if let Some(new_len) = size {
            let rel = Path::new(path.strip_prefix('/').unwrap_or(&path));
            if g.registry.may_govern(rel) {
                let mut content = g.fs.read_all(&path).unwrap_or_default();
                content.resize(new_len as usize, 0); // shrink or zero-extend
                if let Err(report) = g.validate(&path, &content) {
                    let _ = g
                        .fs
                        .write_all(&format!("{path}.errors"), report.as_bytes(), 0o644);
                    reply.error(Errno::EINVAL);
                    return;
                }
            }
            if g.fs.truncate(&path, new_len).is_err() {
                reply.error(Errno::EIO);
                return;
            }
        }

        match g.fs.lstat(&path) {
            Ok(fi) => reply.attr(&TTL, &to_attr(ino.0, &fi)),
            Err(_) => match g.inflight_size(&path) {
                Some(sz) => reply.attr(&TTL, &synth_attr(ino.0, sz)),
                None => reply.error(Errno::ENOENT),
            },
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut g = self.inner.lock().unwrap();
        let Some(path) = g.child_path(parent.0, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        match g.fs.rmdir(&path) {
            Ok(()) => {
                g.forget_path(&path);
                reply.ok();
            }
            // Likely ENOTEMPTY or ENOENT; errno fidelity is a follow-up (the
            // jfs wrapper currently flattens the code).
            Err(_) => reply.error(Errno::ENOTEMPTY),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let mut g = self.inner.lock().unwrap();
        let (Some(link), Some(target)) = (g.child_path(parent.0, link_name), target.to_str())
        else {
            reply.error(Errno::EINVAL);
            return;
        };
        match g.fs.symlink(target, &link) {
            Ok(()) => {
                let ino = g.intern(&link);
                // lstat so the new entry is reported as a symlink, not its target.
                match g.fs.lstat(&link) {
                    Ok(fi) => reply.entry(&TTL, &to_attr(ino, &fi), Generation(0)),
                    Err(_) => reply.error(Errno::EIO),
                }
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let g = self.inner.lock().unwrap();
        let Some(path) = g.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match g.fs.readlink(&path) {
            Ok(target) => reply.data(target.as_bytes()),
            Err(_) => reply.error(Errno::EINVAL),
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let mut g = self.inner.lock().unwrap();
        let (Some(src), Some(dst)) = (g.path_of(ino.0), g.child_path(newparent.0, newname)) else {
            reply.error(Errno::EINVAL);
            return;
        };
        match g.fs.link(&src, &dst) {
            Ok(()) => {
                let new_ino = g.intern(&dst);
                match g.fs.lstat(&dst) {
                    Ok(fi) => reply.entry(&TTL, &to_attr(new_ino, &fi), Generation(0)),
                    Err(_) => reply.error(Errno::EIO),
                }
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let g = self.inner.lock().unwrap();
        let (total, avail) = g.fs.statvfs().unwrap_or((0, 0));
        const BSIZE: u64 = 4096;
        let blocks = total / BSIZE;
        let free = avail / BSIZE;
        // files/ffree are unknown for an object-backed FS — report 0.
        reply.statfs(blocks, free, free, 0, 0, BSIZE as u32, 255, BSIZE as u32);
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
