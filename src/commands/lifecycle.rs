//! Vault lifecycle: the two command pairs from the proposal.
//!
//! - **`mount` / `unmount`** — runtime up/down. Transient; they never touch the
//!   config or the boot agent. `mount --volume <name>` resolves a vault entirely
//!   from its saved config (no cwd, no ambient env), which is exactly what the
//!   boot agent runs. `unmount` is "down for now"; it comes back at next login.
//! - **`attach` (= `init`) / `detach`** — machine membership. `detach` removes
//!   this machine's footprint (unmount + delete config + remove agent) but
//!   **leaves the backend untouched** — other machines are unaffected and you
//!   can `attach` here again later.
//!
//! Plus **`trove ls`**, the fleet view: every configured volume with its mount
//! and boot-agent status.

use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use crate::config::{Config, ResolvedVolume};
use crate::platform::{self, AgentStatus};

/// Fully set up and mount a named vault, blocking until unmounted. This is the
/// `trove mount --volume <name>` path and what each boot agent executes: it
/// resolves the volume's config + credentials, exports the R2 keys libjfs needs,
/// opens the volume, loads the in-vault schema registry, wires versioning +
/// embedding, and hands off to the blocking FUSE loop.
pub fn mount_volume(name: &str, no_embed: bool) -> Result<()> {
    let rv = ResolvedVolume::load(name)?;
    // libjfs reads the R2 secret from the environment at mount; the agent runs
    // bare, so push the saved keys in before we open the volume.
    rv.export_r2_env();

    let meta = rv.meta_url()?;
    let mountpoint = rv.volume.mountpoint.clone();
    // FUSE needs an existing dir to mount onto; a fresh attach point may be gone
    // after a reboot wiped a tmpfs parent, so recreate it defensively.
    std::fs::create_dir_all(&mountpoint)
        .with_context(|| format!("creating mountpoint {mountpoint}"))?;

    let fs = crate::jfs::Fs::init(&rv.name, &meta, &rv.volume.cache)?;
    // Schemas travel *in* the vault (`<vault>/.types/`), read through the volume
    // (libjfs) so attaching picks up the vault's own validation gate.
    let registry = crate::types::Registry::load_from_fs(&fs)?;

    let versions_db = rv.versions_db()?.to_string();
    let versions = Some(crate::version::VersionStore::connect(
        &versions_db,
        Some(&rv.volume.schema),
    )?);

    let embed_tx = if no_embed {
        None
    } else {
        match std::env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty()) {
            Some(key) => Some(crate::embed::spawn_embedder(
                &versions_db,
                key,
                Some(&rv.volume.schema),
            )?),
            None => {
                eprintln!(
                    "{} OPENAI_API_KEY not set — embedding disabled for this mount",
                    "warning:".yellow().bold()
                );
                None
            }
        }
    };

    println!(
        "{} mounting `{}` at {} (versioning on; embed {})",
        "trove:".bold(),
        rv.name,
        mountpoint,
        if embed_tx.is_some() { "on" } else { "off" },
    );
    crate::mount::mount_blocking(fs, registry, versions, embed_tx, Path::new(&mountpoint))?;
    Ok(())
}

/// `trove unmount` — tear the live mount down without touching config or agent.
/// Resolves the mountpoint from the named volume's config. Best-effort across
/// the FUSE unmount tools; a vault that's already down is reported, not an error.
pub fn unmount(name: &str) -> Result<()> {
    let rv = ResolvedVolume::load(name)?;
    let mountpoint = rv.volume.mountpoint.clone();
    if !is_mounted(&mountpoint) {
        println!(
            "{} `{}` is not mounted ({})",
            "trove:".bold(),
            rv.name,
            mountpoint
        );
        return Ok(());
    }
    unmount_path(&mountpoint)?;
    println!(
        "{} unmounted `{}` ({})",
        "trove:".bold(),
        rv.name,
        mountpoint
    );
    Ok(())
}

/// `trove detach` — remove this machine's footprint for a vault: unmount,
/// remove the boot agent, delete the local config. The backend (schema +
/// bucket) is **left intact** — other machines are unaffected and you can
/// `attach` here again later. Destroying a vault is a separate, deliberate
/// manual step (drop the schema, delete the bucket).
pub fn detach(name: &str) -> Result<()> {
    // Resolve first so an unknown volume errors before we change anything.
    let rv = ResolvedVolume::load(name)?;
    let mountpoint = rv.volume.mountpoint.clone();

    // 1. Unmount (best-effort — may already be down).
    if is_mounted(&mountpoint) {
        match unmount_path(&mountpoint) {
            Ok(()) => println!("{} unmounted {}", "trove detach:".bold(), mountpoint),
            Err(e) => eprintln!(
                "{} couldn't unmount {} ({e}); removing config anyway",
                "warning:".yellow().bold(),
                mountpoint
            ),
        }
    }

    // 2. Remove the boot agent (its existence is the reference count).
    if let Err(e) = platform::remove_agent(&rv.name) {
        eprintln!(
            "{} couldn't remove boot agent for `{}` ({e})",
            "warning:".yellow().bold(),
            rv.name
        );
    } else {
        println!("{} removed boot agent", "trove detach:".bold());
    }

    // 3. Delete the local config — the membership record.
    let path = Config::remove_volume(&rv.name)?;
    println!("{} removed {}", "trove detach:".bold(), path.display());

    println!(
        "{} `{}` detached from this machine. The vault lives on in its backend \
         (schema `{}` + bucket); `trove init` in a folder re-attaches it here.",
        "✓".green(),
        rv.name,
        rv.volume.schema
    );
    Ok(())
}

/// `trove ls` — the fleet view: every configured volume with its mount + boot
/// agent status, so a machine holding many vaults stays legible at a glance.
pub fn ls() -> Result<usize> {
    let volumes = Config::list_volumes();
    if volumes.is_empty() {
        println!(
            "{} no vaults configured on this machine. Run `trove init` in a folder to attach one.",
            "trove:".bold()
        );
        return Ok(0);
    }
    println!(
        "{} {} vault(s) on this machine\n",
        "trove:".bold(),
        volumes.len()
    );
    println!(
        "  {:<20} {:<10} {:<11} {}",
        "VOLUME".bold(),
        "MOUNT".bold(),
        "AGENT".bold(),
        "MOUNTPOINT".bold()
    );
    for (name, vol) in &volumes {
        let mounted = is_mounted(&vol.mountpoint);
        // Pad the *plain* text to the column width first, then colorize the
        // padded cell — so alignment is correct whether or not ANSI codes are
        // emitted (they're suppressed when piped, kept on a TTY).
        let mount_cell = {
            let cell = format!("{:<10}", if mounted { "mounted" } else { "down" });
            if mounted {
                cell.green()
            } else {
                cell.dimmed()
            }
        };
        let agent = platform::agent_status(name);
        let agent_cell = {
            let cell = format!("{:<11}", agent.label());
            match agent {
                AgentStatus::Running => cell.green(),
                AgentStatus::Installed => cell.yellow(),
                AgentStatus::Absent | AgentStatus::Unsupported => cell.dimmed(),
            }
        };
        println!(
            "  {:<20} {} {} {}",
            name,
            mount_cell,
            agent_cell,
            vol.mountpoint.dimmed()
        );
    }
    Ok(0)
}

/// Is `path` a live mount point? A FUSE mount sits on its own device, so a
/// mountpoint's `st_dev` differs from its parent's. Cheap, no `/proc` parsing,
/// works the same on Linux and macOS. A missing path or unreadable parent reads
/// as "not mounted".
pub fn is_mounted(path: &str) -> bool {
    let p = Path::new(path);
    let Ok(here) = std::fs::metadata(p) else {
        return false;
    };
    let parent = p.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(p);
    let Ok(up) = std::fs::metadata(parent) else {
        return false;
    };
    here.dev() != up.dev()
}

/// Unmount a FUSE filesystem at `mountpoint`, trying the tools in the order most
/// likely to exist: `fusermount3 -u`, `fusermount -u` (Linux), then `umount`
/// (macOS / fallback). Succeeds if any does.
fn unmount_path(mountpoint: &str) -> Result<()> {
    let attempts: [(&str, &[&str]); 3] = [
        ("fusermount3", &["-u", mountpoint]),
        ("fusermount", &["-u", mountpoint]),
        ("umount", &[mountpoint]),
    ];
    let mut last_err = String::new();
    for (cmd, args) in attempts {
        match Command::new(cmd).args(args).output() {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => {
                last_err = format!(
                    "{cmd}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                // If it's already unmounted, treat as success.
                if last_err.contains("not mounted") || last_err.contains("not found") {
                    return Ok(());
                }
            }
            Err(_) => continue, // tool not installed — try the next
        }
    }
    bail!("could not unmount {mountpoint}: {last_err}")
}
