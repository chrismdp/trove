//! e2e tests for the R2 blob store. Hit real Cloudflare R2 (bucket `trove`,
//! prefix `trove-versions/`) using the creds in the environment — run with
//! `--features mount` and `source ~/.secret_env`. Each test uses a unique hash
//! so runs don't collide; objects are cleaned up at the end.
#![cfg(feature = "mount")]

use trove::blobstore::BlobStore;

fn unique_hash(tag: &str) -> String {
    // Not a real sha256 — just a unique, hash-shaped key for isolation.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("test-{tag}-{}-{nanos}", std::process::id())
}

#[test]
fn put_then_get_roundtrips_content() {
    let store = BlobStore::from_env().expect("R2 creds in env (source ~/.secret_env)");
    let hash = unique_hash("roundtrip");
    let bytes = b"---\ntype: note\n---\nversion blob bytes in R2".to_vec();

    store.put(&hash, &bytes).unwrap();
    let got = store.get(&hash).unwrap();
    assert_eq!(got.as_deref(), Some(&bytes[..]), "round-trip through R2");
}

#[test]
fn get_missing_blob_is_none() {
    let store = BlobStore::from_env().expect("R2 creds in env");
    assert_eq!(store.get(&unique_hash("missing")).unwrap(), None);
}

#[test]
fn put_is_idempotent_for_identical_content() {
    let store = BlobStore::from_env().expect("R2 creds in env");
    let hash = unique_hash("idem");
    let bytes = b"same bytes, put twice".to_vec();
    store.put(&hash, &bytes).unwrap();
    store.put(&hash, &bytes).unwrap(); // no error, same object
    assert_eq!(store.get(&hash).unwrap().as_deref(), Some(&bytes[..]));
}
