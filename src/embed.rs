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

// Paragraph chunking sizes. No tokenizer dependency — we approximate at the
// well-known ~4 chars per token, so these map to roughly 100 and 1000 tokens.
// MAX stays well under `text-embedding-3-large`'s ~8191-token input cap.
const MIN_CHUNK_CHARS: usize = 400;
const MAX_CHUNK_CHARS: usize = 4000;

/// A paragraph block — its end offset, plus heading classification. (The start
/// is implicit: the previous block's end, tracked by the caller.)
struct Block {
    end: usize,
    /// The block is ONLY an ATX heading line (so it must attach to what follows).
    is_heading_only: bool,
    /// Heading text if the block opens with an ATX heading.
    heading: Option<String>,
}

/// Paragraph-based chunking with size caps and heading attachment — the
/// recommended default (set `TROVE_CHUNK_STRATEGY=heading` for the older
/// split-at-every-heading behaviour). Splits on blank lines, then:
/// clusters consecutive paragraphs until a chunk reaches `MIN_CHUNK_CHARS`,
/// never exceeding `MAX_CHUNK_CHARS` (a single over-max paragraph is hard-split
/// at a char/newline boundary), and keeps a lone heading line attached to the
/// paragraph that follows it (never a heading-only chunk while content follows).
/// Byte ranges partition the emitted spans and round-trip (`&content[s..e] == text`).
pub fn chunk_paragraphs(content: &str) -> Vec<Chunk> {
    let blocks = paragraph_blocks(content);
    if blocks.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut heading: Option<String> = None;
    for (i, b) in blocks.iter().enumerate() {
        if heading.is_none() {
            heading = b.heading.clone();
        }
        let cur = b.end - start;
        let last = i + 1 == blocks.len();
        let next_would_exceed = !last && (blocks[i + 1].end - start) > MAX_CHUNK_CHARS;
        // Close after this block when it's the last; or it isn't a lone heading
        // (keep a heading with its following paragraph) AND we've reached MIN or
        // adding the next block would bust MAX.
        let close = last || (!b.is_heading_only && (cur >= MIN_CHUNK_CHARS || next_would_exceed));
        if close {
            emit(&mut chunks, content, start, b.end, heading.take());
            start = b.end;
        }
    }
    chunks
}

/// Push a chunk for `[start, end)`, hard-splitting it into `<= MAX_CHUNK_CHARS`
/// pieces (at a char boundary, preferring a late newline) if it's oversized.
/// Whitespace-only spans are dropped. Only the first piece keeps the heading.
fn emit(chunks: &mut Vec<Chunk>, content: &str, start: usize, end: usize, heading: Option<String>) {
    if content[start..end].trim().is_empty() {
        return;
    }
    let mut h = heading;
    let mut s = start;
    while s < end {
        let mut e = (s + MAX_CHUNK_CHARS).min(end);
        while e > s && !content.is_char_boundary(e) {
            e -= 1;
        }
        // Prefer breaking on a newline in the last fifth, for cleaner chunks.
        if e < end {
            if let Some(nl) = content[s..e].rfind('\n') {
                if nl * 5 > (e - s) * 4 {
                    e = s + nl + 1;
                }
            }
        }
        if e <= s {
            break; // no progress possible (pathological); bail rather than loop
        }
        if !content[s..e].trim().is_empty() {
            chunks.push(Chunk { heading: h.take(), start: s, end: e, text: content[s..e].to_string() });
        }
        s = e;
    }
}

/// Tile `content` into paragraph blocks (separated by blank lines), contiguous
/// and covering `[0, len)` — leading blank lines fold into the first block.
fn paragraph_blocks(content: &str) -> Vec<Block> {
    let mut starts = Vec::new();
    let mut off = 0usize;
    let mut prev_blank = true; // document start behaves like "after a blank line"
    for line in content.split_inclusive('\n') {
        let blank = line.trim().is_empty();
        if !blank && prev_blank {
            starts.push(off);
        }
        prev_blank = blank;
        off += line.len();
    }
    if starts.is_empty() {
        return Vec::new(); // entirely blank
    }
    // Boundaries tile the whole document: force the first to 0.
    let mut bounds = Vec::with_capacity(starts.len() + 1);
    bounds.push(0);
    bounds.extend(starts.iter().skip(1).copied());
    bounds.push(content.len());

    bounds
        .windows(2)
        .map(|w| {
            let (s, e) = (w[0], w[1]);
            let (heading, is_heading_only) = classify(&content[s..e]);
            Block { end: e, is_heading_only, heading }
        })
        .collect()
}

/// Classify a block's text: `(heading text if it opens with an ATX heading,
/// whether the block is ONLY that heading line)`.
fn classify(text: &str) -> (Option<String>, bool) {
    let mut heading = None;
    let mut other = false;
    let mut first = true;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if first {
            first = false;
            if let Some(h) = heading_text(line.trim_start()) {
                heading = Some(h);
                continue;
            }
        }
        other = true;
    }
    let is_heading_only = heading.is_some() && !other;
    (heading, is_heading_only)
}

/// Chunk `content` using the configured strategy (paragraph by default;
/// `TROVE_CHUNK_STRATEGY=heading` for split-at-every-heading). This is what the
/// embed path uses, so the strategy is one switch in one place.
pub fn chunk(content: &str) -> Vec<Chunk> {
    match std::env::var("TROVE_CHUNK_STRATEGY").as_deref() {
        Ok("heading") => chunk_markdown(content),
        _ => chunk_paragraphs(content),
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

/// Embed `content` for blob `hash`: chunk by header, embed each, write
/// `blob_chunks`. Non-UTF-8 or blank content gets a single sentinel row (null
/// embedding) so it isn't reprocessed. Returns the number of embedded chunks
/// (0 = sentinel). No filesystem access — the caller supplies the bytes (the
/// commit path has them in the write buffer; the cron path reads the clone).
pub fn embed_content(
    versions: &mut VersionStore,
    api_key: &str,
    hash: &str,
    content: &[u8],
) -> Result<usize> {
    let text = match std::str::from_utf8(content) {
        Ok(t) if !t.trim().is_empty() => t,
        // Binary or empty: mark processed-but-not-embedded.
        _ => {
            versions.replace_chunks(
                hash,
                MODEL,
                &[ChunkInsert { ordinal: 0, heading: None, start_byte: 0, end_byte: content.len() as i32, embedding: None }],
            )?;
            return Ok(0);
        }
    };

    let chunks = chunk(text);
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

/// Embed a search query into a pgvector text literal, ready to hand straight to
/// [`crate::version::VersionStore::search_chunks`]. Uses the SAME model as the
/// stored chunks — search only works if query and corpus share a vector space.
/// A query is one short text, so this is a single one-element OpenAI request.
pub fn embed_query_literal(api_key: &str, query: &str) -> Result<String> {
    let mut v = embed_texts(api_key, &[query.to_string()])?;
    let emb = v.pop().context("OpenAI returned no embedding for the query")?;
    Ok(vector_literal(&emb))
}

/// Embed one blob by reading its bytes from the version clone via libjfs, then
/// [`embed_content`]. The cron/backfill path (no write buffer to hand).
pub fn embed_blob(fs: &Fs, versions: &mut VersionStore, api_key: &str, hash: &str) -> Result<usize> {
    let bytes = fs
        .read_all(&format!("{VERSIONS_DIR}/{hash}"))
        .with_context(|| format!("reading version clone for {hash}"))?;
    embed_content(versions, api_key, hash, &bytes)
}

/// Spawn a background thread that embeds `(hash, content)` pairs as they arrive,
/// owning its own DB connection + key. Returns the sender that the mount's
/// `commit()` pushes to — so embedding self-triggers on write (no cron, no
/// daemon), runs off the write path, and reuses the buffer (no libjfs read).
/// Connects up front so a bad URL fails at mount start, not silently later.
pub fn spawn_embedder(
    versions_url: &str,
    api_key: String,
) -> Result<std::sync::mpsc::Sender<(String, Vec<u8>)>> {
    let mut versions = VersionStore::connect(versions_url)?;
    let (tx, rx) = std::sync::mpsc::channel::<(String, Vec<u8>)>();
    std::thread::spawn(move || {
        for (hash, content) in rx {
            if let Err(e) = embed_content(&mut versions, &api_key, &hash, &content) {
                eprintln!("trove embed: {hash}: {e:#}");
            }
        }
    });
    Ok(tx)
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

    // --- paragraph chunker ---

    /// Every emitted chunk's byte range round-trips to its text and the chunks
    /// are ordered + non-overlapping.
    fn assert_well_formed(content: &str, chunks: &[Chunk]) {
        let mut prev_end = 0;
        for c in chunks {
            assert_eq!(&content[c.start..c.end], c.text, "range must round-trip");
            assert!(c.start >= prev_end, "chunks ordered + non-overlapping");
            prev_end = c.end;
        }
    }

    #[test]
    fn short_paragraphs_cluster_into_one_chunk() {
        let md = "one short para\n\nanother short para\n\na third\n";
        let c = chunk_paragraphs(md);
        assert_eq!(c.len(), 1, "small paras (< MIN) cluster together");
        assert!(c[0].text.contains("third"));
        assert_well_formed(md, &c);
    }

    #[test]
    fn heading_travels_with_the_following_paragraph() {
        let md = "## Section\n\nbody paragraph under the heading\n";
        let c = chunk_paragraphs(md);
        assert_eq!(c.len(), 1, "no orphan heading-only chunk");
        assert_eq!(c[0].heading.as_deref(), Some("Section"));
        assert!(c[0].text.contains("body paragraph"), "heading chunk includes the para");
        assert_well_formed(md, &c);
    }

    #[test]
    fn oversized_paragraph_is_hard_split_under_max() {
        // One blank-line-free paragraph far larger than MAX.
        let big = "word ".repeat(MAX_CHUNK_CHARS); // ~5 * MAX chars, no blank lines
        let c = chunk_paragraphs(&big);
        assert!(c.len() > 1, "an over-max paragraph splits into several chunks");
        for chunk in &c {
            assert!(chunk.text.len() <= MAX_CHUNK_CHARS, "each piece is <= MAX");
        }
        assert_well_formed(&big, &c);
    }

    #[test]
    fn long_document_splits_at_paragraph_boundaries_near_min() {
        // Several paragraphs each ~MIN/2, so chunks form around MIN and break on
        // paragraph boundaries (not mid-paragraph).
        let para = format!("{}\n", "x".repeat(MIN_CHUNK_CHARS / 2));
        let doc = vec![para.clone(); 8].join("\n");
        let c = chunk_paragraphs(&doc);
        assert!(c.len() >= 2, "a long doc yields multiple chunks");
        for chunk in &c {
            assert!(chunk.text.len() <= MAX_CHUNK_CHARS);
        }
        assert_well_formed(&doc, &c);
    }

    #[test]
    fn chunk_selector_honours_the_strategy_env() {
        let md = "# A\nalpha\n\n# B\nbeta\n";
        std::env::set_var("TROVE_CHUNK_STRATEGY", "heading");
        assert_eq!(chunk(md).len(), chunk_markdown(md).len());
        std::env::remove_var("TROVE_CHUNK_STRATEGY");
        // default = paragraph: these two tiny headed paras cluster into one.
        assert_eq!(chunk(md).len(), chunk_paragraphs(md).len());
    }
}
