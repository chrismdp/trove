//! `trove mount` — a FUSE filesystem backed by JuiceFS (via libjfs, in-process).
//!
//! This increment is a faithful pass-through: kernel file ops delegate straight
//! to `jfs`. The next increment adds the validation gate on the write path
//! (`release`/`fsync`) — the "filesystem that talks back". FUSE addresses files
//! by inode; JuiceFS by path, so we keep an inode↔path map.
//!
//! fuser 0.17 calls handlers through `&self`, so all mutable state lives behind
//! a `Mutex`. Inode/handle ids and flags are newtypes (`INodeNo`, `FileHandle`,
//! `OpenFlags`, …) — we convert at the boundary.

use crate::jfs::{File, FileInfo, Fs};
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

struct Inner {
    fs: Fs,
    ino_to_path: HashMap<u64, String>,
    path_to_ino: HashMap<String, u64>,
    next_ino: u64,
    open_files: HashMap<u64, File>,
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

    fn new_fh(&mut self, file: File) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.open_files.insert(fh, file);
        fh
    }
}

pub struct TroveFs {
    inner: Mutex<Inner>,
}

impl TroveFs {
    pub fn new(fs: Fs) -> Self {
        let mut ino_to_path = HashMap::new();
        let mut path_to_ino = HashMap::new();
        ino_to_path.insert(ROOT_INO, "/".to_string());
        path_to_ino.insert("/".to_string(), ROOT_INO);
        TroveFs {
            inner: Mutex::new(Inner {
                fs,
                ino_to_path,
                path_to_ino,
                next_ino: 2,
                open_files: HashMap::new(),
                next_fh: 1,
            }),
        }
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
            Err(_) => reply.error(Errno::ENOENT),
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
            Err(_) => reply.error(Errno::ENOENT),
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
        match g.fs.create(&path, (mode & 0o7777) as u16) {
            Ok(file) => {
                let ino = g.intern(&path);
                let fi = g.fs.stat(&path).unwrap_or_default();
                let fh = g.new_fh(file);
                reply.created(&TTL, &to_attr(ino, &fi), Generation(0), FileHandle(fh), FopenFlags::empty());
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let mut g = self.inner.lock().unwrap();
        let Some(path) = g.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match g.fs.open(&path, flags.0) {
            Ok(file) => {
                let fh = g.new_fh(file);
                reply.opened(FileHandle(fh), FopenFlags::empty());
            }
            Err(_) => reply.error(Errno::ENOENT),
        }
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
        let Some(file) = g.open_files.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let mut buf = vec![0u8; size as usize];
        match file.read_at(&mut buf, offset as i64) {
            Ok(n) => {
                buf.truncate(n);
                reply.data(&buf);
            }
            Err(_) => reply.error(Errno::EIO),
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
        let g = self.inner.lock().unwrap();
        let Some(file) = g.open_files.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        match file.write_at(data, offset as i64) {
            Ok(n) => reply.written(n as u32),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _lock: LockOwner, reply: ReplyEmpty) {
        let g = self.inner.lock().unwrap();
        if let Some(file) = g.open_files.get(&fh.0) {
            let _ = file.flush();
        }
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let g = self.inner.lock().unwrap();
        if let Some(file) = g.open_files.get(&fh.0) {
            let _ = file.fsync();
        }
        reply.ok();
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
        g.open_files.remove(&fh.0); // drop -> jfs_close
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
            Err(_) => reply.error(Errno::ENOENT),
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
pub fn spawn(fs: Fs, mountpoint: &Path) -> io::Result<BackgroundSession> {
    fuser::spawn_mount2(TroveFs::new(fs), mountpoint, &config())
}

/// Mount in the foreground; blocks until unmounted. For the `trove mount` CLI.
pub fn mount_blocking(fs: Fs, mountpoint: &Path) -> io::Result<()> {
    fuser::mount2(TroveFs::new(fs), mountpoint, &config())
}
