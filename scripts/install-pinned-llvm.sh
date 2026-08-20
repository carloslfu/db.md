#!/bin/sh
# SPDX-License-Identifier: Apache-2.0

# Install the exact macOS LLVM archiver used by cargo-xwin. The Windows
# release build needs llvm-lib for ring's static library; a mutable runner or
# package-manager LLVM would make the published executable irreproducible.
set -eu

destination="${1:-}"
[ -n "$destination" ] || {
  printf '%s\n' "usage: scripts/install-pinned-llvm.sh /absolute/destination" >&2
  exit 2
}
case "$destination" in
  /*) ;;
  *)
    printf '%s\n' "LLVM destination must be absolute" >&2
    exit 2
    ;;
esac
[ "$destination" != / ] || {
  printf '%s\n' "refusing to install LLVM at the filesystem root" >&2
  exit 2
}
[ ! -e "$destination" ] || {
  printf '%s\n' "LLVM destination already exists: $destination" >&2
  exit 2
}

archive="$(mktemp "${TMPDIR:-/tmp}/dbmd-llvm-mingw.XXXXXXXX.tar.xz")"
cleanup() {
  status=$?
  trap - EXIT HUP INT TERM
  unlink "$archive" >/dev/null 2>&1 || true
  exit "$status"
}
trap cleanup EXIT HUP INT TERM

url="https://github.com/mstorsjo/llvm-mingw/releases/download/20260616/llvm-mingw-20260616-ucrt-macos-universal.tar.xz"
expected_sha256="2cab02a2e964bd4aae981150a45985d07c657cfa8d244959eb9e2dcc5eedd7b1"
root="llvm-mingw-20260616-ucrt-macos-universal"

curl \
  --fail \
  --silent \
  --show-error \
  --location \
  --proto '=https' \
  --proto-redir '=https' \
  --tlsv1.2 \
  "$url" \
  --output "$archive"
printf '%s  %s\n' "$expected_sha256" "$archive" | shasum -a 256 -c - >/dev/null

mkdir -m 0700 "$destination"
tar -xJf "$archive" -C "$destination" --strip-components=1 \
  "$root/bin/llvm-ar" \
  "$root/bin/llvm-lib" \
  "$root/lib/libLLVM.dylib"

[ -x "$destination/bin/llvm-ar" ] || {
  printf '%s\n' "pinned LLVM archive did not contain executable llvm-ar" >&2
  exit 1
}
if [ ! -L "$destination/bin/llvm-lib" ] ||
  [ "$(readlink "$destination/bin/llvm-lib")" != llvm-ar ]; then
  printf '%s\n' "pinned LLVM archive exposed an unexpected llvm-lib" >&2
  exit 1
fi
"$destination/bin/llvm-ar" --version | grep -Fq 'LLVM version 22.1.8' || {
  printf '%s\n' "pinned LLVM archive reported an unexpected version" >&2
  exit 1
}

printf '%s\n' "$destination/bin"
