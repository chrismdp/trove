//! Content-addressed blob store over Cloudflare R2.
//!
//! Version bytes live here, keyed by their sha256 (`trove-versions/<hash>`), so
//! identical content is one object and history never bloats the metadata DB.
//! R2 — not Postgres — because versioning keeps every revision's bytes forever;
//! an object store is the right home for ever-growing immutable blobs (and a
//! later Cloudflare Worker reads these same objects, via a native R2 binding,
//! to compute embeddings).
//!
//! Sync on purpose (the commit path and the WAL drain are sync): `rusty-s3`
//! presigns the request, `ureq` does the blocking HTTP — no async runtime.

use anyhow::{bail, Context, Result};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use std::io::Read;
use std::time::Duration;

const PREFIX: &str = "trove-versions/";
const SIGN_TTL: Duration = Duration::from_secs(300);

pub struct BlobStore {
    bucket: Bucket,
    creds: Credentials,
}

impl BlobStore {
    /// Build from the R2 environment: `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`,
    /// `CLOUDFLARE_ACCOUNT_ID`; bucket defaults to `trove` (override `R2_BUCKET`).
    /// Path-style against the account endpoint — the form proven in the spike.
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("R2_ACCESS_KEY_ID").context("R2_ACCESS_KEY_ID")?;
        let secret = std::env::var("R2_SECRET_ACCESS_KEY").context("R2_SECRET_ACCESS_KEY")?;
        let account = std::env::var("CLOUDFLARE_ACCOUNT_ID").context("CLOUDFLARE_ACCOUNT_ID")?;
        let name = std::env::var("R2_BUCKET").unwrap_or_else(|_| "trove".to_string());
        let endpoint = format!("https://{account}.r2.cloudflarestorage.com")
            .parse::<url::Url>()
            .context("R2 endpoint URL")?;
        let bucket = Bucket::new(endpoint, UrlStyle::Path, name, "auto".to_string())
            .context("constructing R2 bucket")?;
        Ok(Self {
            bucket,
            creds: Credentials::new(key, secret),
        })
    }

    fn object_key(hash: &str) -> String {
        format!("{PREFIX}{hash}")
    }

    /// Store bytes content-addressed. Idempotent: the same hash is the same
    /// object, so a repeat put harmlessly overwrites identical bytes.
    pub fn put(&self, hash: &str, bytes: &[u8]) -> Result<()> {
        let key = Self::object_key(hash);
        let url = self.bucket.put_object(Some(&self.creds), &key).sign(SIGN_TTL);
        match ureq::put(url.as_str()).send_bytes(bytes) {
            Ok(_) => Ok(()),
            Err(e) => bail!("R2 put {key}: {e}"),
        }
    }

    /// Fetch a blob by hash. `Ok(None)` if it doesn't exist (404).
    pub fn get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let key = Self::object_key(hash);
        let url = self.bucket.get_object(Some(&self.creds), &key).sign(SIGN_TTL);
        match ureq::get(url.as_str()).call() {
            Ok(resp) => {
                let mut buf = Vec::new();
                resp.into_reader()
                    .read_to_end(&mut buf)
                    .context("reading R2 object body")?;
                Ok(Some(buf))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => bail!("R2 get {key}: {e}"),
        }
    }
}
