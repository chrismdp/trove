//! `trove docs` — the embedded walkthrough, available two ways.
//!
//! The `docs/` directory at the crate root is baked into the binary at compile
//! time via `rust-embed`. Most of the time you just want to *read* a page, so
//! the default is to **print markdown to stdout** ([`page_markdown`],
//! [`all_markdown`], [`index_text`]) — no server, no browser, nothing to kill
//! afterwards. This is what an agent reaches for: `trove docs quickstart` or
//! `trove docs --all` and it has the manual in one read.
//!
//! [`serve`] is the richer path for a human: it renders the same markdown to
//! HTML with `pulldown-cmark` and serves it inside a fumadocs-style shell
//! (sidebar from `meta.toml`, content pane, right-side table of contents,
//! client-side search across all pages).
//!
//! No native deps, no Postgres, no libjfs. `trove check`-only installs get the
//! docs for free.

use anyhow::{anyhow, Context, Result};
use pulldown_cmark::{html, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::borrow::Cow;
use tiny_http::{Header, Response, Server};

#[derive(RustEmbed)]
#[folder = "docs/"]
struct DocsAssets;

#[derive(Debug, Deserialize)]
pub struct Meta {
    sections: Vec<Section>,
}

#[derive(Debug, Deserialize)]
pub struct Section {
    title: String,
    pages: Vec<Page>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Page {
    slug: String,
    title: String,
}

/// Raw markdown for a single page, by slug (the same slug used in the URL and
/// in `meta.toml`). Returns an error listing the valid slugs on a miss — so a
/// typo is self-correcting rather than a bare 404.
pub fn page_markdown(slug: &str) -> Result<String> {
    let file = DocsAssets::get(&format!("{slug}.md")).ok_or_else(|| {
        let available = load_meta()
            .map(|m| {
                m.sections
                    .iter()
                    .flat_map(|s| s.pages.iter())
                    .map(|p| p.slug.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        anyhow!("no docs page {slug:?}. Available pages: {available}")
    })?;
    let md = std::str::from_utf8(&file.data).context("docs page is not UTF-8")?;
    Ok(md.to_string())
}

/// Every page concatenated in nav order, each preceded by an HTML comment
/// naming its source file so the boundaries are visible. Built for piping the
/// whole manual into an agent in one read.
pub fn all_markdown() -> Result<String> {
    let meta = load_meta()?;
    let mut out = String::new();
    for section in &meta.sections {
        for page in &section.pages {
            let Some(file) = DocsAssets::get(&format!("{}.md", page.slug)) else {
                continue;
            };
            let Ok(md) = std::str::from_utf8(&file.data) else {
                continue;
            };
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&format!("<!-- docs/{}.md · {} -->\n\n", page.slug, page.title));
            out.push_str(md);
            if !md.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    Ok(out)
}

/// A plain-text index of every page (section headers + slug + title) plus a
/// usage hint. Printed when `trove docs` is run with no page and no `--serve`.
pub fn index_text() -> Result<String> {
    let meta = load_meta()?;
    let width = meta
        .sections
        .iter()
        .flat_map(|s| &s.pages)
        .map(|p| p.slug.len())
        .max()
        .unwrap_or(0);
    let mut out = String::from("trove docs — bundled manual\n\n");
    out.push_str("  trove docs <slug>     print one page's markdown\n");
    out.push_str("  trove docs --all      print the whole manual\n");
    out.push_str("  trove docs --serve    open the browser UI (http://127.0.0.1:38081)\n\n");
    for section in &meta.sections {
        out.push_str(&section.title);
        out.push('\n');
        for page in &section.pages {
            out.push_str(&format!("  {:<width$}  {}\n", page.slug, page.title, width = width));
        }
        out.push('\n');
    }
    Ok(out)
}

/// Start the docs server, blocking until Ctrl-C.
pub fn serve(port: u16) -> Result<()> {
    let meta = load_meta()?;
    let addr = format!("127.0.0.1:{port}");
    let server = Server::http(&addr).map_err(|e| anyhow!("binding {addr}: {e}"))?;
    println!("trove docs: http://{addr}  (Ctrl-C to stop)");

    for req in server.incoming_requests() {
        let (status, content_type, body) = route(&meta, req.url());
        let header = Header::from_bytes(b"Content-Type".as_ref(), content_type.as_bytes())
            .expect("valid header");
        let response = Response::from_data(body).with_status_code(status).with_header(header);
        let _ = req.respond(response);
    }
    Ok(())
}

fn load_meta() -> Result<Meta> {
    let file = DocsAssets::get("meta.toml")
        .ok_or_else(|| anyhow!("docs/meta.toml missing from binary embed"))?;
    let text = std::str::from_utf8(&file.data).context("meta.toml is not UTF-8")?;
    toml::from_str::<Meta>(text).context("parsing docs/meta.toml")
}

/// Pure URL → response. Easy to unit-test without a socket.
pub fn route(meta: &Meta, url: &str) -> (u16, &'static str, Vec<u8>) {
    let path = url.split('?').next().unwrap_or("/");
    match path {
        "/" => redirect_to("/docs/intro"),
        "/search-index.json" => (
            200,
            "application/json",
            search_index_json(meta).into_bytes(),
        ),
        "/styles.css" => (200, "text/css; charset=utf-8", STYLES.as_bytes().to_vec()),
        "/app.js" => (200, "application/javascript", APP_JS.as_bytes().to_vec()),
        p if p.starts_with("/docs/") => {
            let slug = &p["/docs/".len()..];
            match render_page(meta, slug) {
                Some(html) => (200, "text/html; charset=utf-8", html.into_bytes()),
                None => (404, "text/html; charset=utf-8", not_found(meta, slug).into_bytes()),
            }
        }
        _ => (404, "text/html; charset=utf-8", not_found(meta, path).into_bytes()),
    }
}

fn redirect_to(target: &str) -> (u16, &'static str, Vec<u8>) {
    // Cheap browser redirect via meta refresh (we don't have a Location-header
    // helper at this level and the alternative is reworking the route signature).
    let body = format!(
        "<!doctype html><meta http-equiv=refresh content=\"0; url={target}\"><a href=\"{target}\">docs</a>"
    );
    (200, "text/html; charset=utf-8", body.into_bytes())
}

/// Look up a slug, render its markdown, wrap in the shell.
fn render_page(meta: &Meta, slug: &str) -> Option<String> {
    let file = DocsAssets::get(&format!("{slug}.md"))?;
    let md = std::str::from_utf8(&file.data).ok()?;

    let (body_html, toc) = markdown_to_html_with_toc(md);
    let page_title = meta
        .sections
        .iter()
        .flat_map(|s| &s.pages)
        .find(|p| p.slug == slug)
        .map(|p| p.title.clone())
        .unwrap_or_else(|| slug.to_string());

    Some(shell(meta, slug, &page_title, &body_html, &toc))
}

fn not_found(meta: &Meta, slug: &str) -> String {
    let body = format!(
        "<h1>Not found</h1><p><code>{}</code> is not a known docs page.</p>",
        escape_html(slug)
    );
    shell(meta, "", "Not found", &body, "")
}

/// Walk the markdown twice: once to render to HTML, once to collect H2/H3
/// headings for the right-rail table of contents. We anchor each heading by a
/// kebab-cased slug of its plain-text content.
fn markdown_to_html_with_toc(md: &str) -> (String, String) {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);

    // First pass: collect (level, text, anchor) for h2/h3.
    let mut toc: Vec<(u8, String, String)> = Vec::new();
    {
        let parser = Parser::new_ext(md, opts);
        let mut in_heading: Option<HeadingLevel> = None;
        let mut buf = String::new();
        for ev in parser {
            match ev {
                Event::Start(Tag::Heading { level, .. }) => {
                    in_heading = Some(level);
                    buf.clear();
                }
                Event::End(TagEnd::Heading(_)) => {
                    if let Some(level) = in_heading.take() {
                        let lvl = match level {
                            HeadingLevel::H2 => 2u8,
                            HeadingLevel::H3 => 3,
                            _ => continue,
                        };
                        let text = buf.trim().to_string();
                        let anchor = slugify(&text);
                        toc.push((lvl, text, anchor));
                    }
                }
                Event::Text(t) | Event::Code(t) => {
                    if in_heading.is_some() {
                        buf.push_str(&t);
                    }
                }
                _ => {}
            }
        }
    }

    // Second pass: render to HTML, injecting `id` attributes on h2/h3 so the
    // TOC anchors line up. pulldown-cmark doesn't emit IDs itself; we patch
    // them in by rewriting events.
    let mut anchor_iter = toc.iter().filter(|(l, _, _)| *l == 2 || *l == 3);
    let parser = Parser::new_ext(md, opts).map(|ev| match ev {
        Event::Start(Tag::Heading { level: HeadingLevel::H2, .. })
        | Event::Start(Tag::Heading { level: HeadingLevel::H3, .. }) => {
            // The next anchor in the queue belongs to this heading.
            let next = anchor_iter.next();
            match (ev, next) {
                (Event::Start(Tag::Heading { level, id: _, classes, attrs }), Some((_, _, anchor))) => {
                    Event::Start(Tag::Heading {
                        level,
                        id: Some(anchor.clone().into()),
                        classes,
                        attrs,
                    })
                }
                (other, _) => other,
            }
        }
        other => other,
    });
    let mut html_out = String::new();
    html::push_html(&mut html_out, parser);

    let toc_html = render_toc(&toc);
    (html_out, toc_html)
}

fn render_toc(toc: &[(u8, String, String)]) -> String {
    if toc.is_empty() {
        return String::new();
    }
    let mut out = String::from("<nav class=\"toc\"><div class=\"toc-title\">On this page</div><ul>");
    for (level, text, anchor) in toc {
        let cls = if *level == 3 { "toc-h3" } else { "toc-h2" };
        out.push_str(&format!(
            "<li class=\"{cls}\"><a href=\"#{anchor}\">{}</a></li>",
            escape_html(text)
        ));
    }
    out.push_str("</ul></nav>");
    out
}

/// Lowercase, replace anything non-alphanumeric with `-`, collapse runs.
fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_dash = true; // suppress leading dashes
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The HTML shell: sidebar (from meta), main content, right-rail TOC.
fn shell(meta: &Meta, current: &str, page_title: &str, content: &str, toc: &str) -> String {
    let sidebar = render_sidebar(meta, current);
    let title = escape_html(page_title);
    format!(
        r#"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · Trove docs</title>
<link rel="stylesheet" href="/styles.css">
</head><body>
<header class="topbar">
  <a class="brand" href="/docs/intro">trove<span class="brand-sub"> docs</span></a>
  <div class="search-wrap">
    <input id="search" type="search" placeholder="Search the docs…" autocomplete="off">
    <div id="search-results"></div>
  </div>
</header>
<div class="layout">
  <aside class="sidebar">{sidebar}</aside>
  <main class="content">
    <article>{content}</article>
  </main>
  <aside class="toc-wrap">{toc}</aside>
</div>
<script src="/app.js"></script>
</body></html>"#
    )
}

fn render_sidebar(meta: &Meta, current: &str) -> String {
    let mut out = String::new();
    for section in &meta.sections {
        out.push_str(&format!(
            "<div class=\"sb-section\"><div class=\"sb-title\">{}</div><ul>",
            escape_html(&section.title)
        ));
        for page in &section.pages {
            let active = if page.slug == current { " active" } else { "" };
            out.push_str(&format!(
                "<li><a class=\"sb-link{active}\" href=\"/docs/{slug}\">{title}</a></li>",
                slug = escape_html(&page.slug),
                title = escape_html(&page.title),
            ));
        }
        out.push_str("</ul></div>");
    }
    out
}

fn escape_html(s: &str) -> Cow<'_, str> {
    if !s.contains(['<', '>', '&', '"', '\'']) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Build the client-side search index: every page's slug, title, and raw text.
/// The browser does the substring match (the corpus is small).
fn search_index_json(meta: &Meta) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for section in &meta.sections {
        for page in &section.pages {
            let Some(file) = DocsAssets::get(&format!("{}.md", page.slug)) else {
                continue;
            };
            let Ok(text) = std::str::from_utf8(&file.data) else {
                continue;
            };
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&format!(
                "{{\"slug\":{},\"title\":{},\"section\":{},\"body\":{}}}",
                json_quote(&page.slug),
                json_quote(&page.title),
                json_quote(&section.title),
                json_quote(text),
            ));
        }
    }
    out.push(']');
    out
}

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

/// Fumadocs-inspired styles. Dark sidebar, light content. Monospace code,
/// readable prose width, a sticky right-rail TOC. One file, no build step.
const STYLES: &str = r#"
*,*::before,*::after { box-sizing: border-box; }
html,body { margin: 0; padding: 0; }
body {
  font: 15px/1.6 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
  color: #1a1a1a;
  background: #fafafa;
}
a { color: #0066cc; text-decoration: none; }
a:hover { text-decoration: underline; }

.topbar {
  position: sticky; top: 0; z-index: 10;
  display: flex; align-items: center; gap: 1.5rem;
  height: 56px; padding: 0 1.5rem;
  background: #18181b; color: #fafafa;
  border-bottom: 1px solid #27272a;
}
.brand { color: #fafafa; font-weight: 700; font-size: 1.1rem; }
.brand-sub { color: #71717a; font-weight: 500; }
.search-wrap { position: relative; flex: 1; max-width: 420px; }
#search {
  width: 100%; padding: .5rem .75rem;
  background: #27272a; border: 1px solid #3f3f46; border-radius: 6px;
  color: #fafafa; font: inherit;
}
#search:focus { outline: none; border-color: #0066cc; }
#search-results {
  position: absolute; top: 100%; left: 0; right: 0; margin-top: 4px;
  background: #fff; color: #1a1a1a; border: 1px solid #e4e4e7; border-radius: 6px;
  max-height: 60vh; overflow-y: auto;
  box-shadow: 0 10px 25px rgba(0,0,0,.15);
  display: none;
}
#search-results.open { display: block; }
.search-hit { padding: .5rem .75rem; border-bottom: 1px solid #f4f4f5; cursor: pointer; }
.search-hit:hover, .search-hit.selected { background: #f4f4f5; }
.search-hit .h-title { font-weight: 600; }
.search-hit .h-section { color: #71717a; font-size: .85rem; }
.search-hit .h-snippet { color: #52525b; font-size: .85rem; margin-top: .15rem; }
.search-hit mark { background: #fef08a; padding: 0 2px; border-radius: 2px; }

.layout {
  display: grid;
  grid-template-columns: 260px 1fr 220px;
  max-width: 1400px;
  margin: 0 auto;
}

.sidebar {
  padding: 1.5rem 1rem 4rem 1.5rem;
  border-right: 1px solid #e4e4e7;
  min-height: calc(100vh - 56px);
  background: #fafafa;
  position: sticky; top: 56px;
  align-self: start;
  max-height: calc(100vh - 56px);
  overflow-y: auto;
}
.sb-section { margin-bottom: 1.5rem; }
.sb-title { font-size: .75rem; text-transform: uppercase; letter-spacing: .04em;
  color: #71717a; font-weight: 600; margin: 0 0 .5rem .5rem; }
.sb-section ul { list-style: none; padding: 0; margin: 0; }
.sb-link {
  display: block; padding: .35rem .5rem;
  color: #3f3f46; border-radius: 4px;
  font-size: .9rem;
}
.sb-link:hover { background: #f4f4f5; text-decoration: none; }
.sb-link.active { background: #e0e7ff; color: #1e40af; font-weight: 500; }

.content {
  padding: 2rem 3rem;
  min-width: 0; /* let pre/code overflow correctly */
  background: #fff;
}
.content article { max-width: 720px; }

.content h1 { font-size: 1.9rem; margin-top: 0; line-height: 1.2; }
.content h2 { font-size: 1.4rem; margin-top: 2rem; border-bottom: 1px solid #e4e4e7;
  padding-bottom: .3rem; scroll-margin-top: 72px; }
.content h3 { font-size: 1.1rem; margin-top: 1.5rem; scroll-margin-top: 72px; }
.content p { margin: .75rem 0; }
.content ul, .content ol { padding-left: 1.5rem; }
.content li { margin: .2rem 0; }

.content code {
  font-family: "SF Mono", Menlo, Monaco, Consolas, monospace;
  font-size: .9em;
  background: #f4f4f5;
  padding: .1em .35em;
  border-radius: 3px;
}
.content pre {
  background: #18181b;
  color: #e4e4e7;
  padding: 1rem;
  border-radius: 6px;
  overflow-x: auto;
  line-height: 1.5;
}
.content pre code { background: none; padding: 0; color: inherit; font-size: .85em; }

.content table { border-collapse: collapse; margin: 1rem 0; width: 100%; }
.content th, .content td {
  border: 1px solid #e4e4e7; padding: .4rem .6rem;
  text-align: left; font-size: .9rem;
}
.content th { background: #f4f4f5; font-weight: 600; }

.content blockquote {
  border-left: 3px solid #d4d4d8;
  margin: 1rem 0; padding: .25rem 1rem;
  color: #52525b; background: #fafafa;
}

.toc-wrap {
  padding: 1.5rem 1.5rem 4rem 1rem;
  position: sticky; top: 56px;
  align-self: start;
  max-height: calc(100vh - 56px);
  overflow-y: auto;
}
.toc { font-size: .85rem; }
.toc-title { font-size: .75rem; text-transform: uppercase; letter-spacing: .04em;
  color: #71717a; font-weight: 600; margin-bottom: .5rem; }
.toc ul { list-style: none; padding: 0; margin: 0; }
.toc li { margin: .2rem 0; }
.toc li.toc-h3 { padding-left: 1rem; }
.toc a { color: #52525b; }
.toc a:hover { color: #0066cc; text-decoration: none; }

@media (max-width: 1100px) {
  .layout { grid-template-columns: 240px 1fr; }
  .toc-wrap { display: none; }
}
@media (max-width: 720px) {
  .layout { grid-template-columns: 1fr; }
  .sidebar { display: none; }
  .content { padding: 1rem; }
}
"#;

/// Client-side search and keyboard nav. Loaded lazily, runs on every page.
const APP_JS: &str = r#"
(function(){
  const $ = (s, p=document) => p.querySelector(s);
  const search = $('#search');
  const results = $('#search-results');
  if (!search || !results) return;
  let index = null, selected = -1, hits = [];

  async function loadIndex() {
    if (index) return index;
    const r = await fetch('/search-index.json');
    index = await r.json();
    return index;
  }

  function snippet(body, query) {
    const lower = body.toLowerCase(), q = query.toLowerCase();
    const i = lower.indexOf(q);
    if (i < 0) return body.slice(0, 120) + '…';
    const start = Math.max(0, i - 40);
    const end = Math.min(body.length, i + q.length + 80);
    let s = body.slice(start, end);
    if (start > 0) s = '…' + s;
    if (end < body.length) s = s + '…';
    return s.replace(new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi'),
      m => '<mark>' + m + '</mark>');
  }

  async function runSearch() {
    const q = search.value.trim();
    if (!q) { results.classList.remove('open'); results.innerHTML = ''; return; }
    const idx = await loadIndex();
    const lower = q.toLowerCase();
    hits = idx
      .map(p => {
        const titleHit = p.title.toLowerCase().includes(lower);
        const bodyHit  = p.body.toLowerCase().includes(lower);
        if (!titleHit && !bodyHit) return null;
        return { ...p, score: (titleHit ? 10 : 0) + (bodyHit ? 1 : 0) };
      })
      .filter(Boolean)
      .sort((a,b) => b.score - a.score)
      .slice(0, 8);
    if (!hits.length) {
      results.innerHTML = '<div class="search-hit"><span class="h-snippet">No matches.</span></div>';
    } else {
      results.innerHTML = hits.map((h, i) =>
        `<div class="search-hit${i===0?' selected':''}" data-slug="${h.slug}">
           <div class="h-title">${h.title}</div>
           <div class="h-section">${h.section}</div>
           <div class="h-snippet">${snippet(h.body, q)}</div>
         </div>`
      ).join('');
      selected = 0;
    }
    results.classList.add('open');
    [...results.querySelectorAll('.search-hit')].forEach(el => {
      el.addEventListener('mousedown', e => {
        e.preventDefault();
        location.href = '/docs/' + el.dataset.slug;
      });
    });
  }

  let t;
  search.addEventListener('input', () => { clearTimeout(t); t = setTimeout(runSearch, 80); });
  search.addEventListener('focus', () => { if (search.value.trim()) results.classList.add('open'); });
  search.addEventListener('blur', () => setTimeout(() => results.classList.remove('open'), 120));
  search.addEventListener('keydown', e => {
    const items = [...results.querySelectorAll('.search-hit')];
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      if (!items.length) return;
      items[selected]?.classList.remove('selected');
      selected = (selected + 1) % items.length;
      items[selected].classList.add('selected');
      items[selected].scrollIntoView({block:'nearest'});
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      if (!items.length) return;
      items[selected]?.classList.remove('selected');
      selected = (selected - 1 + items.length) % items.length;
      items[selected].classList.add('selected');
      items[selected].scrollIntoView({block:'nearest'});
    } else if (e.key === 'Enter') {
      const it = items[selected];
      if (it) location.href = '/docs/' + it.dataset.slug;
    } else if (e.key === 'Escape') {
      results.classList.remove('open');
      search.blur();
    }
  });

  // `/` focuses search.
  document.addEventListener('keydown', e => {
    if (e.key === '/' && document.activeElement !== search) {
      e.preventDefault();
      search.focus();
      search.select();
    }
  });
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_fixture() -> Meta {
        Meta {
            sections: vec![Section {
                title: "Test".into(),
                pages: vec![Page { slug: "intro".into(), title: "Intro".into() }],
            }],
        }
    }

    #[test]
    fn slugify_works() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("Section 2.3"), "section-2-3");
        assert_eq!(slugify("--leading"), "leading");
        assert_eq!(slugify("trailing--"), "trailing");
    }

    #[test]
    fn route_redirects_root() {
        let (status, ct, _) = route(&meta_fixture(), "/");
        assert_eq!(status, 200);
        assert!(ct.starts_with("text/html"));
    }

    #[test]
    fn route_serves_styles() {
        let (status, ct, body) = route(&meta_fixture(), "/styles.css");
        assert_eq!(status, 200);
        assert!(ct.contains("css"));
        assert!(!body.is_empty());
    }

    #[test]
    fn escape_html_preserves_safe() {
        assert!(matches!(escape_html("hello world"), Cow::Borrowed(_)));
        let escaped = escape_html("<script>");
        assert_eq!(&*escaped, "&lt;script&gt;");
    }

    #[test]
    fn meta_loads_from_embed() {
        // The actual docs/meta.toml is embedded. If this passes, the
        // rust-embed setup is wired correctly and the toml is valid.
        let meta = load_meta().expect("docs/meta.toml must embed and parse");
        assert!(!meta.sections.is_empty());
    }

    #[test]
    fn page_markdown_returns_known_page() {
        // `intro` is the first page every catalog has; it must embed.
        let md = page_markdown("intro").expect("intro.md must embed");
        assert!(!md.trim().is_empty());
    }

    #[test]
    fn page_markdown_unknown_slug_lists_alternatives() {
        let err = page_markdown("does-not-exist").unwrap_err().to_string();
        assert!(err.contains("does-not-exist"));
        // The error names valid pages so a typo self-corrects.
        assert!(err.contains("intro"), "error should list available pages: {err}");
    }

    #[test]
    fn index_text_lists_pages_with_hint() {
        let idx = index_text().expect("index renders");
        assert!(idx.contains("trove docs <slug>"));
        assert!(idx.contains("quickstart"));
    }

    #[test]
    fn all_markdown_concatenates_every_page() {
        let all = all_markdown().expect("concatenation renders");
        // Boundary markers and at least two distinct pages present.
        assert!(all.contains("<!-- docs/intro.md"));
        assert!(all.contains("<!-- docs/quickstart.md"));
    }
}
