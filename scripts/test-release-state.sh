#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
set -eu

repo_root="$(cd -- "$(dirname -- "$0")/.." && pwd)"
. "$repo_root/scripts/release-lib.sh"

fail() {
    printf 'release state test: %s\n' "$*" >&2
    exit 1
}

# Every successor-controller state first performs the unconditional rebuild in
# release.sh, then this action decides only whether approval is still needed.
test "$(release_resume_action '7	true' in_progress)" = approve ||
    fail "fresh pending state must approve after rebuild"
test "$(release_resume_action '' completed)" = resume ||
    fail "completed resume must continue after rebuild"
test "$(release_resume_action '' completed)" = resume ||
    fail "manually-approved successor must continue after rebuild"
test "$(release_resume_action '' completed)" = resume ||
    fail "successful rerun successor must continue after rebuild"
test "$(release_resume_action '' in_progress)" = invalid ||
    fail "unreviewed in-progress state must fail closed"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/dbmd-release-state.XXXXXXXX")"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
version=1.2.3
target=linux-x86_64-musl
source_stage="$tmp/stage/dbmd-${version}-${target}"
mkdir -p "$source_stage"
printf '%s\n' trusted-binary > "$tmp/rebuilt-dbmd"
printf '%s\n' trusted-binary > "$source_stage/dbmd"
printf '%s\n' notice > "$tmp/NOTICE"
printf '%s\n' notice > "$source_stage/NOTICE"
printf '%s\n' third-party > "$tmp/THIRD_PARTY_NOTICES"
printf '%s\n' third-party > "$source_stage/THIRD_PARTY_NOTICES"
printf '%s\n' license > "$tmp/LICENSE"
printf '%s\n' license > "$source_stage/LICENSE"
tar -czf "$tmp/final.tar.gz" -C "$tmp/stage" "dbmd-${version}-${target}"

compare_immutable_target \
    "$tmp/rebuilt-dbmd" "$tmp/final.tar.gz" "$version" "$target" \
    "$tmp/NOTICE" "$tmp/THIRD_PARTY_NOTICES" "$tmp/LICENSE" "$tmp/exact" ||
    fail "an exact immutable artifact must pass"

printf '%s\n' tampered-binary > "$source_stage/dbmd"
tar -czf "$tmp/tampered.tar.gz" -C "$tmp/stage" "dbmd-${version}-${target}"
if compare_immutable_target \
    "$tmp/rebuilt-dbmd" "$tmp/tampered.tar.gz" "$version" "$target" \
    "$tmp/NOTICE" "$tmp/THIRD_PARTY_NOTICES" "$tmp/LICENSE" "$tmp/tampered" \
    >/dev/null 2>&1; then
    fail "a tampered final immutable binary bypassed independent comparison"
fi

printf '%s\n' trusted-binary >"$source_stage/dbmd"
printf '%s\n' unexpected-payload >"$source_stage/extra"
tar -czf "$tmp/extra.tar.gz" -C "$tmp/stage" "dbmd-${version}-${target}"
if compare_immutable_target \
    "$tmp/rebuilt-dbmd" "$tmp/extra.tar.gz" "$version" "$target" \
    "$tmp/NOTICE" "$tmp/THIRD_PARTY_NOTICES" "$tmp/LICENSE" "$tmp/extra" \
    >/dev/null 2>&1; then
    fail "an archive with an unexpected payload bypassed the exact-shape gate"
fi
rm "$source_stage/extra"

rm "$source_stage/dbmd"
ln -s "$tmp/rebuilt-dbmd" "$source_stage/dbmd"
tar -czf "$tmp/symlink.tar.gz" -C "$tmp/stage" "dbmd-${version}-${target}"
if compare_immutable_target \
    "$tmp/rebuilt-dbmd" "$tmp/symlink.tar.gz" "$version" "$target" \
    "$tmp/NOTICE" "$tmp/THIRD_PARTY_NOTICES" "$tmp/LICENSE" "$tmp/symlink" \
    >/dev/null 2>&1; then
    fail "an archive with a symlinked binary bypassed the regular-member gate"
fi

printf '%s\n' "release state tests passed"
