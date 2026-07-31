#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# shellcheck disable=SC2016 # assertions intentionally match literal shell source

# Hermetic regression checks for the release trust boundary. These assertions
# deliberately inspect executable configuration, require no GitHub credentials,
# and fail if a future edit reintroduces mutable builders, standing Homebrew
# credentials, pre-verification publication, or premature `latest` promotion.
set -eu

repo_root="$(cd -- "$(dirname -- "$0")/.." && pwd)"
workflow="$repo_root/.github/workflows/release.yml"
controller="$repo_root/scripts/release.sh"
controller_lib="$repo_root/scripts/release-lib.sh"
crates_controller_lib="$repo_root/scripts/crates-release-lib.sh"
crates_publisher="$repo_root/scripts/publish-crates.sh"
publishability="$repo_root/scripts/check-publishability.sh"
cross_config="$repo_root/Cross.toml"

fail() {
    printf 'release security test: %s\n' "$*" >&2
    exit 1
}

require_fixed() {
    needle="$1"
    file="$2"
    grep -Fq -- "$needle" "$file" ||
        fail "missing required invariant '$needle' in ${file#"$repo_root"/}"
}

reject_fixed() {
    needle="$1"
    file="$2"
    if grep -Fq -- "$needle" "$file"; then
        fail "forbidden release mechanism '$needle' in ${file#"$repo_root"/}"
    fi
}

bash -n "$controller"
sh -n "$crates_publisher"
sh -n "$publishability"
sh "$repo_root/scripts/test-release-state.sh"
sh "$repo_root/scripts/test-crates-release-state.sh"

# Every trusted local input is frozen from the exact reviewed commit in a
# read-only archive before the controller waits on external workflow gates.
require_fixed 'git archive --format=tar "$source_sha" | tar -xf - -C "$release_source"' "$controller"
require_fixed 'cd "$release_source"' "$controller"
require_fixed '"$release_source/NOTICE"' "$controller"
require_fixed '"$release_source/THIRD_PARTY_NOTICES"' "$controller"
require_fixed '"$release_source/LICENSE"' "$controller"
require_fixed '"$release_source/HomebrewFormula/render.sh"' "$controller"
reject_fixed 'git worktree add' "$controller"

# Both Linux release builders must be content-addressed. A tag-only `cross`
# image would let a registry retarget the builder after review.
require_fixed \
    'ghcr.io/cross-rs/x86_64-unknown-linux-musl@sha256:77db671d8356a64ae72a3e1415e63f547f26d374fbe3c4762c1cd36c7eac7b99' \
    "$cross_config"
require_fixed \
    'ghcr.io/cross-rs/aarch64-unknown-linux-musl@sha256:702154f52b2d8091671aa2c84d5582d849f949977228c735ff8462f93cc0e1e4' \
    "$cross_config"
require_fixed 'use_cross: true' "$workflow"
reject_fixed 'apt-get install' "$workflow"
reject_fixed 'musl-tools' "$workflow"

# CI may publish only after a protected approval. It must publish crates before
# making the immutable GitHub release public, and never chooses `latest`.
require_fixed 'environment: release-publishing' "$workflow"
require_fixed 'needs: [version, publish-crates]' "$workflow"
require_fixed '-F draft=false -f make_latest=false' "$workflow"
require_fixed 'run: scripts/publish-crates.sh "$VERSION"' "$workflow"
reject_fixed 'HOMEBREW_TAP_DEPLOY_KEY' "$workflow"
reject_fixed 'DBMD_RELEASE_AUTH' "$workflow"
reject_fixed 'homebrew-publishing' "$workflow"
reject_fixed 'gh release edit' "$workflow"
reject_fixed 'publish_if_new' "$workflow"

# The pre-publication package-source compatibility proof must never rewrite the
# candidate lock. The exact registry-bound proof happens only after core is
# authoritative on crates.io.
require_fixed 'cli_lock_before="$(shasum -a 256 "$cli/Cargo.lock"' "$publishability"
require_fixed 'cli_lock_after="$(shasum -a 256 "$cli/Cargo.lock"' "$publishability"
reject_fixed 'cargo update' "$publishability"

# Publishing is a strict two-phase convergence: exact core package, exact
# crates.io checksum + non-yanked state, then a plain unpatched CLI package and
# untouched locked check. Version-only existence must never count as success.
require_fixed 'cargo package -p dbmd-core --locked' "$crates_publisher"
require_fixed 'crates_version_state "$version_response" "$expected_checksum" "$version"' "$crates_publisher"
require_fixed 'die "$crate_name $version exists but is yanked"' "$crates_publisher"
require_fixed 'die "$crate_name $version exists with a different checksum"' "$crates_publisher"
require_fixed 'cargo package -p dbmd-cli --locked' "$crates_publisher"
require_fixed 'checksum = \"${core_checksum}\"' "$crates_publisher"
require_fixed '--manifest-path "$cli/Cargo.toml"' "$crates_publisher"
require_fixed 'publish_or_resume dbmd-cli "$cli_checksum" "$cli_target"' "$crates_publisher"
reject_fixed 'patch.crates-io' "$crates_publisher"
reject_fixed 'cargo update' "$crates_publisher"
reject_fixed 'curl -sf' "$crates_publisher"
reject_fixed 'DBMD_CRATES_API_BASE' "$crates_publisher"

core_converged_line="$(
    grep -n 'publish_or_resume dbmd-core' "$crates_publisher" |
        tail -n 1 |
        cut -d: -f1
)"
cli_package_line="$(
    grep -n 'cargo package -p dbmd-cli --locked' "$crates_publisher" |
        tail -n 1 |
        cut -d: -f1
)"
cli_check_line="$(
    grep -n -- '--manifest-path "$cli/Cargo.toml"' "$crates_publisher" |
        tail -n 1 |
        cut -d: -f1
)"
cli_publish_line="$(
    grep -n 'publish_or_resume dbmd-cli' "$crates_publisher" |
        tail -n 1 |
        cut -d: -f1
)"
[ -n "$core_converged_line" ] && [ -n "$cli_package_line" ] &&
    [ -n "$cli_check_line" ] && [ -n "$cli_publish_line" ] &&
    [ "$core_converged_line" -lt "$cli_package_line" ] &&
    [ "$cli_package_line" -lt "$cli_check_line" ] &&
    [ "$cli_check_line" -lt "$cli_publish_line" ] ||
    fail "core/CLI exact publish state machine is out of order"

# The approving controller must consume CI artifacts, independently rebuild all
# four targets, byte-compare them, and approve only after those comparisons.
require_fixed 'gh run download "$run_id"' "$controller"
require_fixed 'compare_target darwin-x86_64 x86_64-apple-darwin' "$controller"
require_fixed 'compare_target darwin-aarch64 aarch64-apple-darwin' "$controller"
require_fixed 'compare_target linux-x86_64-musl x86_64-unknown-linux-musl' "$controller"
require_fixed 'compare_target linux-aarch64-musl aarch64-unknown-linux-musl' "$controller"
require_fixed 'pending_deployments' "$controller"
require_fixed 'state: "approved"' "$controller"
require_fixed '--signer-workflow "${SOURCE_REPO}/.github/workflows/${RELEASE_WORKFLOW}"' "$controller"
require_fixed '--source-digest "$source_sha"' "$controller"
require_fixed '--source-ref "refs/tags/${tag}"' "$controller"
require_fixed '--deny-self-hosted-runners' "$controller"

compare_line="$(grep -n 'rebuild_and_compare$' "$controller" | head -n 1 | cut -d: -f1)"
approve_line="$(grep -n 'state: "approved"' "$controller" | head -n 1 | cut -d: -f1)"
[ -n "$compare_line" ] && [ -n "$approve_line" ] &&
    [ "$compare_line" -lt "$approve_line" ] ||
    fail "publishing approval is not ordered after independent rebuild comparison"

# The rebuild call is outside the pending-approval branch, so fresh,
# manually-approved, completed/resumed, and rerun states cannot bypass it.
pending_branch_line="$(grep -n '^if \[ -n "\$pending_record" \]; then$' "$controller" | head -n 1 | cut -d: -f1)"
[ -n "$compare_line" ] && [ -n "$pending_branch_line" ] &&
    [ "$compare_line" -lt "$pending_branch_line" ] ||
    fail "independent rebuild must run before every approval/resume state branch"
[ "$(grep -Ec '^[[:space:]]*rebuild_and_compare$' "$controller")" -eq 1 ] ||
    fail "controller must have one unconditional independent rebuild call"

# The exact immutable release tarballs, not only transient CI artifacts, are
# compared to that rebuild on every path before Homebrew or latest can move.
require_fixed 'compare_final_target darwin-x86_64 x86_64-apple-darwin' "$controller"
require_fixed 'compare_final_target darwin-aarch64 aarch64-apple-darwin' "$controller"
require_fixed 'compare_final_target linux-x86_64-musl x86_64-unknown-linux-musl' "$controller"
require_fixed 'compare_final_target linux-aarch64-musl aarch64-unknown-linux-musl' "$controller"
require_fixed 'actual_entries="$(tar -tzf "$tarball" | LC_ALL=C sort)"' "$controller_lib"
require_fixed 'tar -xOzf "$tarball" "${stage}/dbmd"' "$controller_lib"
reject_fixed 'tar -xzf "$tarball"' "$controller"
final_compare_line="$(grep -n 'compare_final_target linux-aarch64-musl' "$controller" | tail -n 1 | cut -d: -f1)"
homebrew_line="$(grep -n 'HomebrewFormula/render.sh' "$controller" | head -n 1 | cut -d: -f1)"
[ -n "$final_compare_line" ] && [ -n "$homebrew_line" ] &&
    [ "$final_compare_line" -lt "$homebrew_line" ] ||
    fail "immutable final artifacts must match the rebuild before Homebrew"

# The local successor controller independently rejects a yanked crate even
# when the immutable checksum still matches. It shares the exact classifier
# covered by the hermetic yanked fixture rather than reimplementing a weaker
# checksum-only query.
require_fixed 'source "$script_dir/crates-release-lib.sh"' "$controller"
require_fixed 'crates_version_state "$crate_response" "$local_checksum" "$version"' "$controller"
require_fixed 'yanked) die "crates.io ${crate_name} ${version} is yanked"' "$controller"
require_fixed 'actual_checksum="$(' "$crates_controller_lib"
require_fixed 'yanked="$(' "$crates_controller_lib"

# Homebrew uses the authenticated Contents API with an optimistic blob SHA.
# Its current release must be a trunk ancestor, this controller's source must
# still be origin/main, and the blob SHA closes the check/write race. There is
# no generated key, standing tap secret, or SSH transport.
require_fixed 'repos/${TAP_REPO}/contents/Formula/dbmd.rb' "$controller"
require_fixed '--arg sha "$tap_blob_sha"' "$controller"
require_fixed 'release_channel_transition()' "$controller_lib"
require_fixed 'release_formula_version()' "$controller_lib"
require_fixed 'release_latest_fence_action()' "$controller_lib"
require_fixed 'release_latest_preflight_action()' "$controller_lib"
require_fixed 'require_monotonic_channel "$current_tap_version" Homebrew' "$controller"
require_fixed 'require_controller_current Homebrew' "$controller"
reject_fixed 'HOMEBREW_TAP_DEPLOY_KEY' "$controller"
reject_fixed 'DBMD_RELEASE_AUTH' "$controller"
reject_fixed 'ssh-keygen' "$controller"
reject_fixed 'git@github.com' "$controller"
reject_fixed 'gh secret' "$controller"

# `latest` is the final mutation, after exact crates.io, release, attestation,
# and Homebrew verification. It rechecks both the current latest ancestry and
# origin/main, then fences the edit with the tap head. If a newer controller
# advances the tap inside that final window, latest is repaired forward before
# the stale controller fails.
latest_line="$(grep -n 'gh release edit "$tag".*--latest' "$controller" | tail -n 1 | cut -d: -f1)"
formula_verify_line="$(grep -n 'tap formula does not exactly match' "$controller" | tail -n 1 | cut -d: -f1)"
[ -n "$latest_line" ] && [ -n "$formula_verify_line" ] &&
    [ "$latest_line" -gt "$formula_verify_line" ] ||
    fail "latest promotion must occur after exact Homebrew convergence"
latest_monotonic_line="$(grep -n 'require_monotonic_channel "$latest_version" latest' "$controller" | tail -n 1 | cut -d: -f1)"
latest_current_line="$(grep -n 'require_controller_current latest' "$controller" | tail -n 1 | cut -d: -f1)"
verified_tap_line="$(grep -n 'verified_tap_head=' "$controller" | tail -n 1 | cut -d: -f1)"
tap_fence_before_line="$(grep -n 'tap_head_before_latest=' "$controller" | tail -n 1 | cut -d: -f1)"
tap_fence_after_line="$(grep -n 'tap_head_after_latest=' "$controller" | tail -n 1 | cut -d: -f1)"
[ -n "$latest_monotonic_line" ] && [ -n "$latest_current_line" ] &&
    [ -n "$verified_tap_line" ] && [ -n "$tap_fence_before_line" ] &&
    [ -n "$tap_fence_after_line" ] &&
    [ "$verified_tap_line" -lt "$latest_monotonic_line" ] &&
    [ "$latest_monotonic_line" -lt "$latest_current_line" ] &&
    [ "$latest_current_line" -lt "$tap_fence_before_line" ] &&
    [ "$tap_fence_before_line" -lt "$latest_line" ] &&
    [ "$latest_line" -lt "$tap_fence_after_line" ] ||
    fail "latest mutation is not enclosed by monotonic stale-controller fences"
require_fixed 'contents/Formula/dbmd.rb?ref=${verified_tap_head}' "$controller"
require_fixed 'release_latest_preflight_action' "$controller"
require_fixed 'contents/Formula/dbmd.rb?ref=${repair_head}' "$controller"
require_fixed 'gh attestation verify "$repair_tarball"' "$controller"
require_fixed 'cmp "$repair_expected_formula" "$repair_formula"' "$controller"
require_fixed 'gh release edit "$repair_tag" --repo "$SOURCE_REPO" --latest' "$controller"
require_fixed 'die "tap advanced concurrently; latest was repaired to $repair_tag"' "$controller"

printf '%s\n' "release security tests passed"
