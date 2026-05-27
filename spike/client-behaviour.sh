#!/usr/bin/env bash
# client-behaviour.sh — does a real Unix client NOTICE when Trove's validation
# gate rejects its write? Mounts a real Trove volume with a `person` schema
# (age must be an integer), then writes schema-violating content with a battery
# of ordinary clients and records, for each: exit code, whether the bad bytes
# persisted (they never should), and whether a .errors sidecar was written.
#
# Finding (2026-05-27): the gate ALWAYS holds (invalid never persists, .errors
# always written). Whether the *client* finds out depends on the client — tools
# that check close()/fclose() (cp, dd, tee, Python, careful C) get a non-zero
# exit because Trove commits at `flush` (which reaches close()); fire-and-forget
# close-ignorers (bash `>` redirect, C ignoring fclose) silently see success.
#
# Requires: trove built with `--features mount`, the juicefs binary, /dev/fuse.
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TROVE="${TROVE_BIN:-$ROOT/target/debug/trove}"
JUICEFS="${JUICEFS_BIN:-$ROOT/spike/juicefs/juicefs}"
[ -x "$TROVE" ] || { echo "build first: cargo build --features mount"; exit 1; }

WORK=$(mktemp -d /tmp/trove-clientbe-XXXX); VOL="cb$(date +%s)"
META="sqlite3://$WORK/meta.db"; CACHE="$WORK/cache"; MNT="$WORK/mnt"; SCH="$WORK/schemas"
mkdir -p "$MNT" "$SCH/.types" "$CACHE"
echo '{ "globs":["people/*.md"],"type":"object","required":["type"],"properties":{"type":{"const":"person"},"age":{"type":"integer"}} }' > "$SCH/.types/person.json"
"$JUICEFS" format --storage file --bucket "$WORK/store/" "$META" "$VOL" >/dev/null 2>&1 || { echo "juicefs format failed"; exit 1; }
"$TROVE" mount "$MNT" --volume "$VOL" --meta "$META" --cache "$CACHE" --types "$SCH" >"$WORK/mount.log" 2>&1 &
PID=$!
for i in $(seq 1 80); do mountpoint -q "$MNT" && break; sleep 0.1; done
mountpoint -q "$MNT" || { echo "mount failed:"; cat "$WORK/mount.log"; kill $PID; exit 1; }
mkdir -p "$MNT/people"

BAD=$'---\ntype: person\nage: oops\n---\nbody\n'   # age is a string -> schema violation
GOOD=$'---\ntype: person\nage: 30\n---\nbody\n'
printf '%s' "$BAD" > "$WORK/bad.src"; printf '%s' "$GOOD" > "$WORK/good.src"
cat > "$WORK/wc.c" <<'C'
#include <stdio.h>
#include <string.h>
int main(int c,char**v){FILE*f=fopen(v[1],"w");if(!f)return 2;fwrite(v[2],1,strlen(v[2]),f);int r=fclose(f);if(c>3)return r?1:0;return 0;}
C
cc -O0 -o "$WORK/wc" "$WORK/wc.c" 2>/dev/null

probe(){ local name="$1" file="$2"; shift 2
  "$@" >/dev/null 2>"$WORK/e"; local ex=$?; sleep 0.2
  local got; got=$(cat "$file" 2>/dev/null)
  local verdict; if echo "$got" | grep -q oops; then verdict="BAD-PERSISTED(silent!)"; elif [ -n "$got" ]; then verdict="other"; else verdict="rejected"; fi
  local errs="no"; [ -s "$file.errors" ] && errs="yes"
  printf '%-26s | exit=%-3s | %-20s | .errors=%s\n' "$name" "$ex" "$verdict" "$errs"
}

echo "## writing INVALID (age: oops) to a governed people/*.md, one client each:"
probe "cp (coreutils)"      "$MNT/people/a.md" cp "$WORK/bad.src" "$MNT/people/a.md"
probe "bash > redirect"     "$MNT/people/b.md" bash -c 'printf "%s" "$1">"$2"' _ "$BAD" "$MNT/people/b.md"
probe "dd (plain)"          "$MNT/people/c.md" bash -c 'printf "%s" "$1"|dd of="$2" 2>/dev/null' _ "$BAD" "$MNT/people/c.md"
probe "dd conv=fsync"       "$MNT/people/d.md" bash -c 'printf "%s" "$1"|dd of="$2" conv=fsync 2>/dev/null' _ "$BAD" "$MNT/people/d.md"
probe "tee"                 "$MNT/people/e.md" bash -c 'printf "%s" "$1"|tee "$2">/dev/null' _ "$BAD" "$MNT/people/e.md"
probe "python with-open"    "$MNT/people/f.md" python3 -c 'import sys
with open(sys.argv[1],"w") as fh: fh.write(sys.argv[2])' "$MNT/people/f.md" "$BAD"
probe "python fsync+check"  "$MNT/people/g.md" python3 -c 'import sys,os
fh=open(sys.argv[1],"w"); fh.write(sys.argv[2]); fh.flush()
try: os.fsync(fh.fileno())
except OSError: sys.exit(7)
fh.close()' "$MNT/people/g.md" "$BAD"
probe "C ignore-fclose"     "$MNT/people/h.md" "$WORK/wc" "$MNT/people/h.md" "$BAD"
probe "C check-fclose"      "$MNT/people/i.md" "$WORK/wc" "$MNT/people/i.md" "$BAD" check
echo "## control — VALID write commits:"
probe "cp VALID (control)"  "$MNT/people/ok.md" cp "$WORK/good.src" "$MNT/people/ok.md"
echo
echo "## example sidecar (people/a.md.errors):"; cat "$MNT/people/a.md.errors" 2>/dev/null | sed 's/^/    /'

fusermount3 -uz "$MNT" 2>/dev/null; kill -9 $PID 2>/dev/null; wait 2>/dev/null; rm -rf "$WORK"
