# POSIX compatibility

Trove's FUSE mount aims to be **POSIX-shaped** — the one deliberate
deviation is the schema validation gate (a `close()` on a governed file can
return `EINVAL`). Everything else delegates to libjfs and behaves the way
the kernel and userspace tools expect.

This page is the honest list of what works, what doesn't, and why.

## What's supported

| Surface | Status | Notes |
|---|---|---|
| `open` / `close` / `read` / `write` | ✓ | positional `pread`/`pwrite` underneath; `O_APPEND` works |
| `creat` / `unlink` | ✓ | |
| `mkdir` / `rmdir` / `readdir` | ✓ | |
| `rename` | ✓ | within the volume only (no cross-device) |
| `stat` / `lstat` / `fstat` | ✓ | |
| `chmod` / `chown` | ✓ | mode preserved through validated writes (since 2026-05) |
| `utime` / `utimensat` | ✓ | millisecond resolution; libjfs's native granularity |
| `truncate` / `ftruncate` | ✓ | |
| `access` | ✓ | |
| `symlink` / `readlink` | ✓ | |
| `link` (hard links) | ✓ | within the volume |
| `statvfs` | ✓ | reports the JuiceFS volume's view of bucket usage |
| **Extended attributes** (`setxattr`/`getxattr`/`listxattr`/`removexattr`) | ✓ | stored natively in the metadata DB |
| **`flock(2)`** | ✓ | advisory, per-fd, whole-file |
| **`fcntl(2)` byte-range locks** (`F_SETLK`/`F_GETLK`) | ✓ | advisory, POSIX semantics |
| Sparse files / implicit holes | ✓ | via positional writes |
| `O_TRUNC` on `open` | ✓ | |
| `mtime` updates on write | ✓ | |
| Atomic rename (within volume) | ✓ | |

## What's intentionally different

| Surface | What happens | Why |
|---|---|---|
| `close()` / `fsync()` on a governed file with invalid frontmatter | returns `EINVAL`; nothing persists; a `<path>.errors` sidecar appears | This **is** the "filesystem that talks back" feature. See [The write pipeline](/docs/write-pipeline). |
| File mode on a freshly-committed governed file | set to the mode the agent's `open(O_CREAT, mode)` requested | A change from earlier versions which hardcoded `0o644`. Mode is preserved on every edit thereafter (truncate-in-place). |

## What's not supported (yet)

| Surface | What happens | Why / future |
|---|---|---|
| **POSIX ACLs** (`acl_set_file`, `getfacl`/`setfacl`) | `ENOTSUP` | JuiceFS has ACL support; we haven't surfaced it. Single-tenant + simple-mode usage doesn't need it; if a real use case appears it's a thin FFI addition. |
| **NFSv4 ACLs** | `ENOTSUP` | Same. |
| **`mmap` with `MAP_SHARED` writes** | reads work; shared writes are **buffered through the page cache and may not commit through the validation gate cleanly** | Mmap'd writes bypass our buffer-and-commit cycle. For governed files this can produce surprising behaviour. Safe to use `MAP_PRIVATE` (it's copy-on-write to the process). Use `read`/`write` for governed file edits. |
| **`fallocate(2)`** (including `FALLOC_FL_PUNCH_HOLE`) | not implemented; kernel falls back where it can | libjfs has `jfs_fallocate`; we haven't surfaced it. Low priority — markdown vaults don't need it. |
| **`copy_file_range(2)`** | kernel falls back to read + write | Not optimized. |
| **`SEEK_HOLE` / `SEEK_DATA`** (`lseek` extensions) | `EINVAL` | Files behave as if dense (holes exist but aren't reported). |
| **`O_DIRECT`** | flag is ignored | FUSE filesystems normally ignore O_DIRECT. The kernel does buffered I/O on top of our handlers. |
| **`F_SETLEASE`** (kernel file leases) | not implemented | Used by NFS delegation and a few specialty tools; not a typical vault need. |
| **Mandatory locks** | not supported | All locks are advisory (POSIX standard behaviour; mandatory locks are a Linux-specific kernel feature that's been deprecated). |
| **Quotas** (`quotactl`) | not implemented | JuiceFS supports per-directory quotas; we don't surface them. |
| **Per-file capabilities** (`xattr cap_set_file`) | xattrs are stored, but the *kernel* needs to recognise our FS as supporting file caps | The `security.capability` xattr persists, but the kernel may not honour it without specific filesystem flags. Treat as untested. |
| **inotify / fanotify on bind-mounts of trove paths** | unreliable | Direct watches on paths inside the trove mount usually work (via FUSE's `notify_inval_*` plumbing), but watching through a bind-mount has historically been spotty across FUSE filesystems. |

## Notable nuances

### Locks and the write buffer

trove's three handle kinds (`Read`, `PassThrough`, `Write`) carry different
state. Locks are inode-level (per-fd in the kernel, mapped to the
underlying jfs inode):

- **`Read`** and **`PassThrough`** hold a direct jfs fd; locks pass
  straight through to libjfs.
- **`Write`** is the governed buffered path. For an *existing* file being
  edited, the lock targets the existing inode (works). For a **freshly
  created** governed file that has not yet committed (its bytes live only
  in the handle's buffer), there is no underlying inode to lock; trove
  returns `ENOLCK`. In practice this matters only for editors that take an
  exclusive lock immediately after `O_CREAT` on a file that didn't exist —
  most editors take the lock against the file they read first, not the
  one they're creating from scratch.

### Cross-mount semantics

trove is single-tenant: one mount, one volume, one Postgres. Multiple
mounts of the *same* volume coordinate through libjfs's distributed
filesystem semantics (locks, atomic ops, cache invalidation all flow
through Postgres). Multi-mount embedding still uses an in-process channel,
so embeddings on writes from one mount aren't visible to another mount
until a `trove embed` sweep — see [Self-triggering embeddings](/docs/embedding-pipeline).

### What you can rely on from git, rsync, vim, etc.

The common tool surface works:

- `git init` / `git add` / `git commit` inside a trove mount — works.
  Git's internals (`.git/index`, packs) take the PassThrough path because
  no schema globs them; no validation overhead. Versioning *does* still
  fire on those files, so the version archive grows with every `git`
  invocation. `trove gc` (not built yet) is the eventual cleanup.
- `rsync` in or out — works. xattrs and mode now preserved.
- `vim` / `nvim` / `emacs` — work. They use atomic-write patterns
  (write-to-tmp + rename) which the validation gate handles correctly.
- `cp -a` — works; preserves mode + xattrs + (mostly) times.

### What changes from one mount option to another

Trove deliberately mounts with `default_permissions` (the kernel enforces
mode/uid/gid checks) and disables `allow_other` by default. If you
re-mount with different FUSE options the semantics here may shift; document
the deviation in your operator notes.

## Adding to this list

Found a POSIX operation that's important to you and silently fails or
behaves unexpectedly? Open an issue with:

- The exact syscall + arguments
- The expected behaviour (cite POSIX or Linux man page where possible)
- The actual behaviour (errno or unexpected result)
- What tool surfaced it

POSIX-compat regressions are taken seriously; gaps in the "not supported"
list above are tracked deliberately.
