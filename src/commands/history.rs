//! `trove log` / `cat` / `diff` / `restore` — the user-facing version-history
//! surface over the chain that the mount records on every commit.
//!
//! `log` needs only the version DB; `cat`/`diff`/`restore` also read content
//! back from the COW clones via libjfs, so they need a `Fs`. These functions
//! return data (not printed output) so they're testable directly; `main` does
//! the formatting and I/O.

use crate::jfs::Fs;
use crate::version::{Version, VersionStore};
use crate::versioning::{cat as cat_rev, record_version};
use anyhow::{bail, Context, Result};

/// Full history of `path`, newest first (empty if the path has no versions).
pub fn log(versions: &mut VersionStore, path: &str) -> Result<Vec<Version>> {
    versions.log(path)
}

/// The bytes of `path` at revision `rev`. Errors if that revision is unknown.
pub fn cat(fs: &Fs, versions: &mut VersionStore, path: &str, rev: i32) -> Result<Vec<u8>> {
    cat_rev(fs, versions, path, rev)?
        .with_context(|| format!("{path} has no revision {rev}"))
}

/// A unified line diff between two revisions of `path` (`a` → `b`). Binary
/// (non-UTF-8) revisions can't be line-diffed — report that instead.
pub fn diff(fs: &Fs, versions: &mut VersionStore, path: &str, a: i32, b: i32) -> Result<String> {
    let ba = cat(fs, versions, path, a)?;
    let bb = cat(fs, versions, path, b)?;
    let (Ok(ta), Ok(tb)) = (std::str::from_utf8(&ba), std::str::from_utf8(&bb)) else {
        return Ok(format!(
            "binary content — cannot diff (rev {a}: {} bytes, rev {b}: {} bytes)\n",
            ba.len(),
            bb.len()
        ));
    };
    let diff = similar::TextDiff::from_lines(ta, tb);
    let mut out = format!("--- {path}@{a}\n+++ {path}@{b}\n");
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => "-",
            similar::ChangeTag::Insert => "+",
            similar::ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(change.value());
        if !change.value().ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

/// Restore `path` to revision `rev`: write that (already-validated) version's
/// bytes back to the live tree and record the restore as a NEW revision, so the
/// timeline is append-only — restoring is itself a versioned event, never a
/// silent overwrite. Returns the new revision number.
///
/// The content being restored was valid when first committed, so writing it
/// back via libjfs is safe; we re-record it (dedup means the blob is reused —
/// only a new chain row is appended).
pub fn restore(fs: &Fs, versions: &mut VersionStore, path: &str, rev: i32) -> Result<i32> {
    let content = cat(fs, versions, path, rev)?;
    // Mode is informational: write_all truncates the existing file in place,
    // preserving its current mode/uid/gid/xattrs. Only used if the file has
    // since been deleted (we recreate it with the historical mode if we have
    // it, falling back to 0o644).
    let mode = fs
        .lstat(path)
        .map(|i| (i.mode & 0o7777) as u16)
        .unwrap_or(0o644);
    fs.write_all(path, &content, mode)
        .with_context(|| format!("writing restored content to {path}"))?;
    let new_rev = record_version(fs, versions, path, &content, Some("restore"))
        .with_context(|| format!("recording restore of {path}"))?;
    if new_rev == rev {
        bail!("restore recorded the same rev {rev} — unexpected");
    }
    Ok(new_rev)
}
