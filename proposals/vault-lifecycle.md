# Proposal: vault lifecycle commands + cross-platform auto-mount

Status: **design** · builds on per-folder-vaults (`init`/attach) · targets
**macOS + Linux** · multi-volume is first-class, not an edge.

## Problem

- `trove init` mounts in the **foreground** and blocks — you don't get your shell
  back, and the mount dies when that process does.
- A FUSE mount **doesn't survive a reboot** (true of all FUSE), so today you'd
  manually re-mount every single login. That's the headline pain.
- There's no clean **"remove this vault from this machine"** — `init` wants an
  empty dir and `Ctrl-C` is the only stop.
- A machine routinely holds **many vaults** (the fleet case: one DB + one R2
  credential → many volumes). Each must mount, unmount, and persist
  independently, with no cross-talk.
- Those vaults don't always share creds: a personal vault on one Cloudflare /
  Supabase account and a work vault on another may sit on the **same machine**,
  each needing **its own DB + R2 credentials**.

## Command model — two pairs

| command | live mount | local config + boot agent | backend (schema + bucket) |
|---|---|---|---|
| `init` / `attach` | mounts | **writes + installs** | creates-or-attaches |
| `mount` | mounts | unchanged | — |
| `unmount` | unmounts | unchanged | — |
| `detach` | unmounts | **removes** | **untouched** |
| *(destroy a vault)* | — | — | **manual** — `drop schema` + delete the bucket |

Two distinct pairs:

- **`attach` (= `init`) / `detach` — machine membership.** `attach` sets this
  machine up: create-or-attach the backend, write the per-volume config, install
  the boot agent, mount. `detach` removes this machine's footprint: unmount,
  delete the config, remove the agent. **The vault lives on in the backend** —
  other machines are unaffected, and you can `attach` here again later.
- **`mount` / `unmount` — runtime up/down.** Transient; they never touch the
  config or the agent. The boot agent runs `mount`; `unmount` is "down for now"
  and it comes back at the next login.

**No `kill`.** Destroying a vault (drop the schema, delete the bucket) is a
deliberate manual action in psql + the R2 dashboard. This is the right symmetry:
trove never *creates* the backend either (you make the bucket in the dashboard;
trove only validates it — see per-folder-vaults), so it must not *destroy* it.
Keeps a "nuke the shared vault" footgun out of the CLI entirely.

**Naming.** `attach`/`detach` is the machine-membership pair; `init` is kept as a
familiar alias of `attach` (the `git init`/`npm init` reflex, and what the docs
already say). No user-facing **"clone"** language — that command is gone; the
internal COW `jfs_clone` (version snapshots) keeps the word because that's the
correct meaning.

`mount`/`unmount` resolve a vault by **`--volume <name>`** (from the saved
config), not the cwd — the boot agent runs with no working directory, so it
passes the name explicitly.

## Cross-platform auto-mount (per-vault boot agent)

**One boot agent per vault.** The set of agents *is* the reference count:
installed at `attach`, removed at `detach`, never touched by `mount`/`unmount`.
Detach the last vault and nothing lingers — there's no shared singleton service
that could be orphaned. (Whether to ever consolidate into one supervisor is an
implementation detail; the per-vault model is the simplest correct start and
maps 1:1 to how `launchd`/`systemd` supervise a single blocking process.)

A small platform layer abstracts it — `install_agent(vault)`,
`remove_agent(vault)`, `agent_status(vault)` — `cfg`-gated per OS. Everything
resolves from the vault's saved config (mountpoint, name).

### macOS — LaunchAgent

- `~/Library/LaunchAgents/com.trove.<vault>.plist`
- `ProgramArguments`: `<trove-path> mount --volume <vault>`
- `RunAtLoad: true` (mount at login)
- `StandardOutPath` / `StandardErrorPath`: `~/Library/Logs/trove/<vault>.log`
- load / unload: `launchctl bootstrap gui/$UID <plist>` / `launchctl bootout
  gui/$UID/com.trove.<vault>` (fall back to `load -w` / `unload -w` on older OSes)
- The mount needs **macFUSE installed + approved**; if it isn't, the agent's
  mount fails and the macFUSE guidance lands in the per-vault log.

### Linux — systemd **user** service

- Templated unit `~/.config/systemd/user/trove@.service`, instantiated as
  `trove@<vault>.service` (`%i` = vault name): `ExecStart=<trove> mount --volume %i`
- enable + start: `systemctl --user enable --now trove@<vault>.service`
- logs → the journal: `journalctl --user -u trove@<vault>` (no log file needed)
- `WantedBy=default.target`. Note: mounting *before* an interactive login (true
  boot-time) needs `loginctl enable-linger <user>`; document it, don't force it.

A single template unit covers any number of vaults — instancing is exactly the
multi-volume story on Linux.

## Multiple volumes on one machine (first-class)

This is the fleet case from per-folder-vaults: **one shared `credentials.toml`
(one DB + one R2 cred) + N per-volume configs**. The lifecycle must make N
vaults feel like one:

- Each vault gets its **own** agent, keyed on its canonical (normalized) name:
  `com.trove.<vault>` / `trove@<vault>.service`. Names are unique per vault, so
  no collisions and no shared state to coordinate.
- `attach` a 2nd/3rd vault → a 2nd/3rd agent → all of them mount at login.
  `detach` one → only its agent goes; the others are untouched. `mount`/`unmount`
  on one never affect another.
- **`trove ls`** (new) — list every configured volume with its mount + agent
  status (mounted / configured-not-mounted / agent-installed), so the fleet is
  legible at a glance.

## Credentials: shared by default, per-volume when needed

per-folder-vaults assumed **one** shared cred set (one DB + one R2) backing every
volume — the fleet case. That stays the **default**, but some machines hold
volumes on *different* accounts. So `credentials.toml` becomes **named
credential profiles**:

```toml
# default profile (top-level, unnamed) — the fleet case, unchanged
versions_db          = "postgres://…A"
r2_endpoint          = "https://acctA.r2.cloudflarestorage.com"
r2_access_key_id     = "…"
r2_secret_access_key = "…"

[profiles.work]                     # a second, independent account
versions_db          = "postgres://…B"
r2_endpoint          = "https://acctB.r2.cloudflarestorage.com"
r2_access_key_id     = "…"
r2_secret_access_key = "…"
```

A volume picks a profile (defaults to the unnamed/`default` one):

```toml
# volumes/work.toml
bucket      = "https://acctB.r2.cloudflarestorage.com/trove-work"
schema      = "trove_work"
mountpoint  = "…"
cache       = "…"
credentials = "work"                # ← omit ⇒ default
```

- **Resolution** per volume: the named profile's `{db, r2 endpoint + keys}` →
  else `default` → else env. **Env maps to the `default` profile only** (the
  op/1Password single-cred path); multi-account machines use file profiles.
- **`trove init --profile <name>`** attaches a volume under that cred set; if the
  profile is new, it prompts for its creds and saves them under
  `[profiles.<name>]`. No flag ⇒ `default` (the common case is unchanged — you
  never see profiles unless you need them).
- **Why named profiles, not inline-per-volume creds:** keeps **every secret in
  the one `chmod 600` file** and lets several volumes share a cred set without
  duplicating it. Volume configs stay non-secret (a profile name + the bucket
  URL). per-folder-vaults' blast-radius rule is untouched — DB URL never in the
  bucket, R2 keys never in the DB; profiles all live in the local cred store,
  which was always allowed.
- **Lifecycle impact: none.** Each volume's boot agent mounts `--volume <name>`,
  which resolves *that* volume's profile — so volumes on different accounts
  auto-mount independently and coexist. Fleet (shared) and multi-account volumes
  mix freely on one machine.

## Decisions

1. `attach` **auto-installs** the agent; `--no-autostart` opts out (installing a
   login agent is a system change some will want to skip).
2. **Restart policy is conservative.** RunAtLoad / start-at-login: yes.
   Auto-restart-on-crash: **off (or heavily throttled) initially** — a
   crash-looping mount remounting every few seconds is exactly how a bad mount
   re-wedges the machine (the macOS lock-up we hit). Make resilience opt-in once
   the mount path is proven.
3. **No `kill`** — destroying a vault is manual and deliberate (symmetry with
   user-creates-the-bucket).
4. **Logs:** macOS → `~/Library/Logs/trove/<vault>.log`; Linux → journald. (This
   is also where a backgrounded mount's output goes — answers "where do logs
   collect" for the non-foreground case.)
5. `detach` keeps the backend; it only removes **this machine's** config + agent.

## Build order

1. `mount` / `unmount` resolve a vault by `--volume <name>` from config alone (no
   cwd needed) — prerequisite for the agent.
1b. Credential **profiles**: parse named profiles in `credentials.toml`, add the
   optional `credentials = "<profile>"` field to per-volume config, and resolve
   each volume's creds via its profile → `default` → env. `init --profile <name>`
   prompts + saves a new profile. (Backward-compatible: an existing single-set
   `credentials.toml` is the `default` profile.)
2. Platform agent layer: `install_agent` / `remove_agent` / `agent_status` —
   macOS LaunchAgent + Linux systemd user unit, behind one cross-platform trait.
3. `attach` (= `init`) installs the agent unless `--no-autostart`; `detach`
   unmounts + removes the config + removes the agent.
4. `trove ls` — fleet status.
5. Docs: the two-pairs lifecycle, per-platform agent + log locations, "destroy is
   manual"; scrub residual user-facing "clone" wording.

Suggested version: **0.4.0** (new surface; additive, no backend change).
