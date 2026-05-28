#!/bin/sh
# Trove installer. Detects platform, downloads the latest release from GitHub,
# verifies the sha256, places trove + libjfs in ~/.local/share/trove/<version>/
# and symlinks the binary into ~/.local/bin/trove.
#
# Usage:   curl -fsSL https://raw.githubusercontent.com/chrismdp/trove/main/install.sh | sh
#   or:    curl -fsSL https://raw.githubusercontent.com/chrismdp/trove/main/install.sh | sh -s -- --version v0.2.0
#
# Env overrides:
#   TROVE_INSTALL_DIR   default: ~/.local/share/trove
#   TROVE_BIN_DIR       default: ~/.local/bin

set -eu

REPO="chrismdp/trove"

# ---------- helpers ----------

err() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

info() {
    printf '%s\n' "$*"
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"
}

# ---------- safety ----------

if [ "$(id -u)" = "0" ]; then
    err "do not run as root — trove installs to your home directory.
Re-run as a normal user: curl -fsSL ... | sh"
fi

need_cmd curl
need_cmd uname
need_cmd tar
need_cmd mkdir
need_cmd rm
need_cmd ln

# ---------- arg parsing ----------

VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            shift
            [ $# -gt 0 ] || err "--version requires a value (e.g. v0.2.0)"
            VERSION="$1"
            ;;
        --version=*)
            VERSION="${1#--version=}"
            ;;
        -h|--help)
            sed -n '2,9p' "$0" 2>/dev/null || true
            exit 0
            ;;
        *)
            err "unknown argument: $1"
            ;;
    esac
    shift
done

# ---------- platform detection ----------

uname_s="$(uname -s)"
case "$uname_s" in
    Linux)  OS="linux" ;;
    Darwin) OS="macos" ;;
    *)      err "unsupported OS: $uname_s (trove ships linux + macos only)" ;;
esac

uname_m="$(uname -m)"
case "$uname_m" in
    x86_64|amd64)   ARCH="amd64" ;;
    aarch64|arm64)  ARCH="arm64" ;;
    *)              err "unsupported architecture: $uname_m (trove ships amd64 + arm64 only)" ;;
esac

# Intel Macs aren't supported — refuse with a clear pointer rather than 404 on
# a non-existent release asset. Apple Silicon (arm64) only on macOS.
if [ "$OS" = "macos" ] && [ "$ARCH" = "amd64" ]; then
    err "Intel Macs are not supported. trove ships for Apple Silicon (arm64) only on macOS. See docs/packaging.md."
fi

# ---------- resolve version ----------

if [ -z "$VERSION" ]; then
    info "Resolving latest trove release from github.com/$REPO ..."
    # GitHub's /releases/latest endpoint returns JSON. We avoid a jq
    # dependency by grepping the tag_name field. Anonymous GitHub API is
    # rate-limited to 60 req/hour per IP — usually fine for a one-shot
    # install.
    api_url="https://api.github.com/repos/$REPO/releases/latest"
    release_json="$(curl -fsSL "$api_url")" || err "failed to fetch $api_url
(GitHub may be rate-limiting your IP; retry later or pass --version <tag>)"
    VERSION="$(printf '%s\n' "$release_json" \
        | grep -E '"tag_name"\s*:' \
        | head -n 1 \
        | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
    [ -n "$VERSION" ] || err "could not parse tag_name from GitHub API response"
fi

# Strip leading v for the tarball filename; tags are vX.Y.Z, files use X.Y.Z.
VERSION_NUM="${VERSION#v}"

TARBALL="trove-${VERSION_NUM}-${OS}-${ARCH}.tar.gz"
SHA_FILE="${TARBALL}.sha256"
BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"

# ---------- destination paths ----------

INSTALL_ROOT="${TROVE_INSTALL_DIR:-$HOME/.local/share/trove}"
BIN_DIR="${TROVE_BIN_DIR:-$HOME/.local/bin}"
VERSION_DIR="${INSTALL_ROOT}/${VERSION_NUM}"

# ---------- temp dir + cleanup ----------

TMPDIR_OURS="$(mktemp -d 2>/dev/null || mktemp -d -t trove-install)"
cleanup() {
    rm -rf "$TMPDIR_OURS"
}
trap cleanup EXIT INT HUP TERM

# ---------- download ----------

info "Downloading trove ${VERSION} for ${OS}-${ARCH} ..."
curl -fsSL -o "${TMPDIR_OURS}/${TARBALL}" "${BASE_URL}/${TARBALL}" \
    || err "download failed: ${BASE_URL}/${TARBALL}
(is the tag '${VERSION}' published with binaries for ${OS}-${ARCH}?)"
curl -fsSL -o "${TMPDIR_OURS}/${SHA_FILE}" "${BASE_URL}/${SHA_FILE}" \
    || err "sha256 download failed: ${BASE_URL}/${SHA_FILE}"

# ---------- verify ----------

info "Verifying sha256 ..."
cd "$TMPDIR_OURS"
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$SHA_FILE" >/dev/null 2>&1 \
        || err "sha256 mismatch — download corrupt or tampered with"
elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$SHA_FILE" >/dev/null 2>&1 \
        || err "sha256 mismatch — download corrupt or tampered with"
else
    err "neither sha256sum nor shasum found — cannot verify download"
fi
cd - >/dev/null

# ---------- extract ----------

info "Extracting to ${VERSION_DIR} ..."
mkdir -p "$VERSION_DIR"
tar -C "$VERSION_DIR" -xzf "${TMPDIR_OURS}/${TARBALL}"

# Verify the expected layout landed.
[ -f "${VERSION_DIR}/trove" ] || err "tarball did not contain trove binary"
chmod +x "${VERSION_DIR}/trove"

# ---------- symlink ----------

mkdir -p "$BIN_DIR"
SYMLINK="${BIN_DIR}/trove"
rm -f "$SYMLINK"
ln -s "${VERSION_DIR}/trove" "$SYMLINK"

# ---------- success ----------

info ""
info "Installed trove ${VERSION} to ${VERSION_DIR}/"
info "Symlinked to ${SYMLINK}"

# PATH guidance.
case ":${PATH}:" in
    *":${BIN_DIR}:"*)
        # Already on PATH — nothing to do.
        :
        ;;
    *)
        info ""
        info "Note: ${BIN_DIR} is not on your PATH."
        shell_name="$(basename "${SHELL:-sh}")"
        case "$shell_name" in
            zsh)
                info "  Add this to ~/.zshrc:"
                info "    export PATH=\"${BIN_DIR}:\$PATH\""
                ;;
            bash)
                info "  Add this to ~/.bashrc (or ~/.bash_profile on macOS):"
                info "    export PATH=\"${BIN_DIR}:\$PATH\""
                ;;
            fish)
                info "  Run:"
                info "    fish_add_path ${BIN_DIR}"
                ;;
            *)
                info "  Add ${BIN_DIR} to your PATH in your shell's rc file."
                ;;
        esac
        ;;
esac

# ---------- macOS: warn if macFUSE is missing ----------
# `trove mount` requires macFUSE at runtime. Install/link don't need it,
# but the user should know before they try to mount.
if [ "$OS" = "macos" ]; then
    if ! [ -e /Library/Filesystems/macfuse.fs ] && ! [ -e /Library/Filesystems/osxfuse.fs ]; then
        info ""
        info "Note: macFUSE is not installed on this machine. \`trove mount\` will fail"
        info "until you install it:"
        info ""
        info "  brew install --cask macfuse"
        info ""
        info "(macFUSE requires a one-time approval in System Settings → Privacy & Security"
        info "after installing. Restart required.)"
        info ""
        info "The other trove commands (check, docs, install, search, server, log, cat,"
        info "backup) work without macFUSE."
    fi
fi

info ""
info "Smoke test:   trove --version"
info "Next step:    trove install   (writes config + provisions the substrate)"
