#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Render the Homebrew formula for dbmd from a release version and its
# SHA256SUMS manifest. Fills the per-target placeholders in
# HomebrewFormula/dbmd.rb.template so the formula always matches the
# bytes a release actually shipped — no hand-editing, no drift.
#
# Usage:
#   HomebrewFormula/render.sh <version-without-v> <path/to/SHA256SUMS> > dbmd.rb
#
# The SHA256SUMS file is the standard `shasum -a 256` manifest the release
# job already produces (one "<hash>  <tarball>" line per target). Called by
# the `homebrew` job in .github/workflows/release.yml; also runnable by hand.
set -euo pipefail

version="${1:?usage: render.sh <version> <SHA256SUMS>}"
sums="${2:?usage: render.sh <version> <SHA256SUMS>}"
here="$(cd "$(dirname "$0")" && pwd)"
template="$here/dbmd.rb.template"

[ -f "$template" ] || { echo "render.sh: template not found at $template" >&2; exit 1; }
[ -f "$sums" ]     || { echo "render.sh: SHA256SUMS not found at $sums" >&2; exit 1; }

# Pull the sha256 for a given tarball out of the manifest. Match on the
# exact filename in field 2 (tolerating a leading '*' from binary-mode
# shasum) rather than a regex, so the dots in the name can't mis-match.
sha_for() {
  tarball="$1"
  hash="$(awk -v t="$tarball" '{ f = $2; sub(/^\*/, "", f) } f == t { print $1; exit }' "$sums")"
  [ -n "$hash" ] || { echo "render.sh: no sha256 for $tarball in $sums" >&2; exit 1; }
  printf '%s' "$hash"
}

sha_darwin_aarch64="$(sha_for "dbmd-${version}-darwin-aarch64.tar.gz")"
sha_darwin_x86_64="$(sha_for "dbmd-${version}-darwin-x86_64.tar.gz")"
sha_linux_aarch64="$(sha_for "dbmd-${version}-linux-aarch64-musl.tar.gz")"
sha_linux_x86_64="$(sha_for "dbmd-${version}-linux-x86_64-musl.tar.gz")"

sed \
  -e "s/__VERSION__/${version}/g" \
  -e "s/__SHA256_DARWIN_AARCH64__/${sha_darwin_aarch64}/g" \
  -e "s/__SHA256_DARWIN_X86_64__/${sha_darwin_x86_64}/g" \
  -e "s/__SHA256_LINUX_AARCH64_MUSL__/${sha_linux_aarch64}/g" \
  -e "s/__SHA256_LINUX_X86_64_MUSL__/${sha_linux_x86_64}/g" \
  "$template"
