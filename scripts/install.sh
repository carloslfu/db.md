#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# dbmd installer — the open standard for databases in plain files.
#
#   curl -fsSL https://raw.githubusercontent.com/carloslfu/db.md/main/scripts/install.sh | sh
#
# Or, if you have the Rust toolchain, simply: cargo install dbmd-cli
#
# What it does:
#   1. Detects your platform (darwin / linux  ×  x86_64 / aarch64).
#   2. Resolves the version to install (latest GitHub release, or $DBMD_VERSION).
#   3. Downloads the matching tarball from the GitHub release assets.
#   4. SHA256-verifies it against that release's SHA256SUMS manifest.
#   5. Installs the `dbmd` binary to ~/.dbmd/bin/ (no sudo).
#   6. Prints the PATH line to add (and detects if it is already on PATH).
#
# POSIX sh. No bashisms. No sudo. Honors $DBMD_INSTALL_DIR and $DBMD_VERSION.
#
# Linux always installs the static musl build, so it runs on any glibc/musl
# distro without a libc version dance.

set -eu

# ── Configuration (override via env) ─────────────────────────────────────────
# The GitHub repo that hosts the releases. Release assets live at
# https://github.com/<repo>/releases/download/v<version>/<asset>, and the
# latest version is resolved from the GitHub releases API.
DBMD_REPO="${DBMD_REPO:-carloslfu/db.md}"
DBMD_RELEASES="https://github.com/${DBMD_REPO}/releases/download"
# Where the binary lands. Default ~/.dbmd/bin (no sudo).
DBMD_INSTALL_DIR="${DBMD_INSTALL_DIR:-$HOME/.dbmd/bin}"
# Pinned version (without leading v). Empty => resolve "latest".
DBMD_VERSION="${DBMD_VERSION:-}"

# ── Helpers ──────────────────────────────────────────────────────────────────
err() { printf 'error: %s\n' "$*" >&2; exit 1; }
info() { printf '%s\n' "$*"; }

have() { command -v "$1" >/dev/null 2>&1; }

# Download $1 -> $2. Prefer curl, fall back to wget.
fetch() {
    url="$1"; out="$2"
    if have curl; then
        curl -fsSL "$url" -o "$out" || err "download failed: $url"
    elif have wget; then
        wget -qO "$out" "$url" || err "download failed: $url"
    else
        err "need curl or wget"
    fi
}

# Print to stdout (used for API resolution). Prefer curl, fall back to wget.
fetch_stdout() {
    url="$1"
    if have curl; then
        curl -fsSL "$url" || err "request failed: $url"
    elif have wget; then
        wget -qO- "$url" || err "request failed: $url"
    else
        err "need curl or wget"
    fi
}

# ── Detect platform ──────────────────────────────────────────────────────────
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Darwin) plat_os="darwin" ;;
    Linux)  plat_os="linux" ;;
    *) err "unsupported OS: $os (darwin/linux only; on Windows use WSL)" ;;
esac
case "$arch" in
    x86_64|amd64) plat_arch="x86_64" ;;
    arm64|aarch64) plat_arch="aarch64" ;;
    *) err "unsupported arch: $arch" ;;
esac
# Linux uses the static musl build (runs on any distro).
if [ "$plat_os" = "linux" ]; then
    asset_target="linux-${plat_arch}-musl"
else
    asset_target="darwin-${plat_arch}"
fi

# ── Resolve version ──────────────────────────────────────────────────────────
if [ -n "$DBMD_VERSION" ]; then
    version="$DBMD_VERSION"
else
    info "Resolving latest dbmd release..."
    # Parse the GitHub releases API for the latest tag_name (e.g. "v0.2.0"),
    # then strip the leading "v". POSIX-friendly: grep + sed, no jq.
    api="https://api.github.com/repos/${DBMD_REPO}/releases/latest"
    tag="$(fetch_stdout "$api" | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name"[^"]*"([^"]+)".*/\1/')"
    version="${tag#v}"
    [ -n "$version" ] || err "could not resolve latest version from $api"
fi
info "Installing dbmd v${version} for ${plat_os}/${plat_arch}..."

# ── Download + verify ─────────────────────────────────────────────────────────
tarball="dbmd-${version}-${asset_target}.tar.gz"
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

base="$DBMD_RELEASES/v$version"
info "Downloading $tarball..."
fetch "$base/$tarball" "$workdir/$tarball"

# Verify SHA256 against the release manifest.
info "Verifying checksum..."
fetch "$base/SHA256SUMS" "$workdir/SHA256SUMS"
sha_tool=""
if have sha256sum; then sha_tool="sha256sum"; elif have shasum; then sha_tool="shasum -a 256"; fi
[ -n "$sha_tool" ] || err "need sha256sum or shasum to verify the download"
( cd "$workdir" && grep " ${tarball}\$" SHA256SUMS | $sha_tool -c - ) \
    || err "checksum verification failed for $tarball"

# ── Unpack + install ──────────────────────────────────────────────────────────
info "Installing to $DBMD_INSTALL_DIR..."
mkdir -p "$DBMD_INSTALL_DIR"
tar -xzf "$workdir/$tarball" -C "$workdir"
# Tarball layout: dbmd-<ver>-<target>/dbmd
mv "$workdir/dbmd-${version}-${asset_target}/dbmd" "$DBMD_INSTALL_DIR/dbmd"
chmod +x "$DBMD_INSTALL_DIR/dbmd"

# ── PATH hint ──────────────────────────────────────────────────────────────────
info "Installed dbmd to $DBMD_INSTALL_DIR/dbmd"
case ":$PATH:" in
    *":$DBMD_INSTALL_DIR:"*) : ;;  # already on PATH
    *) info ""
       info "Add to PATH:"
       info "  export PATH=\"$DBMD_INSTALL_DIR:\$PATH\""
       ;;
esac

info "Done. Run 'dbmd spec' to print the canonical SPEC."
info "Run 'dbmd --help' for the full subcommand surface."
