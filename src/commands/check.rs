//! `trove check <store>` — walk a store, validate every typed markdown file
//! against the type registry, and report violations. Exit code is non-zero
//! when anything failed, so it slots straight into a pre-commit hook or CI.

use crate::{frontmatter, types::Registry, validate};
use anyhow::{Context, Result};
use colored::Colorize;
use std::path::Path;
use walkdir::WalkDir;

pub struct Summary {
    pub checked: usize,
    pub valid: usize,
    pub untyped: usize,
    pub failed: usize,
}

pub fn run(store: &Path, quiet: bool) -> Result<Summary> {
    let registry = Registry::load(store).context("loading type registry")?;
    if registry.is_empty() {
        eprintln!(
            "{}: no schemas found in {}/.types — nothing to validate.\n  \
             Add a schema, e.g. .types/person.json, to start enforcing shape.",
            "warning".yellow().bold(),
            store.display()
        );
    }

    let mut s = Summary { checked: 0, valid: 0, untyped: 0, failed: 0 };

    for entry in WalkDir::new(store)
        .into_iter()
        .filter_entry(|e| !is_hidden_dir(e.path(), store))
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        s.checked += 1;
        let rel = path.strip_prefix(store).unwrap_or(path);

        // Read as bytes first: a non-UTF-8 file is itself a finding (a note
        // should be text), never a reason to abort the whole sweep.
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let raw = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => {
                s.failed += 1;
                report_fail(rel, &["file is not valid UTF-8".to_string()]);
                continue;
            }
        };
        let doc = match frontmatter::parse(&raw) {
            Ok(d) => d,
            Err(e) => {
                s.failed += 1;
                report_fail(rel, &[format!("{e}")]);
                continue;
            }
        };

        match validate::validate(&doc, &registry) {
            validate::Verdict::Valid => {
                s.valid += 1;
                if !quiet {
                    println!("{} {}", "ok  ".green(), rel.display());
                }
            }
            validate::Verdict::Untyped => {
                s.untyped += 1;
            }
            validate::Verdict::Unparseable(msg) => {
                s.failed += 1;
                report_fail(rel, &[msg]);
            }
            validate::Verdict::Invalid(violations) => {
                s.failed += 1;
                let msgs: Vec<String> = violations
                    .iter()
                    .map(|v| format!("{}: {}", v.instance_path, v.message))
                    .collect();
                report_fail(rel, &msgs);
            }
        }
    }

    Ok(s)
}

fn report_fail(rel: &Path, msgs: &[String]) {
    println!("{} {}", "FAIL".red().bold(), rel.display());
    for m in msgs {
        println!("      {} {}", "↳".red(), m);
    }
}

/// Skip dotfiles/dotdirs (`.git`, `.obsidian`, `.types`) — the registry itself
/// is loaded explicitly, and we don't validate plumbing.
fn is_hidden_dir(path: &Path, store: &Path) -> bool {
    if path == store {
        return false;
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}
