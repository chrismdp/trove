# The vault lifecycle

A FUSE mount never survives a reboot — that's true of all FUSE, not just trove.
So a vault you actually live in needs a way to come back at login, to go up and
down on demand, and to be cleanly removed from a machine without touching the
shared backend. That's the lifecycle: **two command pairs**, plus a fleet view.

| command | live mount | local config + boot agent | backend (schema + bucket) |
|---|---|---|---|
| `init` / `attach` | mounts | **writes + installs** | creates-or-attaches |
| `mount` | mounts | unchanged | — |
| `unmount` | unmounts | unchanged | — |
| `detach` | unmounts | **removes** | **untouched** |
| *(destroy a vault)* | — | — | **manual** — `drop schema` + delete the bucket |

## `attach` (= `init`) / `detach` — machine membership

`attach` sets *this machine* up: create-or-attach the backend, write the
per-volume config, install the boot agent, and mount. `init` is kept as a
familiar alias (the `git init` / `npm init` reflex).

`detach` removes this machine's footprint — unmount, delete the config, remove
the agent — and **leaves the vault alive in its backend**. Other machines are
unaffected, and you can `attach` here again later.

```bash
cd notes && trove init       # set this machine up + mount in the background
trove detach --volume notes  # remove it from this machine; backend untouched
```

## `mount` / `unmount` — runtime up/down

Transient. They never touch the config or the boot agent.

```bash
trove unmount --volume notes   # down for now — comes back at the next login
trove mount   --volume notes   # bring it back up now
```

`mount --volume <name>` resolves the *entire* vault — mountpoint, schema, cache,
credentials — from its saved config, with no working directory and no ambient
environment. That's exactly what the boot agent runs.

Inside a vault folder you can drop the `--volume` flag; `unmount`/`detach` fall
back to the vault of the current directory.

## `trove ls` — the fleet view

One machine routinely holds many vaults. `trove ls` makes that legible:

```
trove: 2 vault(s) on this machine

  VOLUME               MOUNT      AGENT       MOUNTPOINT
  notes                mounted    running     /home/you/notes
  work                 down       installed   /home/you/work
```

- **MOUNT** — is the FUSE filesystem live right now?
- **AGENT** — `running` (loaded + will mount at login), `installed` (will mount
  at login, not currently loaded), or `none`.

## Auto-mount: one boot agent per vault

The set of installed agents **is** the machine's vault membership: one is
installed at `attach`, removed at `detach`, and never touched by
`mount`/`unmount`. Detach the last vault and nothing lingers — there's no shared
singleton service to orphan. A second/third `attach` adds a second/third agent;
they're keyed on the vault's canonical name, so they never collide and
`unmount`/`detach` on one never affects another.

### macOS — LaunchAgent

- `~/Library/LaunchAgents/com.trove.<vault>.plist`, `RunAtLoad` (mounts at login)
- runs `trove mount --volume <vault>`
- logs → `~/Library/Logs/trove/<vault>.log`
- The mount needs **macFUSE installed + approved**. If it isn't, the agent's
  mount fails and the macFUSE setup guidance lands in that per-vault log.

### Linux — systemd `--user` service

- one template unit `~/.config/systemd/user/trove@.service`, instanced per vault
  as `trove@<vault>.service` (a single template covers any number of vaults)
- `systemctl --user enable --now trove@<vault>.service`
- logs → the journal: `journalctl --user -u trove@<vault>`
- Mounting at true boot time, *before* an interactive login, needs
  `loginctl enable-linger <user>`. It isn't forced — by default the vault mounts
  when you log in.

### Restart policy is conservative

Start-at-login is on; **auto-restart-on-crash is off**. A crash-looping mount
remounting every few seconds is exactly how a bad mount re-wedges a machine.
Resilience is opt-in once the mount path is proven on your setup. An `unmount`
therefore stays down until the next login (or an explicit `mount`).

### Skipping the agent

`trove init --no-autostart` writes the config and mounts in the **foreground**
(blocks the terminal, like a bare `trove mount`) without installing a login
agent — for when you'd rather not make a system change.

## No `kill` — destroying a vault is manual

There is deliberately **no command that destroys a vault**. To remove one for
good you drop its Postgres schema and delete its R2 bucket by hand:

```sql
drop schema trove_notes cascade;     -- in psql
```

…then delete the `trove-notes` bucket in the R2 dashboard.

This is the right symmetry: trove never *creates* the bucket either (you make it
in the dashboard; trove only validates it). So it must not *destroy* it — keeping
a "nuke the shared vault" footgun out of the CLI entirely. `detach` is the safe,
reversible operation; destruction is the deliberate, manual one.

## Where mount logs collect

A backgrounded mount's stdout/stderr go to the same place as the boot agent's:
`~/Library/Logs/trove/<vault>.log` on macOS, the journal
(`journalctl --user -u trove@<vault>`) on Linux. That's where to look when a
vault shows `down` in `trove ls` but you expected it up.
