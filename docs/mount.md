# `mount.rs` — the FUSE projection

**~1,125 lines.** The largest module. The FUSE filesystem that turns a
JuiceFS volume into a "filesystem that talks back" by routing writes
through a validation gate.

If you only read one module, read this one. It's where the system's
*shape* lives — every piece of plumbing in the codebase converges here.

## The mental model

FUSE is a kernel protocol for "filesystems written in userspace". The
kernel calls into your handler functions (`lookup`, `getattr`, `read`,
`write`, `flush`, `release`, …) and you tell it what each one does.

Trove implements `Filesystem` for `TroveFs`, which wraps an `Inner` behind a
`Mutex` (fuser 0.17 calls handlers through `&self`). `Inner` owns:

```rust
struct Inner {
    fs: Fs,                                // libjfs handle
    registry: Registry,                    // schema registry
    versions: Option<VersionStore>,        // version DB (or None = no versioning)
    embed_tx: Option<Sender<(String, Vec<u8>)>>,  // embed worker channel
    ino_to_path: HashMap<u64, String>,     // FUSE addresses by inode, JuiceFS by path
    path_to_ino: HashMap<String, u64>,
    open_files: HashMap<u64, OpenFile>,    // per-handle state
    …
}
```

## Three kinds of open file

A `read` request and a 5 GB binary `write` request cost very different
things. The dispatch happens at `open`/`create`:

```rust
enum OpenFile {
    Read       { reader: File },
    PassThrough { path: String, writer: File },
    Write      { path: String, buf: Vec<u8>, dirty: bool, rejected: bool },
}
```

- **`Read`** — read-only opens. Stream from JuiceFS, no buffering, no
  validation, fully coherent with concurrent writers.
- **`PassThrough`** — writable opens for files no schema can possibly
  claim (decided by `registry.may_govern()`). Writes stream straight to
  JuiceFS — nothing to validate, no need to buffer.
- **`Write`** — writable opens for governed files. The whole file is
  buffered in `buf` so it can be validated *as a unit* at the commit
  barrier. `dirty` = uncommitted; `rejected` = last commit attempt failed.

This tri-state is what makes the "buffer everything for validation" model
affordable. Binary blobs and vendored dirs cost the streaming path; only
files a schema actually governs pay the buffering price.

## The commit barrier

FUSE's `flush` (called on every `close()` of a writable handle) and
`fsync` both route through one function:

```rust
fn barrier(&mut self, fh: u64) -> Result<(), Errno>;
```

`barrier` dispatches:

- **`PassThrough`** → flush + fsync the streaming writer, then version the
  now-durable bytes (best-effort).
- **`Write`** → call `commit`, which is where the validation gate lives.

`commit()` is the heart of the system. In ~50 lines:

1. Pull the dirty buffer from the handle.
2. Call `validate(path, &buf)`:
   - parse frontmatter
   - select schemas
   - run `validate_against`
3. **If valid**:
   - `fs.write_all(path, &buf)` — atomically write the file
   - unlink any stale `.errors` sidecar
   - `record_version` — COW-clone into `/.trove/versions/<hash>`, append
     a chain row (best-effort; never fails the write)
   - if `embed_tx` is set, push `(hash, buf)` to the embed thread
     (non-blocking)
   - mark the handle clean
   - return `Ok(())`
4. **If invalid**:
   - write the violation report to `<path>.errors`
   - mark the handle `rejected: true`
   - return `Err(EINVAL)`

The `flush`/`fsync` syscall that triggered all this now surfaces `EINVAL`
to the agent. The phantom file (which has *never* been written to JuiceFS
— it only existed in the handle's `buf`) is invisible to subsequent
`lookup` calls because `rejected = true` makes `inflight_size` return None.

## "Talks back" via the `.errors` sidecar

A rejected commit writes:

```
~/vault/people/alice.md.errors
```

```
/dob: "not-a-date" is not of type "string" matching format "date"
(root): "name" is a required property
```

The agent reads this with a plain `cat alice.md.errors`. It's a *normal
file* — the validation feedback travels back through the same read path
the agent already uses. **No MCP, no SDK, no schema endpoint.**

When a subsequent valid commit lands, the sidecar is `unlink`'d. A clean
file has no `.errors` next to it.

## Inode ↔ path mapping

FUSE addresses files by **inode** (`u64`); JuiceFS addresses them by
**path** (`/people/alice.md`). Every handler that takes an `ino` first
resolves it to a path via `path_of(ino)`. Every newly-seen path gets a
fresh inode via `intern(path)`.

The interesting case is **`rename`**. The kernel keeps using the source's
inode for the destination (POSIX semantics), so `rename_path(old, new)`
preserves the inode and updates the maps. If we forgot this, the next op
on the renamed file would fail to resolve a path and return ENOENT.

## Phantom-file handling

A `create` returns a handle for a file that **doesn't exist in JuiceFS
yet** — its bytes are only in `buf`. `lookup` and `getattr` for that
path go to JuiceFS first, fall back to `inflight_size(path)` if JuiceFS
says ENOENT:

```rust
fn inflight_size(&self, path: &str) -> Option<u64>;
```

This searches open writable buffers for one that matches and isn't
rejected. The kernel sees a regular file of size `buf.len()` and is
satisfied. Reading the phantom (before the commit) is left as an exercise
for the agent — by convention, agents `close()` to commit and `open()`
again to read.

**The crucial subtlety**: a rejected buffer (`rejected: true`) is skipped
in `inflight_size`. So a failed commit makes the file *immediately
disappear* from `lookup`, even before FUSE's async `release` drops the
handle. Without this, a `ls` after a failed write would briefly show a
phantom that doesn't exist anywhere.

## The `NO_CACHE` TTL trick

```rust
const NO_CACHE: Duration = Duration::ZERO;
```

A freshly-`create`d file's entry reply uses `NO_CACHE` so the kernel
**doesn't cache a positive dentry** for it. Without that, a rejected write
would leave a stale "this file exists!" cached entry that survives until
TTL expires. Returning a zero TTL forces a re-lookup, which finds the file
gone.

## Why one big lock?

`Inner` lives behind a single `Mutex`. Every handler grabs it. This sounds
heavy until you realise:

- FUSE is one request at a time per mount anyway (kernel-side serialisation
  per-inode is the norm).
- libjfs's caching means the hot path doesn't actually wait on Postgres
  per call.
- The mount is single-process, single-tenant. Lock contention is real but
  not catastrophic.

A future perf pass might shard the lock, but the simpler invariant has
caught a class of bugs that finer-grained locking would have hidden.

## Binary detection (`sniff_binary`)

When a `PassThrough` file is versioned, we want to *not* try to embed it
as text. We sniff the first 8 KiB:

```rust
fn sniff_binary(fs: &Fs, path: &str) -> bool {
    // contains a NUL byte in the first 8 KiB → binary
}
```

UTF-8 text never has NUL; real binaries almost always do early. The split
only affects *how* a file is versioned (stream-hash vs read-back), never
*whether* it's versioned. All files get history.

## Concurrency tests

`tests/jfs.rs` proves the libjfs FFI handles parallel ops on distinct
files safely. `tests/mount.rs` puts the whole FUSE stack through the
kernel: read, write, the validation gate, the version-on-commit
self-trigger, the embed channel. Run those if you change anything here.

## What handlers omit

- **No `xattr` handlers** — not implemented (libjfs supports it; we don't
  need it).
- **No `fallocate`** — not surfaced.
- **No locking** — FUSE locks are advisory; the kernel's own per-inode
  locking is enough for v0.1.

## Read this next

The three pipelines that converge here, in order of complexity:

1. [The write pipeline](/docs/write-pipeline) — what one `write()` actually does
2. [How COW versioning works](/docs/cow-versions) — the `record_version` call
3. [Self-triggering embeddings](/docs/embedding-pipeline) — the `embed_tx`
   send

Next module: [`versioning.rs` — COW snapshots →](/docs/versioning)
