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

test "$(release_artifact_state '7	true' in_progress '')" = ready ||
    fail "pending reviewed release must expose build artifacts"
test "$(release_artifact_state '' completed success)" = ready ||
    fail "completed successful release must remain resumable"
test "$(release_artifact_state '' completed failure)" = failed ||
    fail "failed preflight must not fall through to artifact download"
test "$(release_artifact_state '' in_progress '')" = invalid ||
    fail "running release without pending approval must not download artifacts"

# GitHub compare is queried as current-channel tag -> candidate tag. A
# descendant may advance the channel; a candidate behind the served tag is the
# stale-controller exploit and must fail closed.
test "$(release_channel_transition 1.2.2 1.2.3 ahead)" = advance ||
    fail "a trunk descendant must be allowed to advance a channel"
test "$(release_channel_transition 1.2.3 1.2.3 identical)" = exact ||
    fail "an exact idempotent channel resume must be accepted"
test "$(release_channel_transition 1.2.4 1.2.3 behind)" = stale ||
    fail "an older concurrent controller was not classified as stale"
test "$(release_channel_transition 1.2.4 1.2.3 diverged)" = invalid ||
    fail "a diverged release source was not rejected"
test "$(release_channel_transition 1.2.3 1.2.4 identical)" = invalid ||
    fail "different versions on one source commit were not rejected"

# Reproduce the final promotion race: an older controller reads tap head A, a
# newer controller commits head B and promotes its release, then the older
# controller's in-flight `latest` edit lands. The fence must select a repair to
# the strict descendant, never leave the older tag as latest.
test "$(release_latest_fence_action head-a head-a invalid)" = stable ||
    fail "an unchanged tap head was not accepted"
test "$(release_latest_fence_action head-a head-b advance)" = repair-forward ||
    fail "a newer tap race did not force latest forward repair"
test "$(release_latest_fence_action head-a head-b stale)" = invalid ||
    fail "a stale tap race was allowed to choose a latest tag"
test "$(release_latest_fence_action head-a head-b invalid)" = invalid ||
    fail "a diverged tap race was allowed to choose a latest tag"

# Exact pre-edit exploit: the candidate formula was verified at head A, then a
# newer controller advanced the tap to head B before the stale controller
# sampled a head for `latest`. The live sample must be compared with the bound
# verified head; treating B as the new baseline would leave old latest behind.
test "$(release_latest_preflight_action head-a head-a)" = proceed ||
    fail "an exact formula/head binding was not accepted"
test "$(release_latest_preflight_action head-a head-b)" = stale ||
    fail "tap advancement between formula proof and head sample was not rejected"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/dbmd-release-state.XXXXXXXX")"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

printf '%s\n' \
    'class Dbmd < Formula' \
    '  version "1.2.3"' \
    'end' >"$tmp/formula.rb"
test "$(release_formula_version "$tmp/formula.rb")" = 1.2.3 ||
    fail "exact Homebrew formula version was not extracted"
printf '%s\n' \
    'version "1.2.3"' \
    'version "9.9.9"' >"$tmp/ambiguous-formula.rb"
if release_formula_version "$tmp/ambiguous-formula.rb" >/dev/null 2>&1; then
    fail "ambiguous Homebrew formula version bypassed the channel guard"
fi

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
