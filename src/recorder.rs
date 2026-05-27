//! The version recorder — best-effort, eventually consistent.
//!
//! A validated commit must capture the version WITHOUT ever failing the file
//! write: JuiceFS is the live source of truth, and history is applied
//! eventually. So the commit path does the cheapest durable thing — it appends
//! the version (bytes included) to a local write-ahead log — and returns. A
//! background drainer applies the WAL to R2 (blob bytes) + Postgres (metadata)
//! in FIFO order, retrying until each entry lands.
//!
//! Why a WAL rather than "try the DB, fall back on failure":
//!  - the write path never blocks on R2/Postgres latency or availability;
//!  - FIFO drain preserves per-path version order (a later write can't overtake
//!    an earlier one that's still pending);
//!  - the bytes are captured at write time, so a historical version survives
//!    even if the live file is overwritten again before the drain catches up;
//!  - a crash leaves the WAL on disk; the next start drains it.

use crate::blobstore::BlobStore;
use crate::version::{sha256_hex, VersionStore};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One pending version, as journalled. The bytes live alongside in
/// `blobs/<blob_hash>`; this is the metadata + ordering record.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingOp {
    seq: u64,
    path: String,
    blob_hash: String,
    size: i64,
    author: Option<String>,
    ts_ms: u64,
}

/// On-disk write-ahead log: a `journal.jsonl` of [`PendingOp`]s plus a
/// content-addressed `blobs/` staging dir holding the bytes until applied.
struct Wal {
    dir: PathBuf,
    next_seq: u64,
}

impl Wal {
    fn open(dir: PathBuf) -> Result<Wal> {
        fs::create_dir_all(dir.join("blobs")).context("creating WAL dir")?;
        // Resume the seq counter past anything already journalled (crash recovery).
        let next_seq = read_journal(&dir)?
            .iter()
            .map(|e| e.seq)
            .max()
            .map_or(0, |m| m + 1);
        Ok(Wal { dir, next_seq })
    }

    fn journal_path(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }
    fn blob_path(&self, hash: &str) -> PathBuf {
        self.dir.join("blobs").join(hash)
    }

    /// Durably stage one version: write its bytes (content-addressed, once) then
    /// append the journal line. The line is appended last, so a crash mid-write
    /// leaves an orphan blob (harmless, GC'd) rather than a dangling journal ref.
    fn append(&mut self, path: &str, content: &[u8], author: Option<&str>) -> Result<()> {
        let blob_hash = sha256_hex(content);
        let bp = self.blob_path(&blob_hash);
        if !bp.exists() {
            // Write to a temp then rename so a reader never sees a partial blob.
            let tmp = bp.with_extension("tmp");
            fs::write(&tmp, content).context("staging WAL blob")?;
            fs::rename(&tmp, &bp).context("committing WAL blob")?;
        }
        let op = PendingOp {
            seq: self.next_seq,
            path: path.to_string(),
            blob_hash,
            size: content.len() as i64,
            author: author.map(str::to_string),
            ts_ms: now_ms(),
        };
        self.next_seq += 1;
        let line = serde_json::to_string(&op)?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.journal_path())
            .context("opening WAL journal")?;
        writeln!(f, "{line}").context("appending WAL journal")?;
        Ok(())
    }

    fn entries(&self) -> Result<Vec<PendingOp>> {
        read_journal(&self.dir)
    }

    /// Replace the journal with `remaining` (applied entries dropped) and GC any
    /// staged blob no longer referenced. Atomic via temp-file + rename.
    fn rewrite(&self, remaining: &[PendingOp]) -> Result<()> {
        let body: String = remaining
            .iter()
            .map(|e| serde_json::to_string(e).map(|s| s + "\n"))
            .collect::<Result<String, _>>()?;
        let tmp = self.dir.join("journal.tmp");
        fs::write(&tmp, body).context("writing WAL journal")?;
        fs::rename(&tmp, self.journal_path()).context("swapping WAL journal")?;

        let keep: std::collections::HashSet<&str> =
            remaining.iter().map(|e| e.blob_hash.as_str()).collect();
        for entry in fs::read_dir(self.dir.join("blobs"))? {
            let p = entry?.path();
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if !keep.contains(name) {
                    let _ = fs::remove_file(&p);
                }
            }
        }
        Ok(())
    }
}

fn read_journal(dir: &Path) -> Result<Vec<PendingOp>> {
    let p = dir.join("journal.jsonl");
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("reading WAL journal"),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).context("parsing WAL entry"))
        .collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct Inner {
    versions: VersionStore,
    blobs: BlobStore,
    wal: Wal,
}

/// Best-effort, eventually-consistent version recorder. Cheap to clone (shares
/// one locked backend), so the mount and the drain thread hold the same one.
#[derive(Clone)]
pub struct Recorder {
    inner: Arc<Mutex<Inner>>,
}

/// What a single drain pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainStats {
    pub applied: usize,
    pub remaining: usize,
}

impl Recorder {
    pub fn new(versions: VersionStore, blobs: BlobStore, wal_dir: PathBuf) -> Result<Recorder> {
        let wal = Wal::open(wal_dir)?;
        Ok(Recorder {
            inner: Arc::new(Mutex::new(Inner { versions, blobs, wal })),
        })
    }

    /// Record a validated commit. Only touches the local WAL, so it is fast and
    /// cannot fail because of R2/Postgres — the file write is never blocked.
    pub fn record(&self, path: &str, content: &[u8], author: Option<&str>) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.wal.append(path, content, author)
    }

    /// Apply pending WAL entries FIFO until one fails (preserving order), then
    /// compact the journal. Safe to call repeatedly; a no-op when the WAL is
    /// empty. Returns how many applied and how many remain.
    pub fn drain_once(&self) -> Result<DrainStats> {
        let mut g = self.inner.lock().unwrap();
        let entries = g.wal.entries()?;
        if entries.is_empty() {
            return Ok(DrainStats::default());
        }

        let mut applied = 0usize;
        let mut first_error: Option<anyhow::Error> = None;
        for op in &entries {
            let bytes = match fs::read(g.wal.blob_path(&op.blob_hash)) {
                Ok(b) => b,
                Err(e) => {
                    first_error = Some(anyhow::Error::new(e).context("reading staged WAL blob"));
                    break;
                }
            };
            // Blob to R2 first: record_meta references the hash, so the bytes
            // must exist before the metadata claims they do.
            if let Err(e) = g.blobs.put(&op.blob_hash, &bytes) {
                first_error = Some(e);
                break;
            }
            if let Err(e) = g
                .versions
                .record_meta(&op.path, &op.blob_hash, op.size, op.author.as_deref())
            {
                first_error = Some(e);
                break;
            }
            applied += 1;
        }

        let remaining: Vec<PendingOp> = entries[applied..].to_vec();
        let remaining_count = remaining.len();
        g.wal.rewrite(&remaining)?;

        match first_error {
            // Some applied, rest deferred to the next pass — not an error.
            Some(e) if applied == 0 => Err(e),
            _ => Ok(DrainStats {
                applied,
                remaining: remaining_count,
            }),
        }
    }

    /// The content of `path` at revision `rev`: hash from Postgres, bytes from
    /// R2. `None` if unknown. (A version still queued in the WAL isn't visible
    /// here until drained — eventual consistency.)
    pub fn cat(&self, path: &str, rev: i32) -> Result<Option<Vec<u8>>> {
        let mut g = self.inner.lock().unwrap();
        let Some(hash) = g.versions.blob_hash_at(path, rev)? else {
            return Ok(None);
        };
        g.blobs.get(&hash)
    }

    /// Spawn a background thread that drains the WAL every `interval`. Errors are
    /// swallowed (logged) so a transient R2/Postgres outage just defers work to
    /// the next tick — the whole point of best-effort recording.
    pub fn start_draining(&self, interval: Duration) {
        let me = self.clone();
        std::thread::spawn(move || loop {
            if let Err(e) = me.drain_once() {
                eprintln!("trove: version drain deferred: {e:#}");
            }
            std::thread::sleep(interval);
        });
    }
}
