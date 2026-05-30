# Packaging Trove

How Trove's `mount` build links to `libjfs`, the platform matrix it ships
across, and the release engineering needed to produce binaries for all
of them.

If you're just trying to build the validator (`trove check`), you don't
need any of this — the core crate has no native dependency. This page is
for the `--features mount` build (libjfs + FUSE).

## What libjfs is

`libjfs` is a Go-compiled shared library Trove embeds in-process to
drive the JuiceFS substrate: R2 blob storage + Postgres metadata, with
JuiceFS's caching and consistency model. The source is fetched from
upstream JuiceFS at a pinned SHA (see `JUICEFS_SHA` at the top of
`libjfs/build.sh`), patched with the trove-specific changes in
`libjfs/patches/`, and built into a single `.so`/`.dylib` per platform.
Trove calls into it via FFI from `src/jfs.rs` — `jfs_format` to format
a volume, `jfs_init` to open one, plus the POSIX-shaped
read/write/stat/clone surface that backs the FUSE mount (see
[jfs.rs — libjfs FFI](/docs/jfs)).

## The platform matrix

Trove targets three host triples for v1. Windows and Intel Macs are
out of scope.

| target triple              | libjfs filename       | rustc link name |
|----------------------------|-----------------------|-----------------|
| `x86_64-unknown-linux-gnu` | `libjfs-amd64.so`     | `jfs-amd64`     |
| `aarch64-unknown-linux-gnu`| `libjfs-arm64.so`     | `jfs-arm64`     |
| `aarch64-apple-darwin`     | `libjfs-arm64.dylib`  | `jfs-arm64`     |

**Why no Intel Mac?** The `macos-13` GitHub-hosted runner is being
deprecated, the Intel Mac population is small and shrinking (every Mac
sold since late 2020 is Apple Silicon), and supporting it adds a CI leg
that's increasingly fragile for marginal coverage. `build.rs` and
`install.sh` both fail loudly with a clear pointer when run on Intel
Mac. If your use case genuinely needs it, open an issue — the build is
straightforward; we just don't run it in CI.

The rustc link name is shared across OSes; the system linker resolves
the `.so` vs `.dylib` extension. `build.rs` checks the per-OS filename
exists at `LIBJFS_DIR` (default: `libjfs/build/`) and aborts with a
clear message if it doesn't. `src/jfs.rs` selects the matching `#[link]`
via `#[cfg_attr]` and `compile_error!`s on any other target.

## Building libjfs locally

Single command, all platforms:

```bash
./libjfs/build.sh
```

The script fetches JuiceFS at the pinned SHA (see `JUICEFS_SHA` at the
top of the script), applies the patches in `libjfs/patches/`, and runs
`make`. Output lands in `libjfs/build/`. Idempotent: re-running is a
no-op unless the SHA or patches change. Use `--force` to rebuild.

Prerequisites: `go` (>= 1.22) on PATH, `curl` (or `wget`/`git`), and the
host's C toolchain. On Linux for the host arch only.

### Pointing Trove at the result

`build.rs` defaults `LIBJFS_DIR` to `libjfs/build/`, so the standard
flow is:

```bash
./libjfs/build.sh
cargo build --release --features mount
```

If you've built libjfs somewhere else, point at it explicitly:

```bash
LIBJFS_DIR=/abs/path/to/libjfs cargo build --release --features mount
```

`build.rs` looks for the matching filename for the current target in
`LIBJFS_DIR` and adds it to the link search path with `-rpath` so the
binary finds it at runtime.

### Bumping the JuiceFS base

To track a newer upstream JuiceFS release:

1. Change `JUICEFS_SHA` at the top of `libjfs/build.sh`.
2. Re-run `./libjfs/build.sh --force`.
3. If a patch in `libjfs/patches/` no longer applies cleanly, fix it
   (regenerate from a working tree, or edit the hunks by hand) and
   re-run.
4. Commit the SHA bump and the updated patches together.
5. The rebuild regenerates `THIRD-PARTY-LICENSES.md` for the new
   dependency set. Watch the build output for any
   `collect-licenses: WARNING no license text for …` line — that means a
   newly-pulled module ships no license file. Add a curated note under
   `libjfs/license-notes/<module-path-with-slashes-as-underscores>.txt`
   (see the three already there) so the manifest stays complete.

### Cross-builds for other platforms

The build script builds for the host arch only — Linux amd64 from amd64,
arm64 from arm64, macOS native from native. Cross-builds are handled in
the release pipeline (`.github/workflows/release.yml`), which runs the
script on a per-platform runner from the matrix.

## Third-party licenses

`libjfs` is built from Apache-2.0 JuiceFS and statically links a large set of
third-party Go modules, so every binary release must carry their attribution
(Apache 2.0 §4 for JuiceFS; each module's own terms for the rest). `build.sh`
handles this automatically: after `make`, it concatenates
`libjfs/THIRD-PARTY-LICENSES.head.md` (JuiceFS attribution + Trove's
modification notice + the full Apache 2.0 text) with the output of
`libjfs/collect-licenses.sh`. That script walks the exact module set linked
into the build (`go list -deps -tags nogspt`) and pulls each module's license
from the Go module cache — no extra tooling, just `go` + a POSIX shell. The
result, `libjfs/build/THIRD-PARTY-LICENSES.md`, is copied into every release
tarball.

A few small transitive deps ship no license file in their pinned version;
those have curated notes under `libjfs/license-notes/`, which the collector
prefers over a cache lookup. If a JuiceFS bump pulls in a new such module the
build prints a `WARNING` naming it (see "Bumping the JuiceFS base" above).

Trove's own code is under the FSL — see [LICENSE.md](../LICENSE.md). The FSL
does not relicense the bundled JuiceFS or its patches; those stay Apache 2.0,
and recipients keep their Apache-2.0 rights to that component.

## Release process

Tagging a version triggers
[`.github/workflows/release.yml`](../.github/workflows/release.yml),
which builds across the four-runner matrix below and publishes a
GitHub Release with all four tarballs attached.

```bash
# From main, with everything green and Cargo.toml at the right version:
git tag v0.2.0
git push origin v0.2.0
```

The workflow:

1. Builds `libjfs` on each runner (`./libjfs/build.sh`, which fetches
   upstream JuiceFS at the pinned SHA, applies trove's patches, and
   runs `make`).
2. `cargo build --release --features mount` with
   `LIBJFS_DIR` pointing at `libjfs/build/`. The relocatable rpath
   (`$ORIGIN` on Linux, `@loader_path` on macOS) makes the binary
   find the dylib alongside it at runtime, so the shipped tarball
   needs no `LD_LIBRARY_PATH`.
3. **macOS only**: patches the dylib install name with
   `install_name_tool`. JuiceFS's upstream Makefile gives
   `libjfs-arm64.dylib` an install name = the bare filename, no
   `@rpath/` prefix. rustc copies that literal into trove's
   `LC_LOAD_DYLIB`, so at runtime dyld does absolute/CWD search and
   the `@loader_path` rpath never fires. The release step rewrites the
   dylib's id to `@rpath/libjfs-arm64.dylib`, changes trove's load
   command to match, and re-signs both binaries (codesign breaks on
   `install_name_tool` edits). Linux is unaffected — ELF DT_NEEDED
   resolves via `$ORIGIN` from the load name directly.
4. Tars `trove`, the matching `libjfs-<arch>.{so,dylib}`,
   `LICENSE.md`, `README.md` and `THIRD-PARTY-LICENSES.md` into
   `trove-<version>-<os>-<arch>.tar.gz`.
5. Computes `sha256` of the tarball, emits a sidecar `.sha256` file.
6. Uploads all six assets (3 tarballs + 3 sha256s) as a GitHub
   Release for the tag.

Users get sha256 verification automatically via the install script
([`install.sh`](../install.sh)), which downloads both the tarball and
the `.sha256` and runs `sha256sum -c` / `shasum -a 256 -c` before
extracting.

Each archive ships **five files**: the `trove` binary, the matching
`libjfs-*.so` / `libjfs-*.dylib`, `LICENSE.md`, `README.md` and
`THIRD-PARTY-LICENSES.md`. The
binary's rpath is `$ORIGIN` (Linux) / `@loader_path` (macOS), so the
loader finds libjfs alongside the binary — no `LD_LIBRARY_PATH` /
`DYLD_LIBRARY_PATH` needed as long as users keep them in the same
directory. The install script puts both under
`~/.local/share/trove/<version>/`.

## macOS runtime requirements

The macOS tarballs ship with `trove` linked against macFUSE's userland.
**All trove commands except `trove mount` / `trove init` work without macFUSE installed**
— `check`, `docs`, `search`, `server`, `log`, `cat`, `backup`
have no FUSE dependency at runtime.

For `trove mount`, you need macFUSE on the machine:

```bash
brew install --cask macfuse
```

macFUSE bundles a kernel extension which requires KEXT consent — a
one-time approval in **System Settings → Privacy & Security** after the
install, plus a restart. This is a fundamental Mac FUSE constraint, not
a Trove choice: macOS won't let any userland mount a FUSE filesystem
without an approved KEXT. (It's also why the macOS release builds in
CI install macFUSE's *userland* only, never the KEXT — kernel-extension
consent isn't possible on a headless runner, but the userland headers
and dylib are enough to link.)

The Linux tarballs use `libfuse3` instead and have no equivalent
approval step.

## The future state — single self-contained binary

Today every release archive is binary + dylib. The next iteration will
`include_bytes!` the right libjfs into the `trove` binary itself,
extract it to a per-user tmp path at first run, and `dlopen` it. That
collapses distribution to a single executable per platform.

Doing that requires moving `src/jfs.rs` off `#[link(name = ...)]` (which
binds at link time) to runtime `libloading::Library::new(...)`. Every
`extern "C"` declaration becomes a function-pointer field on a `Jfs`
struct populated once at `Fs::init`. It's a ~200-line refactor of
`jfs.rs` that doesn't change its public surface. Tracked separately —
not in this milestone.

The platform-aware `build.rs` + `#[cfg_attr]` link selection here is the
bridge state: it gets Trove to four platforms now, and leaves the
extern signatures unchanged so the runtime-load version can drop in
later without touching callers.
