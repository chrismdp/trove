//! `trove server` — a **single-tenant, read-only** HTTP view of one Trove store.
//!
//! Bound to `127.0.0.1` only (never `0.0.0.0`): external access is the operator's
//! job via an nginx reverse proxy, where TLS and any auth terminate. That's why
//! there's deliberately **no auth and no write API in v1** — it's the "view app
//! on my own VPS for my own vault" surface, not a SaaS.
//!
//! Routes:
//!   GET /                 — minimal HTML browser (file list + search box)
//!   GET /files            — JSON: every known path (from the version chain)
//!   GET /file/<path>      — current bytes of <path> (via libjfs)
//!   GET /search?q=...     — JSON: semantic search hits (embeds q via OpenAI)
//!
//! Requests are served sequentially on one thread — a personal localhost view
//! has trivial traffic, and the single Postgres connection + libjfs handle are
//! simplest reused, not shared across threads.

use crate::jfs::Fs;
use crate::version::VersionStore;
use anyhow::{anyhow, Result};
use tiny_http::{Header, Response, Server};

/// Start the server, blocking forever (Ctrl-C to stop). `api_key` is used only
/// to embed search queries; `/files` and `/file` need just the DB + libjfs.
pub fn run(fs: &Fs, versions: &mut VersionStore, api_key: &str, port: u16) -> Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let server = Server::http(&addr).map_err(|e| anyhow!("binding {addr}: {e}"))?;
    println!("trove server: http://{addr}  (localhost only — front with nginx for external access)");

    for req in server.incoming_requests() {
        let (status, content_type, body) = route(fs, versions, api_key, req.url());
        let header = Header::from_bytes(b"Content-Type".as_ref(), content_type.as_bytes())
            .expect("valid header");
        let response = Response::from_data(body).with_status_code(status).with_header(header);
        let _ = req.respond(response);
    }
    Ok(())
}

/// Route a request to (status, content-type, body). Pure over the URL so it's
/// testable without a socket.
pub fn route(fs: &Fs, versions: &mut VersionStore, api_key: &str, url: &str) -> (u16, &'static str, Vec<u8>) {
    let path = url.split('?').next().unwrap_or("/");
    match path {
        "/" | "/index.html" => (200, "text/html; charset=utf-8", INDEX_HTML.as_bytes().to_vec()),
        "/files" => match versions.paths() {
            Ok(paths) => (200, "application/json", json_string_array(&paths).into_bytes()),
            Err(e) => server_error(e),
        },
        "/search" => {
            let q = query_param(url, "q").unwrap_or_default();
            if q.is_empty() {
                return (400, "application/json", br#"{"error":"missing q"}"#.to_vec());
            }
            match search_json(versions, api_key, &q) {
                Ok(body) => (200, "application/json", body.into_bytes()),
                Err(e) => server_error(e),
            }
        }
        p if p.starts_with("/file/") => {
            // Everything after "/file" is the in-volume path (leading slash kept).
            let file_path = percent_decode(&p["/file".len()..]);
            match fs.read_all(&file_path) {
                Ok(bytes) => (200, "text/plain; charset=utf-8", bytes),
                Err(_) => (404, "text/plain; charset=utf-8", format!("not found: {file_path}").into_bytes()),
            }
        }
        _ => (404, "text/plain; charset=utf-8", b"not found".to_vec()),
    }
}

fn server_error(e: anyhow::Error) -> (u16, &'static str, Vec<u8>) {
    (500, "application/json", format!("{{\"error\":{}}}", json_quote(&format!("{e:#}"))).into_bytes())
}

/// Embed the query and return search hits as a JSON array of {path,heading,score}.
fn search_json(versions: &mut VersionStore, api_key: &str, q: &str) -> Result<String> {
    let literal = crate::embed::embed_query_literal(api_key, q)?;
    let hits = versions.search_chunks(&literal, 20)?;
    let mut out = String::from("[");
    for (i, h) in hits.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let heading = h.heading.as_deref().map(json_quote).unwrap_or_else(|| "null".to_string());
        out.push_str(&format!(
            "{{\"path\":{},\"heading\":{},\"score\":{:.4}}}",
            json_quote(&h.path),
            heading,
            1.0 - h.distance
        ));
    }
    out.push(']');
    Ok(out)
}

/// A JSON array of strings (manual — avoids pulling serde into the hot path).
fn json_string_array(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_quote(s));
    }
    out.push(']');
    out
}

/// Minimal JSON string escaping (quotes, backslashes, control chars).
fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Extract a query-string parameter value (percent-decoded).
fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

/// Decode `%XX` escapes and `+` (form encoding) in a URL component.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The single-page browser. Vanilla JS hitting /files and /search — no build step.
const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Trove</title>
<style>
 body{font:16px/1.5 system-ui,sans-serif;max-width:52rem;margin:2rem auto;padding:0 1rem;color:#222}
 h1{font-size:1.4rem} input{width:100%;padding:.6rem;font-size:1rem;box-sizing:border-box}
 .hit{padding:.5rem 0;border-bottom:1px solid #eee} .score{color:#888;font-variant-numeric:tabular-nums}
 .heading{color:#555} a{color:#0645ad;text-decoration:none} a:hover{text-decoration:underline}
 .muted{color:#888;font-size:.9rem} ul{padding-left:1.1rem}
</style></head><body>
<h1>Trove <span class="muted">— a filesystem that talks back</span></h1>
<input id="q" placeholder="Search your vault… (semantic)" autofocus>
<div id="results"></div>
<h2 style="font-size:1.1rem">Files</h2>
<ul id="files"><li class="muted">loading…</li></ul>
<script>
 const q=document.getElementById('q'),results=document.getElementById('results'),files=document.getElementById('files');
 let t;
 q.addEventListener('input',()=>{clearTimeout(t);t=setTimeout(search,300)});
 async function search(){
   const v=q.value.trim(); if(!v){results.innerHTML='';return}
   results.innerHTML='<p class="muted">searching…</p>';
   try{const r=await fetch('/search?q='+encodeURIComponent(v));const hits=await r.json();
     results.innerHTML=hits.length?hits.map(h=>
       `<div class="hit"><span class="score">${h.score.toFixed(3)}</span>
        <a href="/file${encodeURI(h.path)}">${h.path}</a>
        ${h.heading?'<span class="heading">› '+h.heading+'</span>':''}</div>`).join('')
       :'<p class="muted">no matches</p>';
   }catch(e){results.innerHTML='<p class="muted">error: '+e+'</p>'}
 }
 (async()=>{try{const r=await fetch('/files');const ps=await r.json();
   files.innerHTML=ps.length?ps.map(p=>`<li><a href="/file${encodeURI(p)}">${p}</a></li>`).join(''):'<li class="muted">no files yet</li>';
 }catch(e){files.innerHTML='<li class="muted">error loading files</li>'}})();
</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b+c"), "a b c");
        assert_eq!(percent_decode("/people/alice.md"), "/people/alice.md");
        assert_eq!(percent_decode("%2Ffoo"), "/foo");
    }

    #[test]
    fn query_param_extraction() {
        assert_eq!(query_param("/search?q=knee%20pain", "q").as_deref(), Some("knee pain"));
        assert_eq!(query_param("/search?x=1&q=cats", "q").as_deref(), Some("cats"));
        assert_eq!(query_param("/search", "q"), None);
    }

    #[test]
    fn json_helpers_escape() {
        assert_eq!(json_quote(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(json_string_array(&["a".into(), "b".into()]), r#"["a","b"]"#);
    }
}
