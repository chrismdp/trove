//! Cross-platform boot-agent layer.
//!
//! **One boot agent per vault.** The set of installed agents *is* the machine's
//! vault membership: an agent is installed at `attach`, removed at `detach`, and
//! never touched by `mount`/`unmount`. Each agent supervises a single blocking
//! `trove mount --volume <name>` process — which maps 1:1 onto how `launchd`
//! (macOS) and `systemd --user` (Linux) supervise one long-running process.
//! Detach the last vault and nothing lingers; there is no shared singleton
//! service to orphan.
//!
//! Everything an agent needs resolves from the vault's saved config (name →
//! mountpoint, credentials), so the agent runs in a bare environment with no
//! working directory.
//!
//! **Restart policy is deliberately conservative** (proposal decision 2):
//! RunAtLoad / start-at-login is on, but auto-restart-on-crash is **off**. A
//! crash-looping mount remounting every few seconds is exactly how a bad mount
//! re-wedges a machine; resilience is opt-in once the mount path is proven.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

/// Where a vault's boot agent stands on this machine. `Installed` = will mount
/// at the next login; `Running` = installed *and* currently loaded/active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// Installed and currently loaded/active.
    Running,
    /// Installed (will mount at next login) but not currently loaded.
    Installed,
    /// No agent configured for this vault.
    Absent,
    /// This platform has no boot-agent backend.
    Unsupported,
}

impl AgentStatus {
    pub fn label(self) -> &'static str {
        match self {
            AgentStatus::Running => "running",
            AgentStatus::Installed => "installed",
            AgentStatus::Absent => "none",
            AgentStatus::Unsupported => "n/a",
        }
    }
}

/// The path to bake into the boot agent's command. Prefer the stable
/// `~/.local/bin/trove` symlink the installer creates, so a `trove self-update`
/// (which swaps the versioned binary under `~/.local/share/trove/<v>/`) doesn't
/// leave the agent pointing at a path that later disappears. Fall back to the
/// running binary's real path when the symlink isn't there (source builds).
fn trove_launcher_path() -> Result<PathBuf> {
    if let Some(home) = home_dir() {
        let symlink = home.join(".local/bin/trove");
        if symlink.exists() {
            return Ok(symlink);
        }
    }
    std::env::current_exe().context("locating the trove binary for the boot agent")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Install + start the boot agent for `name`, mounting at `mountpoint`. Idempotent
/// — re-installing an existing agent rewrites the unit/plist and reloads it.
pub fn install_agent(name: &str, mountpoint: &str) -> Result<()> {
    imp::install(name, mountpoint)
}

/// Remove the boot agent for `name` (unload + delete the unit/plist). A missing
/// agent is not an error — `detach` is best-effort cleanup.
pub fn remove_agent(name: &str) -> Result<()> {
    imp::remove(name)
}

/// Where this vault's agent stands right now.
pub fn agent_status(name: &str) -> AgentStatus {
    imp::status(name)
}

/// Human-readable description of where the agent's logs land, for `attach`
/// output and `trove ls`.
pub fn log_hint(name: &str) -> String {
    imp::log_hint(name)
}

// =========================================================================
// macOS — LaunchAgent
// =========================================================================
#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::process::Command;

    fn label(name: &str) -> String {
        format!("com.trove.{name}")
    }

    fn plist_path(name: &str) -> Result<PathBuf> {
        let home = home_dir().context("HOME is not set")?;
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", label(name))))
    }

    fn log_path(name: &str) -> Result<PathBuf> {
        let home = home_dir().context("HOME is not set")?;
        Ok(home.join("Library/Logs/trove").join(format!("{name}.log")))
    }

    fn uid_target(name: &str) -> String {
        let uid = unsafe { libc::getuid() };
        format!("gui/{uid}/{}", label(name))
    }

    pub fn install(name: &str, _mountpoint: &str) -> Result<()> {
        let trove = trove_launcher_path()?;
        let plist = plist_path(name)?;
        let log = log_path(name)?;
        if let Some(dir) = plist.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let body = plist_xml(&label(name), &trove.to_string_lossy(), name, &log.to_string_lossy());
        std::fs::write(&plist, body).with_context(|| format!("writing {}", plist.display()))?;

        // Reload cleanly: bootout any stale instance (ignore "not loaded"), then
        // bootstrap the fresh plist. RunAtLoad starts the mount immediately.
        let uid = unsafe { libc::getuid() };
        let _ = Command::new("launchctl")
            .args(["bootout", &uid_target(name)])
            .output();
        let out = Command::new("launchctl")
            .args(["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])
            .output()
            .context("running launchctl bootstrap")?;
        if !out.status.success() {
            // Older macOS: fall back to the legacy load verb.
            let legacy = Command::new("launchctl")
                .args(["load", "-w", &plist.to_string_lossy()])
                .output()
                .context("running launchctl load")?;
            if !legacy.status.success() {
                bail!(
                    "launchctl could not load {}: {}",
                    plist.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
        Ok(())
    }

    pub fn remove(name: &str) -> Result<()> {
        let _ = Command::new("launchctl")
            .args(["bootout", &uid_target(name)])
            .output();
        if let Ok(plist) = plist_path(name) {
            if plist.exists() {
                let _ = std::fs::remove_file(&plist);
            }
        }
        Ok(())
    }

    pub fn status(name: &str) -> AgentStatus {
        let installed = plist_path(name).map(|p| p.exists()).unwrap_or(false);
        let uid = unsafe { libc::getuid() };
        let loaded = Command::new("launchctl")
            .args(["print", &uid_target(name)])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        let _ = uid;
        match (installed, loaded) {
            (_, true) => AgentStatus::Running,
            (true, false) => AgentStatus::Installed,
            (false, false) => AgentStatus::Absent,
        }
    }

    pub fn log_hint(name: &str) -> String {
        log_path(name)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| format!("~/Library/Logs/trove/{name}.log"))
    }

    fn plist_xml(label: &str, trove: &str, volume: &str, log: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{trove}</string>
        <string>mount</string>
        <string>--volume</string>
        <string>{volume}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn plist_runs_mount_with_volume_and_logs() {
            let xml = plist_xml(
                "com.trove.notes",
                "/Users/u/.local/bin/trove",
                "notes",
                "/Users/u/Library/Logs/trove/notes.log",
            );
            assert!(xml.contains("<string>/Users/u/.local/bin/trove</string>"));
            assert!(xml.contains("<string>mount</string>"));
            assert!(xml.contains("<string>--volume</string>"));
            assert!(xml.contains("<string>notes</string>"));
            assert!(xml.contains("<key>RunAtLoad</key>"));
            assert!(xml.contains("/Users/u/Library/Logs/trove/notes.log"));
        }

        #[test]
        fn label_format() {
            assert_eq!(label("my-notes"), "com.trove.my-notes");
        }
    }
}

// =========================================================================
// Linux — systemd --user, one template unit instanced per vault
// =========================================================================
#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::process::Command;

    fn unit_dir() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join(".config")))
            .context("neither XDG_CONFIG_HOME nor HOME is set")?;
        Ok(base.join("systemd/user"))
    }

    fn template_path() -> Result<PathBuf> {
        Ok(unit_dir()?.join("trove@.service"))
    }

    fn instance(name: &str) -> String {
        format!("trove@{name}.service")
    }

    /// The template unit's text. `%i` is the systemd instance specifier (the
    /// vault name), so one file covers every vault — instancing is the
    /// multi-volume story on Linux. Pure, for testability.
    fn systemd_unit(trove: &str) -> String {
        format!(
            "[Unit]\n\
             Description=trove vault %i\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={trove} mount --volume %i\n\
             # Conservative restart policy (proposal decision 2): a crash-looping\n\
             # mount is how a bad mount re-wedges the machine. Resilience is opt-in.\n\
             Restart=no\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        )
    }

    /// Write the template unit (rewritten on every install so a moved `trove`
    /// binary is picked up).
    fn write_template() -> Result<()> {
        let trove = trove_launcher_path()?;
        let dir = unit_dir()?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let body = systemd_unit(&trove.to_string_lossy());
        let path = template_path()?;
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn systemctl(args: &[&str]) -> Result<std::process::Output> {
        Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .context("running systemctl --user")
    }

    pub fn install(name: &str, _mountpoint: &str) -> Result<()> {
        write_template()?;
        // Reload so a rewritten template is seen, then enable + start now.
        let _ = systemctl(&["daemon-reload"]);
        let out = systemctl(&["enable", "--now", &instance(name)])?;
        if !out.status.success() {
            bail!(
                "systemctl --user enable --now {} failed: {}\n\
                 (is a user session/D-Bus available? for boot-time mounts before login, run \
                 `loginctl enable-linger $USER`)",
                instance(name),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    pub fn remove(name: &str) -> Result<()> {
        // disable --now unloads + stops; the shared template unit stays in place.
        let _ = systemctl(&["disable", "--now", &instance(name)]);
        Ok(())
    }

    pub fn status(name: &str) -> AgentStatus {
        let active = systemctl(&["is-active", &instance(name)])
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
            .unwrap_or(false);
        if active {
            return AgentStatus::Running;
        }
        let enabled = systemctl(&["is-enabled", &instance(name)])
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                let s = s.trim();
                s == "enabled" || s == "enabled-runtime" || s == "linked"
            })
            .unwrap_or(false);
        if enabled {
            AgentStatus::Installed
        } else {
            AgentStatus::Absent
        }
    }

    pub fn log_hint(name: &str) -> String {
        format!("journalctl --user -u trove@{name}")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn unit_uses_instance_specifier_and_no_restart() {
            let u = systemd_unit("/home/u/.local/bin/trove");
            // %i (the vault name) is filled by systemd per instance.
            assert!(u.contains("ExecStart=/home/u/.local/bin/trove mount --volume %i"));
            assert!(u.contains("Description=trove vault %i"));
            // Conservative restart policy (decision 2).
            assert!(u.contains("Restart=no"));
            assert!(u.contains("WantedBy=default.target"));
        }

        #[test]
        fn instance_and_template_names() {
            assert_eq!(instance("my-notes"), "trove@my-notes.service");
            assert!(log_hint("notes").contains("journalctl --user -u trove@notes"));
        }
    }
}

// =========================================================================
// Other platforms — no boot-agent backend
// =========================================================================
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::*;

    pub fn install(_name: &str, _mountpoint: &str) -> Result<()> {
        bail!("auto-mount boot agents are only supported on macOS and Linux; mount manually with `trove mount --volume <name>`")
    }
    pub fn remove(_name: &str) -> Result<()> {
        Ok(())
    }
    pub fn status(_name: &str) -> AgentStatus {
        AgentStatus::Unsupported
    }
    pub fn log_hint(_name: &str) -> String {
        "n/a".to_string()
    }
}
