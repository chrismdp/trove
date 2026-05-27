//! `trove embed` — the server-side embedding worker.
//!
//! Search lives in Postgres (`blob_chunks`, pgvector). This worker fills it:
//! for each blob with no chunks yet, it reads the content from its COW clone via
//! libjfs, splits it into header-delimited chunks, embeds each with OpenAI
//! `text-embedding-3-large`, and writes the vectors. It must run where libjfs +
//! the OpenAI key are (the VPS) — psql alone can't read content, and a V8 CF
//! Worker can't load libjfs.
//!
//! Two modes: `run_once` drains the pending set and exits (cron-friendly);
//! `run_watch` does an initial sweep then `LISTEN`s for the `trove_embed` notify
//! that `commit()` fires, so embedding follows a write within seconds — without
//! the OpenAI call ever sitting on the write path.

use crate::jfs::Fs;
use crate::version::{ChunkInsert, VersionStore};
use anyhow::{Context, Result};
use std::time::Duration;

const MODEL: &str = "text-embedding-3-large";
const VERSIONS_DIR: &str = "/.trove/versions";
const BATCH: i64 = 64;

/// A header-delimited slice of a document, with its byte range in the original.
#[derive(Debug, PartialEq, Eq)]
pub struct Chunk {
    pub heading: Option<String>,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// Split markdown into chunks at heading lines. Text before the first heading is
/// the preamble (chunk 0, no heading); each `#`..`######` heading starts a new
/// chunk running to the next heading. Whitespace-only chunks are dropped. Byte
/// ranges locate each chunk in the original for deep-linking.
pub fn chunk_markdown(content: &str) -> Vec<Chunk> {
    // Heading boundaries as (byte offset, heading text). Always include offset 0
    // so leading preamble becomes a chunk.
    let mut bounds: Vec<(usize, Option<String>)> = vec![(0, None)];
    let mut off = 0usize;
    for line in content.split_inclusive('\n') {
        if let Some(h) = heading_text(line.trim_start()) {
            if off == 0 {
                bounds[0].1 = Some(h); // file opens with a heading
            } else {
                bounds.push((off, Some(h)));
            }
        }
        off += line.len();
    }

    let mut chunks = Vec::new();
    for i in 0..bounds.len() {
        let start = bounds[i].0;
        let end = bounds.get(i + 1).map_or(content.len(), |b| b.0);
        let text = &content[start..end];
        if text.trim().is_empty() {
            continue;
        }
        chunks.push(Chunk {
            heading: bounds[i].1.clone(),
            start,
            end,
            text: text.to_string(),
        });
    }
    chunks
}

/// If `line` is an ATX heading (`#`..`######` then whitespace), return its text.
fn heading_text(line: &str) -> Option<String> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ') {
        Some(line[hashes..].trim().to_string())
    } else {
        None
    }
}

/// Embed `texts` in one OpenAI request; returns vectors in input order.
fn embed_texts(api_key: &str, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let resp = ureq::post("https://api.openai.com/v1/embeddings")
        .set("Authorization", &format!("Bearer {api_key}"))
        .send_json(ureq::json!({ "model": MODEL, "input": texts }))
        .context("OpenAI embeddings request")?;
    let body: serde_json::Value = resp.into_json().context("parsing OpenAI response")?;
    let data = body["data"].as_array().context("OpenAI response has no `data`")?;
    let mut out = vec![Vec::new(); data.len()];
    for item in data {
        let idx = item["index"].as_u64().context("embedding item missing `index`")? as usize;
        let emb = item["embedding"]
            .as_array()
            .context("embedding item missing `embedding`")?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();
        *out.get_mut(idx).context("OpenAI returned an out-of-range index")? = emb;
    }
    Ok(out)
}

/// pgvector text literal: `[0.1,0.2,…]`.
fn vector_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

/// Embed one blob: read its bytes from the version clone, chunk + embed, write
/// `blob_chunks`. Non-UTF-8 or blank content gets a single sentinel row (null
/// embedding) so it isn't reprocessed every run. Returns the number of embedded
/// chunks (0 = sentinel).
pub fn embed_blob(fs: &Fs, versions: &mut VersionStore, api_key: &str, hash: &str) -> Result<usize> {
    let bytes = fs
        .read_all(&format!("{VERSIONS_DIR}/{hash}"))
        .with_context(|| format!("reading version clone for {hash}"))?;

    let text = match std::str::from_utf8(&bytes) {
        Ok(t) if !t.trim().is_empty() => t,
        // Binary or empty: mark processed-but-not-embedded so it stops being pending.
        _ => {
            versions.replace_chunks(
                hash,
                MODEL,
                &[ChunkInsert { ordinal: 0, heading: None, start_byte: 0, end_byte: bytes.len() as i32, embedding: None }],
            )?;
            return Ok(0);
        }
    };

    let chunks = chunk_markdown(text);
    let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
    let embeddings = embed_texts(api_key, &texts)?;

    let rows: Vec<ChunkInsert> = chunks
        .iter()
        .zip(&embeddings)
        .enumerate()
        .map(|(i, (c, emb))| ChunkInsert {
            ordinal: i as i32,
            heading: c.heading.as_deref(),
            start_byte: c.start as i32,
            end_byte: c.end as i32,
            embedding: Some(vector_literal(emb)),
        })
        .collect();
    versions.replace_chunks(hash, MODEL, &rows)?;
    Ok(rows.len())
}

/// Drain every pending blob (no chunks yet) and exit. Cron-friendly. Returns the
/// number of blobs processed.
pub fn run_once(fs: &Fs, versions: &mut VersionStore, api_key: &str) -> Result<usize> {
    let mut processed = 0;
    loop {
        let pending = versions.pending_embedding_hashes(BATCH)?;
        if pending.is_empty() {
            break;
        }
        for hash in &pending {
            embed_blob(fs, versions, api_key, hash)
                .with_context(|| format!("embedding blob {hash}"))?;
            processed += 1;
        }
    }
    Ok(processed)
}

/// Sweep on an interval forever: embed anything pending, then sleep. Latency is
/// bounded by `interval`. (A future upgrade fires a `pg_notify` from `commit()`
/// and `LISTEN`s here for near-instant wake; polling is the robust v1.)
pub fn run_watch(fs: &Fs, versions: &mut VersionStore, api_key: &str, interval: Duration) -> Result<()> {
    loop {
        if let Err(e) = run_once(fs, versions, api_key) {
            eprintln!("trove embed: pass deferred: {e:#}");
        }
        std::thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_then_headed_sections() {
        let md = "intro line\n\n## First\nalpha\n\n## Second\nbeta\n";
        let c = chunk_markdown(md);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].heading, None);
        assert_eq!(c[1].heading.as_deref(), Some("First"));
        assert_eq!(c[2].heading.as_deref(), Some("Second"));
        // Ranges partition the document and round-trip the bytes.
        assert_eq!(&md[c[1].start..c[1].end], c[1].text);
        assert!(c[2].text.contains("beta"));
    }

    #[test]
    fn file_opening_with_a_heading_has_no_empty_preamble() {
        let c = chunk_markdown("# Title\nbody\n");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].heading.as_deref(), Some("Title"));
    }

    #[test]
    fn no_headings_is_one_chunk() {
        let c = chunk_markdown("just\nsome\nprose\n");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].heading, None);
    }

    #[test]
    fn blank_content_yields_no_chunks() {
        assert!(chunk_markdown("   \n\n").is_empty());
    }

    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        let c = chunk_markdown("#nospace is text\nmore\n");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].heading, None);
    }
}
