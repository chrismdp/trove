#!/bin/sh
# Generate the "Bundled Go modules" section of THIRD-PARTY-LICENSES.md.
#
# Walks the exact set of modules linked into the libjfs c-shared build (the
# default `-tags nogspt` configuration the Makefile uses) and concatenates each
# module's license text from the Go module cache. Tool-free — needs only `go`
# and a POSIX shell, so it adds no extra CI dependency (unlike go-licenses).
#
# Output: Markdown to stdout. For any module whose license text can't be found,
# it emits a visible placeholder AND prints a WARNING to stderr — so bumping
# JUICEFS_SHA surfaces any new dependency that ships no license (add a curated
# note under libjfs/license-notes/ for those; see the three already there).
#
# Usage: collect-licenses.sh <juicefs-worktree>
set -eu

WT="${1:?usage: collect-licenses.sh <juicefs-worktree>}"
GO="${GO:-go}"
PKGDIR="$WT/sdk/java/libjfs"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OVERRIDES="$SCRIPT_DIR/license-notes"

[ -d "$PKGDIR" ] || { echo "collect-licenses: $PKGDIR not found" >&2; exit 1; }
command -v "$GO" >/dev/null 2>&1 || { echo "collect-licenses: $GO not on PATH" >&2; exit 1; }

# Enumerate every non-stdlib, non-main module that contributes a package to the
# default libjfs build, as  path|version|cachedir.  `.Main` drops the juicefs
# module itself (covered by the Apache 2.0 section above); empty `.Module`
# drops the standard library.
mods="$(cd "$PKGDIR" && CGO_ENABLED=1 "$GO" list -deps -tags nogspt \
    -f '{{with .Module}}{{if not .Main}}{{.Path}}|{{.Version}}|{{.Dir}}{{end}}{{end}}' . \
    | sort -u | awk 'NF')"

printf '## Bundled Go modules (statically linked into libjfs)\n\n'
printf 'The libjfs shared library statically links the Go modules below. Each is\n'
printf 'listed with its module path, version, and the license text shipped with it.\n\n'

echo "$mods" | while IFS='|' read -r path ver dir; do
    [ -n "$path" ] || continue
    printf -- '--------------------------------------------------------------------------------\n'
    printf '### %s %s\n\n' "$path" "$ver"

    # 1) curated override — for module versions that ship no license file.
    ov="$OVERRIDES/$(echo "$path" | tr '/' '_').txt"
    if [ -f "$ov" ]; then
        cat "$ov"
        printf '\n\n'
        continue
    fi

    # 2) license file at the module root.
    lic="$(find "$dir" -maxdepth 1 -type f \
        \( -iname 'LICEN[SC]E*' -o -iname 'COPYING*' -o -iname 'COPYRIGHT*' \
           -o -iname 'UNLICEN[SC]E*' \) 2>/dev/null | sort | head -1)"
    # 3) fall back to a license file in a subdirectory (per-package licenses).
    [ -n "$lic" ] || lic="$(find "$dir" -type f \
        \( -iname 'LICEN[SC]E*' -o -iname 'COPYING*' -o -iname 'UNLICEN[SC]E*' \) \
        2>/dev/null | sort | head -1)"

    if [ -n "$lic" ]; then
        cat "$lic"
        printf '\n\n'
    else
        printf '_No license text located in this module version. See https://pkg.go.dev/%s_\n\n' "$path"
        echo "collect-licenses: WARNING no license text for $path $ver (add libjfs/license-notes/$(echo "$path" | tr '/' '_').txt)" >&2
    fi
done
