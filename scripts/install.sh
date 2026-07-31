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
#   2. Resolves the version to install (independent trusted manifest, or $DBMD_VERSION).
#   3. Downloads the matching tarball from the GitHub release assets.
#   4. SHA256-verifies it against an independently deployed digest.
#   5. Installs the `dbmd` binary to ~/.dbmd/bin/ (no sudo).
#   6. Prints the PATH line to add (and detects if it is already on PATH).
#
# POSIX sh. No bashisms. No sudo. Honors $DBMD_INSTALL_DIR, $DBMD_VERSION,
# $DBMD_REPO, $DBMD_BASE_URL, $DBMD_TRUSTED_LATEST_URL, and
# $DBMD_TRUSTED_MANIFEST_BASE.
#
# Linux always installs the static musl build, so it runs on any glibc/musl
# distro without a libc version dance.

set -eu

# ── Configuration (override via env) ─────────────────────────────────────────
# The GitHub repo that hosts the release artifacts. Release assets live at
# https://github.com/<repo>/releases/download/v<version>/<asset>. The latest
# version and per-asset digest are resolved independently from sevrahq.com.
DBMD_REPO="${DBMD_REPO:-carloslfu/db.md}"
DBMD_BASE_URL_WAS_SET="${DBMD_BASE_URL+x}"
# Base URL the tarball + SHA256SUMS are fetched from. Per-version assets live at
# $DBMD_BASE_URL/v<version>/<asset>. Defaults to the GitHub release-download base;
# override $DBMD_BASE_URL to point at a local mirror (the release smoke test
# serves the just-built tree over http://127.0.0.1:8099 and sets this).
DBMD_BASE_URL="${DBMD_BASE_URL:-https://github.com/${DBMD_REPO}/releases/download}"
# A digest on a separately deployed origin is the normal trust root. A
# same-release SHA256SUMS file proves download integrity but cannot protect
# against a compromised release publisher replacing both files.
DBMD_TRUSTED_MANIFEST_BASE="${DBMD_TRUSTED_MANIFEST_BASE:-https://www.sevrahq.com/api/hub/releases/dbmd}"
# Independently deployed latest pointer. It returns one exact SemVer line.
DBMD_TRUSTED_LATEST_URL="${DBMD_TRUSTED_LATEST_URL:-${DBMD_TRUSTED_MANIFEST_BASE}/latest}"
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
    version="$(fetch_stdout "$DBMD_TRUSTED_LATEST_URL" | tr -d '[:space:]')"
    [ -n "$version" ] || err "could not resolve latest version from independent manifest"
fi
printf '%s\n' "$version" |
    grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?(\+[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$' ||
    err "invalid release version: $version"

# Same-origin checksums are a controlled-mirror test escape hatch, never a
# production downgrade. They require an explicitly supplied non-GitHub base.
if [ "${DBMD_ALLOW_SAME_ORIGIN_CHECKSUM:-0}" = "1" ]; then
    [ -n "$DBMD_BASE_URL_WAS_SET" ] ||
        err "same-origin checksum opt-in requires an explicit custom DBMD_BASE_URL"
    case "$DBMD_BASE_URL" in
        https://github.com/*/releases/download | https://github.com/*/releases/download/)
            err "same-origin checksum opt-in is forbidden for the official GitHub release origin" ;;
    esac
fi
info "Installing dbmd v${version} for ${plat_os}/${plat_arch}..."

# ── Download + verify ─────────────────────────────────────────────────────────
tarball="dbmd-${version}-${asset_target}.tar.gz"
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

base="$DBMD_BASE_URL/v$version"
info "Downloading $tarball..."
fetch "$base/$tarball" "$workdir/$tarball"

# Verify SHA256 against the independent origin. A custom mirror used in CI or
# an air-gapped deployment must provide its own independent digest endpoint.
# The weaker same-origin manifest path exists only behind an explicit,
# loudly-named opt-in; it is never an outage fallback.
info "Verifying checksum..."
sha_tool=""
if have sha256sum; then sha_tool="sha256sum"; elif have shasum; then sha_tool="shasum -a 256"; fi
[ -n "$sha_tool" ] || err "need sha256sum or shasum to verify the download"
actual="$( $sha_tool "$workdir/$tarball" | cut -d' ' -f1 )"
if [ "${DBMD_ALLOW_SAME_ORIGIN_CHECKSUM:-0}" = "1" ]; then
    fetch "$base/SHA256SUMS" "$workdir/SHA256SUMS"
    expected="$(grep " ${tarball}\$" "$workdir/SHA256SUMS" | awk '{print $1}')"
    info "warning: using explicitly enabled same-origin release checksum"
else
    expected="$(fetch_stdout "$DBMD_TRUSTED_MANIFEST_BASE/$version/$tarball" | tr -d '[:space:]')"
fi
case "$expected" in *[!0-9a-f]* | "") err "no valid independent checksum for $tarball" ;; esac
[ "$actual" = "$expected" ] || err "checksum verification failed for $tarball"
info "checksum: verified"

# ── Unpack + install (atomically) ─────────────────────────────────────────────
info "Installing to $DBMD_INSTALL_DIR..."
tar -xzf "$workdir/$tarball" -C "$workdir"
# Tarball layout: dbmd-<ver>-<target>/dbmd
#
# The verified binary performs the final copy itself. It opens/creates every
# destination component with openat(O_NOFOLLOW), retains the resulting dirfd,
# stages with O_EXCL on that exact filesystem, fsyncs, and renameat(2)s the
# regular `dbmd` leaf. The shell never reopens a mutable destination pathname.
verified="$workdir/dbmd-${version}-${asset_target}/dbmd"
chmod +x "$verified"
"$verified" __install-verified "$verified" "$DBMD_INSTALL_DIR" ||
    err "could not securely install the new binary to $DBMD_INSTALL_DIR/dbmd"
install_stage=""  # consumed by the rename; nothing left for EXIT to remove

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
