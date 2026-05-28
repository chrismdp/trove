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

if [ "$FORCE" -eq 1 ]; then
    echo "--force: wiping $BUILD_DIR"
    rm -rf "$BUILD_DIR"
fi

# Cache hit: built artefact + marker file at the pinned SHA.
if [ -f "$BUILD_DIR/$LIBFILE" ] && [ -f "$MARKER" ] && [ "$(cat "$MARKER")" = "$JUICEFS_SHA" ]; then
    echo "libjfs cached at $BUILD_DIR/$LIBFILE (SHA $JUICEFS_SHA)"
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

# Stamp the SHA so subsequent runs detect a cache hit.
printf '%s\n' "$JUICEFS_SHA" > "$MARKER"

# Clean up the source worktree — we only need the built artefact.
rm -rf "$WORKTREE"

echo "built $BUILD_DIR/$LIBFILE"
