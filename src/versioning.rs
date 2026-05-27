//! Version capture — copy-on-write, single storage substrate, no WAL.
//!
//! A validated commit is versioned by *cloning* the just-written file inside
//! JuiceFS to `/.trove/versions/<hash>` (a COW clone — new metadata sharing the
//! same data blocks, zero byte copy) and appending a row to the `file_versions`
//! chain. Because the clone and the chain row ride the *same* backend the live
//! write just succeeded against (one JuiceFS volume, one Postgres), there's no
//! independent store to be "eventually" consistent with — so this is plain
//! best-effort with a light retry, not a durable write-ahead log.
//!
//! `cat` reads a historical version straight back out of its clone via libjfs.

use crate::jfs::Fs;
use crate::version::{sha256_hex, VersionStore};
use anyhow::{Context, Result};
use std::thread::sleep;
use std::time::Duration;

/// Where COW version snapshots live inside the volume (hidden from normal use).
const VERSIONS_DIR: &str = "/.trove/versions";
const RETRIES: usize = 3;

fn version_path(hash: &str) -> String {
    format!("{VERSIONS_DIR}/{hash}")
}

/// Ensure `/.trove/versions` exists (idempotent — ignores "already exists").
fn ensure_versions_dir(fs: &Fs) {
    let _ = fs.mkdir("/.trove", 0o755);
    let _ = fs.mkdir(VERSIONS_DIR, 0o755);
}

/// Record a validated commit of `path` (already written to the live tree with
/// `content`). Clones it to the content-addressed version archive (skipping if
/// that hash is already snapshotted — dedup) and appends the chain row. Retries
/// a few times on transient failure; the caller treats a final error as
/// best-effort (the file is already safely written — versioning must not fail
/// the write).
pub fn record_version(
    fs: &Fs,
    versions: &mut VersionStore,
    path: &str,
    content: &[u8],
    author: Option<&str>,
) -> Result<i32> {
    let hash = sha256_hex(content);
    let size = content.len() as i64;
    let dst = version_path(&hash);

    let mut last_err = None;
    for attempt in 0..RETRIES {
        let result = (|| -> Result<i32> {
            ensure_versions_dir(fs);
            // COW snapshot of the live file. Skip if this content is already
            // archived (dedup) — a clone over an existing path would error.
            if !fs.exists(&dst) {
                fs.clone_file(path, &dst, true).context("cloning version snapshot")?;
            }
            versions.record_meta(path, &hash, size, author)
        })();
        match result {
            Ok(rev) => return Ok(rev),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < RETRIES {
                    sleep(Duration::from_millis(200));
                }
            }
        }
    }
    Err(last_err.unwrap())
}

/// The content of `path` at revision `rev`: resolve its blob hash from the chain,
/// then read the COW clone back via libjfs. `None` if that (path, rev) is unknown.
pub fn cat(fs: &Fs, versions: &mut VersionStore, path: &str, rev: i32) -> Result<Option<Vec<u8>>> {
    let Some(hash) = versions.blob_hash_at(path, rev)? else {
        return Ok(None);
    };
    Ok(Some(fs.read_all(&version_path(&hash))?))
}
