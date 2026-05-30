#!/bin/sh
# Build libjfs from upstream JuiceFS at a pinned SHA with trove's patches.
#
# Output: libjfs/build/libjfs-<arch>.{so,dylib} + matching header.
# Idempotent: re-running with an existing build/ no-ops unless --force is passed.
#
# Env vars:
#   LIBJFS_BUILD_DIR  override the build output dir (default: libjfs/build/)
#   GO                go binary (default: `go`)
set -eu

JUICEFS_SHA="4811a1129d78f5972650e38faf9106727e9e745b"
JUICEFS_URL="https://github.com/juicedata/juicefs.git"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
BUILD_DIR="${LIBJFS_BUILD_DIR:-$SCRIPT_DIR/build}"
GO="${GO:-go}"

FORCE=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        -h|--help)
            sed -n '2,11p' "$0"
            exit 0
            ;;
        *)
            echo "unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

# Detect arch — match JuiceFS Makefile naming.
uname_m="$(uname -m)"
case "$uname_m" in
    x86_64|amd64)   ARCH="amd64" ;;
    aarch64|arm64)  ARCH="arm64" ;;
    *)
        echo "unsupported architecture: $uname_m" >&2
        exit 1
        ;;
esac

case "$(uname -s)" in
    Darwin)  EXT="dylib" ;;
    Linux)   EXT="so" ;;
    *)
        echo "unsupported OS: $(uname -s)" >&2
        exit 1
        ;;
esac

LIBFILE="libjfs-${ARCH}.${EXT}"
HEADERFILE="libjfs-${ARCH}.h"
MARKER="$BUILD_DIR/.sha"

# Cache identity = pinned juicefs SHA + a hash of this script, every patch, and
# the license-generation inputs (collect-licenses.sh, the head, the curated
# notes). Keying on the SHA alone is a trap: editing a patch (e.g. adding the
# locks export) or a license note leaves JUICEFS_SHA unchanged, so a restored
# stale build/ would be treated as a cache hit and the new symbols / license
# text would never be regenerated. CI restores build/ via a partial cache-key
# match, so this guard is what forces the rebuild. (sha256sum / shasum.)
if command -v sha256sum >/dev/null 2>&1; then
    HASHER="sha256sum"
else
    HASHER="shasum -a 256"
fi
PATCH_HASH="$(cat "$0" \
    "$SCRIPT_DIR"/patches/*.patch \
    "$SCRIPT_DIR/collect-licenses.sh" \
    "$SCRIPT_DIR/THIRD-PARTY-LICENSES.head.md" \
    "$SCRIPT_DIR"/license-notes/*.txt \
    2>/dev/null | $HASHER | cut -d' ' -f1)"
STAMP="${JUICEFS_SHA}-${PATCH_HASH}"

if [ "$FORCE" -eq 1 ]; then
    echo "--force: wiping $BUILD_DIR"
    rm -rf "$BUILD_DIR"
fi

# Cache hit: built artefact + the license manifest + marker matching the SHA
# *and* the patch hash. The license file is part of the hit condition so a
# partial cache restore (lib without manifest) falls through and regenerates.
if [ -f "$BUILD_DIR/$LIBFILE" ] && [ -f "$BUILD_DIR/THIRD-PARTY-LICENSES.md" ] \
        && [ -f "$MARKER" ] && [ "$(cat "$MARKER")" = "$STAMP" ]; then
    echo "libjfs cached at $BUILD_DIR/$LIBFILE (stamp $STAMP)"
    exit 0
fi

if ! command -v "$GO" >/dev/null 2>&1; then
    echo "error: $GO not found on PATH" >&2
    exit 1
fi

mkdir -p "$BUILD_DIR"
WORKTREE="$BUILD_DIR/juicefs"
rm -rf "$WORKTREE"
mkdir -p "$WORKTREE"

echo "fetching juicefs@${JUICEFS_SHA}..."
if command -v curl >/dev/null 2>&1; then
    curl -fsSL "https://github.com/juicedata/juicefs/archive/${JUICEFS_SHA}.tar.gz" \
        | tar xz --strip-components=1 -C "$WORKTREE"
elif command -v wget >/dev/null 2>&1; then
    wget -q -O - "https://github.com/juicedata/juicefs/archive/${JUICEFS_SHA}.tar.gz" \
        | tar xz --strip-components=1 -C "$WORKTREE"
else
    echo "fallback: git clone"
    git clone --filter=blob:none "$JUICEFS_URL" "$WORKTREE"
    (cd "$WORKTREE" && git checkout "$JUICEFS_SHA")
fi

# Apply patches in sorted order.
for p in "$SCRIPT_DIR/patches/"*.patch; do
    [ -f "$p" ] || continue
    echo "applying $(basename "$p")"
    (cd "$WORKTREE" && patch -p1 < "$p")
done

echo "running make in $WORKTREE/sdk/java/libjfs..."
(cd "$WORKTREE/sdk/java/libjfs" && $GO version && make)

# Copy artefacts up to BUILD_DIR so build.rs / packaging only have to look in one place.
cp "$WORKTREE/sdk/java/libjfs/$LIBFILE" "$BUILD_DIR/$LIBFILE"
if [ -f "$WORKTREE/sdk/java/libjfs/$HEADERFILE" ]; then
    cp "$WORKTREE/sdk/java/libjfs/$HEADERFILE" "$BUILD_DIR/$HEADERFILE"
fi

# Generate the third-party license manifest from the exact module set just
# built. collect-licenses.sh needs the worktree (go list + the module cache),
# so this must run before cleanup. Apache 2.0 §4 requires shipping JuiceFS's
# license + attribution alongside the binary; the statically-linked Go deps
# carry their own notices. A failure here aborts before the cache is stamped,
# so we never mark a build "done" without its manifest.
echo "generating THIRD-PARTY-LICENSES.md..."
LICENSE_OUT="$BUILD_DIR/THIRD-PARTY-LICENSES.md"
cat "$SCRIPT_DIR/THIRD-PARTY-LICENSES.head.md" > "$LICENSE_OUT"
printf '\n' >> "$LICENSE_OUT"
GO="$GO" sh "$SCRIPT_DIR/collect-licenses.sh" "$WORKTREE" >> "$LICENSE_OUT"
echo "wrote $LICENSE_OUT"

# Stamp the SHA + patch hash so subsequent runs detect a cache hit.
printf '%s\n' "$STAMP" > "$MARKER"

# Clean up the source worktree — we only need the built artefact.
rm -rf "$WORKTREE"

echo "built $BUILD_DIR/$LIBFILE"
