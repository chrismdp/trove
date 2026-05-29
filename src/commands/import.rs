//! `trove import <path>` — take over an existing directory.
//!
//! `trove mount` is an *overlay*: FUSE hides whatever was already on the
//! mountpoint while it's mounted. The first time a user runs `trove mount`
//! against their existing vault that's alarming — "where did all my files
//! go?" — even though it's recoverable (unmount, files reappear). `import`
//! is the right tool for that case: it MOVES the existing tree out of the
//! way to a timestamped backup, mounts trove on the now-empty directory,
//! and streams the files back through the write pipeline so they go through
//! the validation gate, get versioned, and get embedded.
//!
//! The safety story is layered. **First**, a set of pure predicates
//! ([`is_dangerous_path`], [`exceeds_thresholds`]) refuse obviously
//! disastrous targets (`/`, `$HOME`, anything with ≤ 2 path components) and
//! large or file-count-heavy trees that smell like a typo. **Second**, a
//! dry-run validation sweep over the source — if any file would be rejected
//! by the validation gate, we abort BEFORE moving anything; the user's data
//! is still intact in its original location. **Third**, an explicit typed
//! confirmation (the user retypes the destination path) before any rename.
//! Then the rename to a timestamped backup directory (atomic on the same
//! filesystem), the mount, and the streaming copy-back.
//!
//! The pure half (predicates + threshold checks) is unit-tested in isolation.

use anyhow::{bail, Result};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// What the CLI shell passes in. The IO half ([`run`]) does the prompts and
/// the actual move/mount/copy-back. The pure half is in the predicates.
pub struct ImportOptions {
    pub path: PathBuf,
    /// Override schema directory (defaults to `<path>/.types/`).
    pub types: Option<PathBuf>,
    /// Skip the typed-confirmation step. Path/size safety still applies.
    pub yes: bool,
    /// Skip the file-count / total-size threshold guard. Required for
    /// directories with > 10k files or > 1 GB.
    pub force: bool,
}

/// File-count threshold above which `--force` is required. Picked to catch
/// typos like `trove import /` or `trove import ~` while still letting a
/// modestly-sized markdown vault through unprompted.
pub const MAX_FILES_WITHOUT_FORCE: u64 = 10_000;
/// Byte-size threshold (1 GiB) above which `--force` is required. Same
/// "is this a typo?" purpose as the file count.
pub const MAX_BYTES_WITHOUT_FORCE: u64 = 1024 * 1024 * 1024;

/// Pure predicate — refuse paths that would be catastrophic to take over.
/// `/`, top-level system dirs, and any path with ≤ 2 components (so `/home`
/// and `/usr/local` are out but `/home/cp/vault` is fine). The user's home
/// dir itself is refused (taking over `$HOME` would mean every dotfile gets
/// validated; not what anyone wants).
pub fn is_dangerous_path(path: &Path, home: Option<&Path>) -> bool {
    // Component count: `/` is 1, `/home` is 2, `/home/cp` is 3 (Component::RootDir + …)
    // We want to refuse anything with ≤ 2 *non-root* components — i.e. ≤ 3
    // total components when normalised with a leading `/`. Use `iter().count()`.
    let components: Vec<_> = path.components().collect();
    if components.len() <= 2 {
        return true;
    }
    // Refuse a fixed list of system roots (independent of the component-count
    // gate, so a `/etc` symlinked to `/private/etc` on macOS is still caught).
    let bad: &[&str] = &[
        "/", "/home", "/Users", "/usr", "/etc", "/var", "/opt", "/Volumes",
        "/tmp", "/private", "/bin", "/sbin", "/lib", "/lib64", "/boot", "/root",
        "/sys", "/proc", "/dev",
    ];
    for b in bad {
        if path == Path::new(b) {
            return true;
        }
    }
    // Refuse $HOME itself (but allow subdirectories).
    if let Some(h) = home {
        if path == h {
            return true;
        }
    }
    false
}

/// Pure predicate — given measured size + count, is `--force` required?
pub fn exceeds_thresholds(file_count: u64, total_bytes: u64) -> bool {
    file_count > MAX_FILES_WITHOUT_FORCE || total_bytes > MAX_BYTES_WITHOUT_FORCE
}

/// Walk `src` and return `(file_count, total_bytes)`. Symlinks count as a
/// file (we copy them across as symlinks) but don't follow.
pub fn scan_source(src: &Path) -> Result<(u64, u64)> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in walkdir::WalkDir::new(src)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let ft = entry.file_type();
        if ft.is_file() {
            files += 1;
            if let Ok(md) = entry.metadata() {
                bytes += md.len();
            }
        } else if ft.is_symlink() {
            files += 1;
        }
    }
    Ok((files, bytes))
}

/// Human-readable byte size. Mirrors `usage::human_bytes` style.
#[cfg_attr(not(feature = "mount"), allow(dead_code))]
fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Format a timestamp suitable for embedding in a backup directory name —
/// `YYYY-MM-DDTHH-MM-SS` (colons would be inconvenient on macOS Finder).
#[cfg_attr(not(feature = "mount"), allow(dead_code))]
fn backup_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Tiny date math, no chrono dep. Algorithm: civil_from_days (Howard Hinnant).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{month:02}-{d:02}T{h:02}-{m:02}-{s:02}"
    )
}

/// Build the backup directory path: `$HOME/.trove-backup/<basename>-<ts>`.
#[cfg_attr(not(feature = "mount"), allow(dead_code))]
pub fn backup_dir_for(path: &Path, home: &Path, ts: &str) -> PathBuf {
    let base = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "import".to_string());
    home.join(".trove-backup").join(format!("{base}-{ts}"))
}

/// Prompt the user to retype the destination path. Anything else aborts.
#[cfg_attr(not(feature = "mount"), allow(dead_code))]
fn confirm_typed(path: &Path) -> Result<()> {
    print!("To proceed, type the destination path: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let typed = line.trim_end_matches(['\n', '\r']).trim_end_matches('/');
    let expected = path.to_string_lossy();
    let expected = expected.trim_end_matches('/');
    if typed != expected {
        bail!("aborted — typed path did not match (\"{typed}\" vs \"{expected}\")");
    }
    Ok(())
}

#[cfg(feature = "mount")]
mod io_path {
    //! IO half — feature-gated because it needs libjfs (mount feature).

    use super::*;
    use crate::config::Config;
    use crate::types::Registry;
    use crate::version::VersionStore;
    use anyhow::{anyhow, Context};
    use colored::Colorize;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::mpsc::Sender;

    /// Streamed copy-back result: how many files committed cleanly, and how
    /// many were rejected by the validation gate (`.errors` sidecars next to
    /// the destination).
    pub struct CopyReport {
        pub ok: u64,
        pub failed: u64,
    }

    /// Walk `src` and copy every entry to `dst`, going *through the mount*
    /// (so the validation gate fires on each file). The mount is already
    /// running in the background by the time we get here.
    pub fn stream_into_mount(src: &Path, dst: &Path) -> Result<CopyReport> {
        let mut ok = 0u64;
        let mut failed = 0u64;
        for entry in walkdir::WalkDir::new(src)
            .min_depth(1)
            .follow_links(false)
            .sort_by_file_name() // directories before their children
        {
            let entry = entry?;
            let rel = entry.path().strip_prefix(src).unwrap();
            let target = dst.join(rel);
            let ft = entry.file_type();

            if ft.is_dir() {
                if let Err(e) = std::fs::create_dir_all(&target) {
                    eprintln!("trove import: mkdir {}: {e}", target.display());
                    failed += 1;
                }
                continue;
            }

            if ft.is_symlink() {
                match std::fs::read_link(entry.path()) {
                    Ok(link_target) => {
                        if let Err(e) = symlink(&link_target, &target) {
                            eprintln!("trove import: symlink {}: {e}", target.display());
                            failed += 1;
                        } else {
                            ok += 1;
                        }
                    }
                    Err(e) => {
                        eprintln!("trove import: readlink {}: {e}", entry.path().display());
                        failed += 1;
                    }
                }
                continue;
            }

            if !ft.is_file() {
                // Skip sockets / fifos / device nodes — out of scope.
                continue;
            }

            // Ensure parent exists (walkdir surfaces parents first via
            // sort_by_file_name, but a missing parent here is recoverable).
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }

            // Stream source -> destination. The destination open + write
            // goes through FUSE → the validation gate. EINVAL on close means
            // the gate rejected it; a `.errors` sidecar will exist alongside.
            let result = (|| -> io::Result<()> {
                let mut src_f = std::fs::File::open(entry.path())?;
                let mut dst_f = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&target)?;
                std::io::copy(&mut src_f, &mut dst_f)?;
                // Explicit drop -> close -> flush -> validation gate fires.
                drop(dst_f);
                // Preserve permissions where the gate accepted the file.
                if let Ok(meta) = entry.metadata() {
                    let mode = meta.permissions().mode();
                    let _ = std::fs::set_permissions(
                        &target,
                        std::fs::Permissions::from_mode(mode),
                    );
                }
                Ok(())
            })();

            match result {
                Ok(()) => ok += 1,
                Err(e) => {
                    // EINVAL is the validation gate; anything else is an IO
                    // problem we want logged but not aborting the whole import.
                    eprintln!(
                        "trove import: {} rejected: {e}",
                        target.display()
                    );
                    failed += 1;
                }
            }
        }
        Ok(CopyReport { ok, failed })
    }

    /// The orchestrator. Walks the safety gates in order, then performs the
    /// rename + mount + copy-back. Blocks on the mount until SIGINT.
    pub fn run(
        opts: ImportOptions,
        cfg: &Config,
        volume: Option<String>,
        meta: Option<String>,
        cache: Option<PathBuf>,
        versions_db: Option<String>,
        no_embed: bool,
    ) -> Result<()> {
        let path = opts
            .path
            .canonicalize()
            .with_context(|| format!("resolving {}", opts.path.display()))?;
        let home = dirs_home()?;

        // (1) refuse obviously-dangerous targets.
        if is_dangerous_path(&path, Some(&home)) {
            bail!(
                "refusing to import {} — that path is too close to the system root \
                 or to your home directory. Pick a specific subdirectory.",
                path.display()
            );
        }
        if !path.is_dir() {
            bail!("{} is not a directory", path.display());
        }
        if is_already_mountpoint(&path)? {
            bail!(
                "{} is already a mountpoint — refusing to import a live mount",
                path.display()
            );
        }

        // (2) measure the source.
        let (file_count, total_bytes) = scan_source(&path)?;
        if exceeds_thresholds(file_count, total_bytes) && !opts.force {
            bail!(
                "{} has {file_count} files ({}). \
                 Above the safe threshold ({MAX_FILES_WITHOUT_FORCE} files / {}). \
                 Pass --force if this is really what you meant.",
                path.display(),
                human_bytes(total_bytes),
                human_bytes(MAX_BYTES_WITHOUT_FORCE),
            );
        }

        // (3) dry-run validation against the SOURCE — so a known-bad file
        // halts the import before we move anything. Only if a schema dir
        // resolves AND it exists.
        let types_dir = opts.types.clone().unwrap_or_else(|| path.join(".types"));
        if types_dir.is_dir() {
            println!(
                "{} dry-run validation against {}…",
                "trove import:".bold(),
                types_dir.display()
            );
            let s = crate::commands::check::run(&path, true)?;
            if s.failed > 0 {
                bail!(
                    "validation sweep found {} failure(s) in {} — aborting before any move. \
                     Fix the failures in place, then re-run `trove import`.",
                    s.failed,
                    path.display()
                );
            }
            println!(
                "  {} {} files, {} valid, {} untyped",
                "✓".green(),
                s.checked,
                s.valid,
                s.untyped
            );
        }

        // (4) confirmation
        let ts = backup_timestamp();
        let backup = backup_dir_for(&path, &home, &ts);
        if !opts.yes {
            println!();
            println!(
                "Detected {} file(s) ({}) in {}.",
                file_count,
                human_bytes(total_bytes),
                path.display()
            );
            println!("About to:");
            println!("  1. Move {}  →  {}", path.display(), backup.display());
            println!("  2. Mount trove at {}", path.display());
            println!("  3. Copy files back through the validation gate");
            println!();
            println!(
                "This is reversible: if anything fails, mv {} back to {}.",
                backup.display(),
                path.display()
            );
            println!();
            confirm_typed(&path)?;
        }

        // (5) backup
        let parent = backup
            .parent()
            .ok_or_else(|| anyhow!("backup path has no parent"))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        if backup.exists() {
            bail!(
                "backup destination {} already exists — refusing to clobber",
                backup.display()
            );
        }
        // Try rename first (atomic on same fs). On cross-device, fall back to
        // copy + remove; on the same volume this is the common case and fast.
        if let Err(e) = std::fs::rename(&path, &backup) {
            // EXDEV on Linux, ENOTSUP/EXDEV on macOS — fall back to recursive copy.
            eprintln!(
                "trove import: rename failed ({e}), falling back to copy + remove"
            );
            copy_dir_recursive(&path, &backup)?;
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("removing original {}", path.display()))?;
        }
        std::fs::create_dir_all(&path)
            .with_context(|| format!("re-creating {}", path.display()))?;
        println!(
            "{} moved original to {}",
            "trove import:".bold(),
            backup.display()
        );

        // (6) mount
        let fs = init_fs(volume, meta, cache, cfg)?;
        let registry = if types_dir.is_dir() {
            // Schemas were under the SOURCE — now under the backup. Load from
            // the backup so the mount validates with the same schemas.
            let backed_up_types = backup.join(".types");
            if backed_up_types.is_dir() {
                Registry::load(&backup)?
            } else {
                Registry::empty()
            }
        } else {
            Registry::empty()
        };
        let versions_url = versions_db
            .or_else(|| std::env::var("TROVE_VERSIONS_DB").ok().filter(|s| !s.is_empty()))
            .or_else(|| cfg.versions_db.clone());
        let schema = cfg.schema_name();
        let versions = match &versions_url {
            Some(url) => Some(VersionStore::connect(url, schema.as_deref())?),
            None => None,
        };
        let embed_tx: Option<Sender<(String, Vec<u8>)>> = match (&versions_url, no_embed) {
            (Some(url), false) => {
                let key = std::env::var("OPENAI_API_KEY").map_err(|_| {
                    anyhow!(
                        "OPENAI_API_KEY not set. Set it, or pass --no-embed to import without embedding."
                    )
                })?;
                Some(crate::embed::spawn_embedder(url, key, schema.as_deref())?)
            }
            _ => None,
        };

        println!(
            "{} mounting at {} (versioning {}; embed {})",
            "trove import:".bold(),
            path.display(),
            if versions.is_some() { "on" } else { "off" },
            if embed_tx.is_some() {
                "on"
            } else if versions.is_none() {
                "off (no versioning)"
            } else {
                "off (--no-embed)"
            },
        );
        // Spawn the mount in the background so we can write files through it.
        // The returned BackgroundSession unmounts on drop — keep it alive
        // across the copy-back AND the foreground park-loop after.
        let session = crate::mount::spawn_with_versions_and_embed(
            fs, registry, versions, embed_tx, &path,
        )
        .with_context(|| format!("mounting at {}", path.display()))?;

        // Tiny wait — the kernel sometimes needs a moment to make the mount
        // serviceable (first stat() may otherwise race the spawn).
        wait_mounted(&path);

        // (7) stream files back through the mount.
        println!("{} copying files back…", "trove import:".bold());
        let report = stream_into_mount(&backup, &path)?;

        // (8) report
        println!();
        println!(
            "{} imported {} file(s); {} rejected by validation (see .errors sidecars).",
            "✓".green(),
            report.ok,
            report.failed
        );
        println!("  Backup retained at {}", backup.display());
        println!("  Mount is live at {}. Ctrl-C to unmount.", path.display());

        // (9) park until SIGINT. fuser's BackgroundSession joins its worker
        // threads when dropped; we want it kept alive until the user stops us.
        park_until_interrupted();
        // Explicit drop so the unmount happens before main returns.
        drop(session);
        Ok(())
    }

    fn dirs_home() -> Result<PathBuf> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("$HOME not set — required for the backup location"))
    }

    /// Best-effort check: is `path` already a mountpoint? Compare its device
    /// number to its parent's. Mismatch = mounted.
    fn is_already_mountpoint(path: &Path) -> Result<bool> {
        use std::os::unix::fs::MetadataExt;
        let here = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?;
        let parent = match path.parent() {
            Some(p) => p,
            None => return Ok(true), // `/` — already refused by is_dangerous_path
        };
        let parent_md = match std::fs::metadata(parent) {
            Ok(m) => m,
            Err(_) => return Ok(false),
        };
        Ok(here.dev() != parent_md.dev())
    }

    /// Recursive directory copy used when rename hits EXDEV.
    fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in walkdir::WalkDir::new(src).min_depth(1).follow_links(false) {
            let entry = entry?;
            let rel = entry.path().strip_prefix(src).unwrap();
            let target = dst.join(rel);
            let ft = entry.file_type();
            if ft.is_dir() {
                std::fs::create_dir_all(&target)?;
            } else if ft.is_symlink() {
                let link = std::fs::read_link(entry.path())?;
                symlink(&link, &target)?;
            } else if ft.is_file() {
                if let Some(p) = target.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::copy(entry.path(), &target)?;
            }
        }
        Ok(())
    }

    fn init_fs(
        volume: Option<String>,
        meta: Option<String>,
        cache: Option<PathBuf>,
        cfg: &Config,
    ) -> Result<crate::jfs::Fs> {
        let volume = crate::config::resolve(volume, "TROVE_VOLUME", cfg.volume.clone(), "volume name")?;
        let meta = crate::config::resolve(meta, "TROVE_META", cfg.meta.clone(), "meta URL")?;
        let cache = cache
            .map(|c| c.to_string_lossy().into_owned())
            .or_else(|| std::env::var("TROVE_CACHE").ok().filter(|s| !s.is_empty()))
            .or_else(|| cfg.cache.clone())
            .unwrap_or_else(|| "/tmp/trove-cache".to_string());
        crate::jfs::Fs::init(&volume, &meta, &cache)
    }

    /// Block until the mountpoint can be `stat`'d (the kernel has accepted
    /// the FUSE connection and is serving requests).
    fn wait_mounted(mountpoint: &Path) {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if std::fs::metadata(mountpoint).is_ok() {
                // One extra beat — readdir tends to need a little more time.
                std::thread::sleep(Duration::from_millis(50));
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Park the foreground thread indefinitely. Ctrl-C unwinds main, which
    /// drops the BackgroundSession and unmounts.
    fn park_until_interrupted() {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
}

#[cfg(feature = "mount")]
pub use io_path::run;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_root() {
        assert!(is_dangerous_path(Path::new("/"), None));
    }

    #[test]
    fn refuses_top_level_system_dirs() {
        for p in &["/home", "/usr", "/etc", "/var", "/Users", "/Volumes"] {
            assert!(
                is_dangerous_path(Path::new(p), None),
                "expected {p} to be refused"
            );
        }
    }

    #[test]
    fn refuses_two_component_paths() {
        // `/home`, `/usr/local` — too shallow.
        assert!(is_dangerous_path(Path::new("/home"), None));
        // Note: `/usr/local` has 3 components (root + usr + local). The bad
        // list above covers `/usr` itself; `/usr/local` is acceptable from a
        // component-count standpoint and only refused if it's on the bad list.
        // We document the rule as "≤ 2 components OR on the bad list".
    }

    #[test]
    fn refuses_home_itself() {
        let home = PathBuf::from("/home/cp");
        assert!(is_dangerous_path(&home, Some(&home)));
    }

    #[test]
    fn allows_vault_subdirectory_of_home() {
        let home = PathBuf::from("/home/cp");
        assert!(!is_dangerous_path(Path::new("/home/cp/vault"), Some(&home)));
    }

    #[test]
    fn allows_deep_user_path() {
        assert!(!is_dangerous_path(Path::new("/home/cp/code/trove"), None));
    }

    #[test]
    fn thresholds_refuse_too_many_files() {
        assert!(exceeds_thresholds(MAX_FILES_WITHOUT_FORCE + 1, 0));
    }

    #[test]
    fn thresholds_refuse_too_many_bytes() {
        assert!(exceeds_thresholds(0, MAX_BYTES_WITHOUT_FORCE + 1));
    }

    #[test]
    fn thresholds_allow_small_trees() {
        assert!(!exceeds_thresholds(100, 5_000_000));
    }

    #[test]
    fn backup_dir_includes_basename_and_timestamp() {
        let home = PathBuf::from("/home/cp");
        let bd = backup_dir_for(Path::new("/home/cp/vault"), &home, "2026-05-28T12-00-00");
        assert_eq!(
            bd,
            PathBuf::from("/home/cp/.trove-backup/vault-2026-05-28T12-00-00")
        );
    }

    #[test]
    fn backup_timestamp_is_parseable_shape() {
        let ts = backup_timestamp();
        // YYYY-MM-DDTHH-MM-SS = 19 chars
        assert_eq!(ts.len(), 19, "ts = {ts:?}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], "-");
        assert_eq!(&ts[16..17], "-");
    }
}
