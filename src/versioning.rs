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
use sha2::{Digest, Sha256};
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
    snapshot(fs, versions, path, &sha256_hex(content), content.len() as i64, author)
}

/// Version a file already written to the live tree **without holding its bytes
/// in memory** — stream-hash it from the volume (1 MiB chunks, never buffering
/// the whole file) so a multi-GB binary can be versioned cheaply. Returns
/// `(rev, content hash)`. This is what the mount uses for ungoverned *binary*
/// files (which stream straight through and are never embedded, so there's no
/// reason to ever hold them whole); governed/text files use [`record_version`]
/// with the bytes they already have buffered for validation/embedding.
pub fn record_version_from_fs(
    fs: &Fs,
    versions: &mut VersionStore,
    path: &str,
    author: Option<&str>,
) -> Result<(i32, String)> {
    let size = fs.stat(path)?.length as i64;
    let hash = stream_hash(fs, path)?;
    let rev = snapshot(fs, versions, path, &hash, size, author)?;
    Ok((rev, hash))
}

/// sha256 of a file's contents read in 1 MiB chunks — never holds the whole
/// file, so content-addressing a large binary stays cheap.
fn stream_hash(fs: &Fs, path: &str) -> Result<String> {
    let f = fs.open(path, 0).context("opening file to hash")?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0i64;
    loop {
        let n = f.read_at(&mut buf, off).context("reading file to hash")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        off += n as i64;
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Clone the live file at `path` into the content-addressed archive and append
/// the chain row, with light retry. Shared by the buffered ([`record_version`])
/// and streamed ([`record_version_from_fs`]) entry points. Best-effort: a final
/// error is non-fatal to the caller (the live file is already safely written).
fn snapshot(
    fs: &Fs,
    versions: &mut VersionStore,
    path: &str,
    hash: &str,
    size: i64,
    author: Option<&str>,
) -> Result<i32> {
    let dst = version_path(hash);
    let mut last_err = None;
    for attempt in 0..RETRIES {
        let result = (|| -> Result<i32> {
            ensure_versions_dir(fs);
            // COW snapshot of the live file. Skip if this content is already
            // archived (dedup) — a clone over an existing path would error.
            if !fs.exists(&dst) {
                fs.clone_file(path, &dst, true).context("cloning version snapshot")?;
            }
            versions.record_meta(path, hash, size, author)
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
