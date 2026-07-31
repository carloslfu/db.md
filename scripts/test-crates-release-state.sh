#!/bin/sh
# SPDX-License-Identifier: Apache-2.0

set -eu

repo_root="$(cd -- "$(dirname -- "$0")/.." && pwd)"
# shellcheck source=scripts/crates-release-lib.sh
. "$repo_root/scripts/crates-release-lib.sh"

fail() {
    printf 'crates release state test: %s\n' "$*" >&2
    exit 1
}

tmp="$(mktemp -d "${TMPDIR:-/tmp}/dbmd-crates-state.XXXXXXXX")"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

expected=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
version=1.2.3

printf '{"version":{"num":"%s","checksum":"%s","yanked":false}}\n' \
    "$version" "$expected" >"$tmp/exact.json"
[ "$(crates_version_state "$tmp/exact.json" "$expected" "$version")" = exact ] ||
    fail "exact non-yanked version was not accepted"

printf '{"version":{"num":"%s","checksum":"%s","yanked":false}}\n' \
    "$version" bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb \
    >"$tmp/mismatch.json"
[ "$(crates_version_state "$tmp/mismatch.json" "$expected" "$version")" = mismatch ] ||
    fail "checksum mismatch was not rejected"

printf '{"version":{"num":"%s","checksum":"%s","yanked":true}}\n' \
    "$version" "$expected" >"$tmp/yanked.json"
[ "$(crates_version_state "$tmp/yanked.json" "$expected" "$version")" = yanked ] ||
    fail "yanked exact version was not rejected"

printf '{"version":{"num":"9.9.9","checksum":"%s","yanked":false}}\n' \
    "$expected" >"$tmp/wrong-version.json"
[ "$(crates_version_state "$tmp/wrong-version.json" "$expected" "$version")" = wrong-version ] ||
    fail "wrong version response was not rejected"

printf '{"version":{"num":"%s","checksum":"%s"}}\n' \
    "$version" "$expected" >"$tmp/malformed.json"
[ "$(crates_version_state "$tmp/malformed.json" "$expected" "$version")" = invalid ] ||
    fail "malformed version response was not rejected"

printf '%s\n' "crates release state tests passed"
