//! e2e test for `trove embed`. Stages content as a version clone in a JuiceFS
//! volume, registers the blob in the version DB, then embeds it for real via
//! OpenAI and checks the vectors land in `blob_chunks`. Run with `--features
//! mount` and `source ~/.secret_env` (needs OPENAI_API_KEY + the supabase stack
//! + libjfs/juicefs). The OpenAI cost is a few cents' fraction (2 short chunks).
#![cfg(feature = "mount")]

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use trove::embed::{chunk_markdown, embed_blob};
use trove::jfs::Fs;
use trove::version::{sha256_hex, VersionStore};

fn juicefs_bin() -> String {
    std::env::var("JUICEFS_BIN")
        .unwrap_or_else(|_| "/home/cp/code/trove/spike/juicefs/juicefs".to_string())
}
fn db_url() -> String {
    std::env::var("TROVE_DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54322/postgres".to_string())
}
fn uniq() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    )
}

/// A freshly formatted throwaway JuiceFS volume (sqlite meta, file store).
fn fresh_fs() -> Fs {
    let dir = std::env::temp_dir().join(format!("trove-embed-{}", uniq()));
    std::fs::create_dir_all(dir.join("store")).unwrap();
    let name = format!("vol{}", uniq().replace('-', ""));
    let meta = format!("sqlite3://{}/meta.db", dir.display());
    let out = Command::new(juicefs_bin())
        .args(["format", "--storage", "file", "--bucket", &format!("{}/store/", dir.display()), &meta, &name])
        .output()
        .expect("run juicefs format");
    assert!(out.status.success(), "format failed: {}", String::from_utf8_lossy(&out.stderr));
    Fs::init(&name, &meta, &format!("{}/cache", dir.display())).unwrap()
}

#[test]
fn embed_blob_writes_one_vector_per_header_chunk() {
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY (source ~/.secret_env)");
    let fs = fresh_fs();
    let mut vs = VersionStore::connect(&db_url()).expect("version DB up?");

    // Unique content so the hash is fresh each run (blob starts un-embedded).
    let tag = uniq();
    let content = format!("# Alpha {tag}\nfirst section about cats\n\n# Beta {tag}\nsecond section about boats\n");
    let hash = sha256_hex(content.as_bytes());

    // Stage it as the version clone, and register the blob in the chain.
    let _ = fs.mkdir("/.trove", 0o755);
    let _ = fs.mkdir("/.trove/versions", 0o755);
    fs.write_all(&format!("/.trove/versions/{hash}"), content.as_bytes(), 0o644).unwrap();
    vs.record_meta(&format!("/test/embed-{tag}.md"), &hash, content.len() as i64, None).unwrap();

    // Pending before, not pending after.
    assert!(vs.pending_embedding_hashes(100_000).unwrap().contains(&hash));
    let n = embed_blob(&fs, &mut vs, &key, &hash).unwrap();
    assert_eq!(n, chunk_markdown(&content).len());
    assert_eq!(n, 2, "two headed sections => two chunks");
    assert!(!vs.pending_embedding_hashes(100_000).unwrap().contains(&hash));

    // Vectors really landed: 2 rows, both with a 3072-dim embedding + heading.
    let mut chk = postgres::Client::connect(&db_url(), postgres::NoTls).unwrap();
    let row = chk
        .query_one(
            "select count(*), count(embedding), max(vector_dims(embedding)) \
             from blob_chunks where blob_hash = $1",
            &[&hash],
        )
        .unwrap();
    let (total, embedded, dims): (i64, i64, Option<i32>) = (row.get(0), row.get(1), row.get(2));
    assert_eq!(total, 2);
    assert_eq!(embedded, 2, "both chunks embedded (no null sentinels)");
    assert_eq!(dims, Some(3072));

    let headings: Vec<Option<String>> = chk
        .query("select heading from blob_chunks where blob_hash = $1 order by ordinal", &[&hash])
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(headings, vec![Some(format!("Alpha {tag}")), Some(format!("Beta {tag}"))]);
}
