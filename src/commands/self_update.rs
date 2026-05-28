//! `trove self-update` — re-run the canonical installer to upgrade in place.
//!
//! The install script (`install.sh` at the repo root) already does platform
//! detection, sha256 verification, extraction, and symlinking. Reimplementing
//! any of that in Rust would just be a second source of truth that drifts.
//! So this command does the minimum on its own:
//!
//!   1. Hit the GitHub API to resolve the latest release tag.
//!   2. Compare against the compiled-in `CARGO_PKG_VERSION`. If they match
//!      and `--force` wasn't passed, exit 0.
//!   3. Otherwise shell out to `sh -c "curl -fsSL <install.sh> | sh"` (with
//!      `-s -- --version <tag>` if `--version` was passed).
//!
//! Both the version check and the install both go through `curl`, which the
//! install script already requires — no new runtime deps.

use anyhow::{Context, Result, anyhow};
use colored::Colorize;
use std::process::Command;

const REPO: &str = "chrismdp/trove";
const INSTALL_URL: &str = "https://raw.githubusercontent.com/chrismdp/trove/main/install.sh";

/// Resolve the latest published release tag (e.g. `"v0.1.2"`) via GitHub's
/// `/releases/latest` endpoint. Anonymous, rate-limited to 60/hour per IP,
/// which is fine for a manual command.
fn fetch_latest_tag() -> Result<String> {
    let api_url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let out = Command::new("curl")
        .args(["-fsSL", "-H", "User-Agent: trove-self-update", &api_url])
        .output()
        .context("running curl to fetch latest release tag (is curl on PATH?)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "GitHub API request failed (status {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .context("parsing GitHub API JSON response")?;
    let tag = parsed
        .get("tag_name")
        .and_then(|v| v.as_str())
        .context("GitHub API response had no tag_name field")?
        .to_string();
    Ok(tag)
}

pub fn run(pin: Option<&str>, force: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("{} current version: {current}", "trove:".bold());

    let target_tag = match pin {
        Some(v) => {
            // Accept either `v0.1.2` or `0.1.2`.
            let v = v.trim();
            if v.starts_with('v') { v.to_string() } else { format!("v{v}") }
        }
        None => {
            println!("{} resolving latest release from github.com/{REPO} …", "trove:".bold());
            fetch_latest_tag()?
        }
    };
    let target_num = target_tag.trim_start_matches('v');
    println!("{} target version:  {target_num} ({target_tag})", "trove:".bold());

    if target_num == current && !force {
        println!("{} already on the latest version — nothing to do.", "trove:".green().bold());
        println!("   (pass --force to re-install the same version anyway)");
        return Ok(());
    }

    let installer = if pin.is_some() {
        format!("curl -fsSL {INSTALL_URL} | sh -s -- --version {target_tag}")
    } else {
        format!("curl -fsSL {INSTALL_URL} | sh")
    };

    println!("{} running: {installer}", "trove:".bold());
    let status = Command::new("sh")
        .arg("-c")
        .arg(&installer)
        .status()
        .context("spawning sh to run the installer")?;

    if !status.success() {
        return Err(anyhow!(
            "installer exited with status {status} — your existing trove install is unchanged"
        ));
    }
    println!("{} upgraded to {target_num}.", "trove:".green().bold());
    Ok(())
}
