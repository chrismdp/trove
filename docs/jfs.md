# `jfs.rs` — libjfs FFI

**~430 lines.** A safe Rust wrapper over JuiceFS's `libjfs` C ABI.

This is the module that earns Trove its "filesystem-shaped" promise. It
embeds a *full distributed filesystem* — R2 blobs + Postgres metadata,
strong consistency, caching — **in-process**. Nobody runs `juicefs mount`.
The `trove` binary *is* the JuiceFS client.

## Why FFI rather than the `juicefs` binary?

Two options would have been simpler:

1. `Command::new("juicefs").arg("mount").spawn()` — let JuiceFS do its own
   FUSE mount; Trove just lives in the same directory.
2. Use the gRPC/Thrift sidecar JuiceFS exposes.

Both were rejected:

- **The subprocess approach** means a separate FUSE process kernel, with no
  way for Trove to intercept writes. The validation gate would have to be a
  separate FUSE layer on top — two FUSE mounts stacked, with all the
  context-switch overhead that implies.
- **The sidecar approach** loses the in-process performance and introduces
  another moving part.

FFI gives us **one process**, one binary, one cache, and direct access to
`jfs_clone` (the COW primitive that powers versioning).

## The interface, in one screen

The `extern "C"` block has ~25 functions. They divide cleanly:

- **Lifecycle**: `jfs_init`
- **File I/O**: `jfs_create`, `jfs_open`, `jfs_pread`, `jfs_pwrite`,
  `jfs_flush`, `jfs_fsync`, `jfs_close`
- **Metadata**: `jfs_stat`, `jfs_lstat`, `jfs_chmod`, `jfs_chown`,
  `jfs_utime`, `jfs_truncate`, `jfs_access`, `jfs_statvfs`
- **Directories**: `jfs_mkdir`, `jfs_rmdir`, `jfs_listdir2`
- **Names**: `jfs_rename`, `jfs_unlink`, `jfs_symlink`, `jfs_readlink`,
  `jfs_link`
- **The COW primitive**: `jfs_clone`

The Rust wrapper exposes a tidy `Fs` struct with idiomatic methods (`read_all`,
`write_all`, `clone_file`, etc.) plus a `File` handle type.

## The libjfs error convention

> A non-negative return is success (a file handle, a byte count, or 0). A
> negative return is `-errno`.

Every wrapper goes through:

```rust
fn check(ret: c_int, op: &str) -> Result<c_int> {
    if ret < 0 { bail!("{op}: errno {}", -ret); }
    Ok(ret)
}
```

So a `read_at` returning `-2` (ENOENT) becomes an `anyhow` error
`pread: errno 2`.

## `jfs_init`: the gotcha

libjfs's init wants a JSON config blob, and **certain fields must not be
empty** or it panics the whole Go runtime in `ParseBytesStr`/`ParseMbpsStr`.
We learned this in the spike; the cure is to always pass values:

```rust
let conf = serde_json::json!({
    "meta":          meta,
    "cacheDir":      cache_dir,
    "cacheSize":     "1024",
    "memorySize":    "300",
    "readahead":     "0",
    "uploadLimit":   "0",
    "downloadLimit": "0",
    "autoCreate":    true,
    "noUsageReport": true,
    "caller":        1,
}).to_string();
```

If you ever change this, **test with all the rate-limit fields stringified
to "0"**, never omitted. A naive caller passing `null` here will produce a
process crash with no Rust-side stack.

## `jfs_clone`: where versioning comes from

```rust
pub fn clone_file(&self, src: &str, dst: &str, preserve: bool) -> Result<()>;
```

This is the single most important function in the file. A `jfs_clone` is a
**metadata-only copy**: the destination shares the source's data blocks via
a refcount bump. **Zero bytes are copied.** Overwriting the source later
preserves the clone's old blocks (true copy-on-write).

History accumulates without byte duplication. N versions of a file = N
clones sharing whatever blocks they have in common. The version archive at
`/.trove/versions/<hash>` is, in storage terms, almost free.

See [How COW versioning works](/docs/cow-versions) for what happens around
this call.

## `jfs_pread` / `jfs_pwrite` are positional

There's no implicit file pointer. Every read and write takes an offset.
This matches FUSE's model (`read(off, size)`, `write(off, buf)`), so the
mount layer can pass through positions cleanly without an explicit
`seek` call.

`read_all` and `write_all` are convenience wrappers built on top, looping
until the whole buffer is consumed.

## `parse_listdir2`: hand-parsed binary format

`jfs_listdir2` returns a malloc'd buffer. Each entry is **big-endian**:

```
u16 name_len, name bytes (name_len), 44 bytes of stat block (starts with u32 mode)
```

We parse this in `parse_listdir2`, copy what we need (`name` and `mode`),
and `libc::free` the buffer. The 44-byte stat block lays out as
`mode(4)+inode(8)+nlink(4)+uid(4)+gid(4)+length(8)+atime+mtime+ctime(4*3)` —
we read only `mode` for FUSE `readdir`; the rest is fetched per-entry via
`getattr` if the kernel asks.

## Drop order: `File` borrows `Fs`

A `File` carries a `c_int` fd. When it drops, it calls `jfs_close`. The
borrow checker prevents you from holding a `File` after its `Fs` has
dropped, because `File::open` returns `Result<File>` not `Result<File<'_>>`
— wait, that's a fib: we *don't* lifetime-bind `File` to `Fs` in the type.
We rely on the mount layer owning both together in `Inner` and dropping them
in the right order. **Do not return a `File` past the lifetime of its
`Fs`.** The borrow check won't catch you.

## What `Fs` doesn't expose

- **xattrs**: JuiceFS supports them; we don't need them for v0.1, so the
  FFI is omitted.
- **Locking** (`flock`, `fcntl`): same. The mount serialises per-inode
  itself.
- **Block-level extended ops**: `fallocate`, `copy_file_range`, etc. —
  not surfaced.

If you want to add one, the pattern is uniform: declare in the `extern
"C"` block, add a safe wrapper that goes through `check()`.

## The build link

`build.rs` is what actually finds `libjfs-amd64.so`:

```rust
let dir = std::env::var("LIBJFS_DIR")
    .unwrap_or_else(|_| "/home/cp/code/trove/spike/juicefs/sdk/java/libjfs".to_string());
println!("cargo:rustc-link-search=native={dir}");
println!("cargo:rustc-link-lib=dylib=jfs-amd64");
println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
```

Override `LIBJFS_DIR` in CI. The `rpath` line means binaries find the `.so`
at runtime without `LD_LIBRARY_PATH` — the binary is self-locating.

Next: [`mount.rs` — the FUSE projection →](/docs/mount)
