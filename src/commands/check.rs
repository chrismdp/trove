//! `trove check <store>` — walk a store, select the governing schema(s) for
//! each markdown file by path glob (+ type-field disambiguation), validate, and
//! report violations. Exit code is non-zero on any failure, so it drops into a
//! pre-commit hook or CI.

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
            "{}: no schemas found in {}/.types — nothing to validate.",
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

        // Read as bytes first: a non-UTF-8 file is itself a finding, never a
        // reason to abort the whole sweep.
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
                // Can't read the type of an unparseable file, so decide on the
                // path glob alone: a finding only if some schema governs this
                // path. Templates / vendored dirs nothing globs are skipped.
                if !registry.path_is_governed(rel) {
                    s.untyped += 1;
                } else {
                    s.failed += 1;
                    report_fail(rel, &[format!("{e}")]);
                }
                continue;
            }
        };

        let file_type = doc.frontmatter.get("type").and_then(|v| v.as_str());
        let schemas = registry.select(rel, file_type);
        if schemas.is_empty() {
            s.untyped += 1;
            continue;
        }

        let mut violations: Vec<String> = Vec::new();
        for schema in schemas {
            if let Err(errs) = validate::validate_against(&doc.frontmatter, schema) {
                for v in errs {
                    violations.push(format!("[{}] {}: {}", schema.name, v.instance_path, v.message));
                }
            }
        }

        if violations.is_empty() {
            s.valid += 1;
            if !quiet {
                println!("{} {}", "ok  ".green(), rel.display());
            }
        } else {
            s.failed += 1;
            report_fail(rel, &violations);
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

/// Skip dotfiles/dotdirs (`.git`, `.obsidian`, `.types`) — the registry is
/// loaded explicitly and we don't validate plumbing.
fn is_hidden_dir(path: &Path, store: &Path) -> bool {
    if path == store {
        return false;
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}
