#!/bin/sh
# Mount the vault (creating or attaching) and then block — the foreground mount
# IS the container's main process, so the mount lives as long as the container.
#
# `--no-autostart` keeps init in the foreground (a container has no login/boot
# agent to install into, and we *want* the blocking mount as PID 1's child).
# Without it, init would install a per-vault boot agent and return immediately.
#
# `trove init` derives the schema + bucket from the folder name (always
# "notes"), so every instance targets the *same* vault: the first to run creates
# it, the rest attach. If an attacher starts during the brief window before the
# creator has finished, `init` exits non-zero (a create/attach conflict) and we
# retry — but the compose healthcheck normally sequences A-then-B so it's clean.
set -u
VAULT="${VAULT_DIR:-/vault/notes}"
mkdir -p "$VAULT"
cd "$VAULT" || exit 1

echo "[trove-mount role=${ROLE:-?}] init/attach $VAULT"
i=0
until trove init --no-embed --no-autostart; do
  i=$((i + 1))
  if [ "$i" -ge 30 ]; then
    echo "[trove-mount] gave up after $i attempts"
    exit 1
  fi
  echo "[trove-mount] vault not ready yet (peer still creating?) — retry $i in 2s"
  sleep 2
done
