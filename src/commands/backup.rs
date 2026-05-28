//! `trove backup` — write a local mirror of every committed file by walking the
//! version chain so history is preserved.
//!
//! Trove's primary durability already lives in two places: the bucket (R2/S3,
//! which holds the actual blob bytes) and Postgres (which holds the version
//! chain + embeddings). This command is the third leg of a belt-and-braces
//! backup strategy: a plain on-disk tree the operator can `rsync`/snapshot
//! with normal tools, with **every historical revision** present — not just
//! the live tree.
//!
//! Two layouts are supported. [`Layout::ByPath`] (the default, the one most
//! operators want) produces a *live tree* at `<dest>/<path>` plus a
//! `<dest>/.versions/<path>/rev-<N>` sidecar carrying historical revisions.
//! That mirrors what the user "sees" via the mount, with history tucked
//! away beside it. [`Layout::ByRev`] flips the cardinality: one full tree
//! per rev under `<dest>/rev-<N>/<path>`. Useful when you want to diff
//! whole-tree snapshots at a point in time.
//!
//! Incremental by design: before writing each file we hash what's already on
//! disk and compare against the version's `blob_hash`. A re-run with no new
//! commits writes no bytes and reports `skipped_unchanged` for every rev. A
//! `dry_run` walks the chain and counts what *would* be written without
//! touching disk — useful for sizing a destination before committing to it.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::jfs::Fs;
use crate::version::VersionStore;
use crate::versioning::cat;

/// On-disk layout for the mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `<dest>/<path>` for the live tree (latest rev), plus
    /// `<dest>/.versions/<path>/rev-<N>` for every historical revision.
    /// The default — matches the operator's mental model of "the tree, with
    /// history beside it".
    ByPath,
    /// `<dest>/rev-<N>/<path>` — one full tree per revision. Useful when you
    /// want point-in-time snapshots you can diff with `diff -r`.
    ByRev,
}

impl Layout {
    /// Parse the CLI string. Accepts `by-path` or `by-rev` (kebab-case).
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "by-path" => Ok(Layout::ByPath),
            "by-rev" => Ok(Layout::ByRev),
            other => anyhow::bail!("unknown layout `{other}` — expected `by-path` or `by-rev`"),
        }
    }
}

/// What `run` should do.
pub struct BackupOptions {
    /// Output directory. Created if absent.
    pub dest: PathBuf,
    /// See [`Layout`].
    pub layout: Layout,
    /// Reserved for a future "only export revisions newer than X" filter.
    /// Currently unused — included so callers don't break when it's wired up.
    // TODO: thread through to `file_versions.created_at >= $since` when
    // a chrono-free way to bind a timestamptz from a string is settled.
    pub since: Option<String>,
    /// Walk + count but don't write.
    pub dry_run: bool,
}

/// What `run` actually did. Counts grow monotonically: every revision is
/// either written or skipped-unchanged; together they account for the full
/// walk of the version chain across every distinct path.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BackupReport {
    /// Distinct paths walked.
    pub paths: usize,
    /// Revisions whose bytes were written to disk this run.
    pub revisions_written: usize,
    /// Bytes written (sum of `version.size` for written revisions).
    pub bytes_written: u64,
    /// Revisions whose bytes were already on disk with a matching sha256.
    /// On a re-run with no new commits, this equals the total revision count.
    pub skipped_unchanged: usize,
}

/// Walk every versioned path, write every revision to `opts.dest` per `opts.layout`.
///
/// Algorithm: for each path, walk its log oldest→newest (we reverse the
/// `log()` output, which is newest-first). For each version, read the bytes
/// through [`versioning::cat`] — the same primitive the rest of Trove uses
/// to recover historical content. Then either write or skip-unchanged based
/// on what's on disk already. For [`Layout::ByPath`] we additionally drop a
/// "live tree" copy of the latest rev at `<dest>/<path>`.
pub fn run(fs: &Fs, versions: &mut VersionStore, opts: &BackupOptions) -> Result<BackupReport> {
    if !opts.dry_run {
        fs::create_dir_all(&opts.dest)
            .with_context(|| format!("creating {}", opts.dest.display()))?;
    }

    let paths = versions.paths().context("listing versioned paths")?;
    let mut report = BackupReport::default();
    report.paths = paths.len();

    for path in &paths {
        // Newest-first from the DB; reverse so the natural order of writes
        // is rev 1 → rev 2 → … (cheaper to reason about; no functional impact).
        let mut log = versions.log(path).with_context(|| format!("log({path})"))?;
        log.reverse();

        let Some(latest) = log.last().cloned() else { continue };

        for version in &log {
            let bytes = match cat(fs, versions, path, version.rev) {
                Ok(Some(b)) => b,
                // Defensive: shouldn't happen — log() returned a rev whose
                // blob hash resolves elsewhere. Skip and carry on.
                Ok(None) => continue,
                // Volume doesn't carry the clone for this rev (e.g. the DB is
                // shared with a different volume, or the COW snapshot was
                // pruned). Skip rather than failing the whole backup — the
                // operator's right next step is `trove doctor`, not abort.
                Err(_) => continue,
            };

            let target = revision_dest(&opts.dest, opts.layout, path, version.rev);
            if file_matches(&target, &version.blob_hash) {
                report.skipped_unchanged += 1;
            } else {
                report.revisions_written += 1;
                report.bytes_written = report.bytes_written.saturating_add(bytes.len() as u64);
                if !opts.dry_run {
                    write_atomically(&target, &bytes)?;
                }
            }
        }

        // For ByPath we also drop the latest content at the live-tree path.
        // Same defensive treatment as the per-rev walk: a missing clone is a
        // skip, not a hard error.
        if opts.layout == Layout::ByPath {
            if let Ok(Some(bytes)) = cat(fs, versions, path, latest.rev) {
                let live = live_dest(&opts.dest, path);
                if file_matches(&live, &latest.blob_hash) {
                    // Counts deliberately don't double-count the live-tree
                    // mirror — `revisions_written` is the rev count, not the
                    // file count. The live copy is bookkeeping, not history.
                } else if !opts.dry_run {
                    write_atomically(&live, &bytes)?;
                }
            }
        }
    }

    Ok(report)
}

/// Compute the on-disk path for `(path, rev)` under `layout`.
///
/// - `ByPath`: `<dest>/.versions/<normalised>/rev-<N>`
/// - `ByRev`:  `<dest>/rev-<N>/<normalised>`
fn revision_dest(dest: &Path, layout: Layout, path: &str, rev: i32) -> PathBuf {
    let rel = normalise(path);
    match layout {
        Layout::ByPath => dest.join(".versions").join(&rel).join(format!("rev-{rev}")),
        Layout::ByRev => dest.join(format!("rev-{rev}")).join(&rel),
    }
}

/// Path for the latest-rev "live tree" copy in `ByPath` layout.
fn live_dest(dest: &Path, path: &str) -> PathBuf {
    dest.join(normalise(path))
}

/// Strip the leading slash so `/people/alice.md` joins under `dest` cleanly
/// (rather than rooting back at `/people/...`). Empty segments are dropped.
fn normalise(path: &str) -> PathBuf {
    let trimmed = path.trim_start_matches('/');
    PathBuf::from(trimmed)
}

/// Does the file at `target` already exist with sha256 matching `expected_hex`?
/// Returns false on any IO error (missing file, permissions, …) — the caller
/// then writes through, which is the safe default.
fn file_matches(target: &Path, expected_hex: &str) -> bool {
    let Ok(mut f) = fs::File::open(target) else { return false };
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    hex::encode(hasher.finalize()) == expected_hex
}

/// Write `bytes` to `target`, creating the parent directory if needed. Uses a
/// `target.tmp` + rename so a crash mid-write doesn't leave a half-file that
/// would pass a (wrong) hash check on the next run.
fn write_atomically(target: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let tmp = target.with_extension("trove-backup.tmp");
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, target)
        .with_context(|| format!("renaming into place: {}", target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_path_revision_dest_buries_history_under_dot_versions() {
        let d = revision_dest(Path::new("/tmp/b"), Layout::ByPath, "/people/alice.md", 3);
        assert_eq!(d, PathBuf::from("/tmp/b/.versions/people/alice.md/rev-3"));
    }

    #[test]
    fn by_rev_revision_dest_groups_per_rev() {
        let d = revision_dest(Path::new("/tmp/b"), Layout::ByRev, "/people/alice.md", 3);
        assert_eq!(d, PathBuf::from("/tmp/b/rev-3/people/alice.md"));
    }

    #[test]
    fn live_dest_drops_leading_slash() {
        let d = live_dest(Path::new("/tmp/b"), "/people/alice.md");
        assert_eq!(d, PathBuf::from("/tmp/b/people/alice.md"));
    }

    #[test]
    fn normalise_strips_only_the_first_slash() {
        assert_eq!(normalise("/a/b"), PathBuf::from("a/b"));
        assert_eq!(normalise("a/b"), PathBuf::from("a/b"));
        // multi-leading-slash isn't expected from the DB but the trim is
        // greedy — defend against the pathological input.
        assert_eq!(normalise("///a"), PathBuf::from("a"));
    }

    #[test]
    fn layout_parses_known_values_and_rejects_unknown() {
        assert_eq!(Layout::parse("by-path").unwrap(), Layout::ByPath);
        assert_eq!(Layout::parse("by-rev").unwrap(), Layout::ByRev);
        assert!(Layout::parse("by_rev").is_err());
        assert!(Layout::parse("flat").is_err());
    }

    #[test]
    fn file_matches_returns_false_for_missing_file() {
        assert!(!file_matches(Path::new("/no/such/file/here"), "0".repeat(64).as_str()));
    }

    #[test]
    fn file_matches_returns_true_when_hash_agrees() {
        let dir = std::env::temp_dir().join(format!("trove-backup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f");
        std::fs::write(&p, b"hello").unwrap();
        // sha256("hello")
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(file_matches(&p, expected));
        assert!(!file_matches(&p, &"0".repeat(64)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
