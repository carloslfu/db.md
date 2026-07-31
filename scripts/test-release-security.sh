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
sh "$repo_root/scripts/test-release-state.sh"

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
reject_fixed 'HOMEBREW_TAP_DEPLOY_KEY' "$workflow"
reject_fixed 'DBMD_RELEASE_AUTH' "$workflow"
reject_fixed 'homebrew-publishing' "$workflow"
reject_fixed 'gh release edit' "$workflow"

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

# Homebrew uses the authenticated Contents API with an optimistic blob SHA.
# There is no generated key, standing tap secret, or SSH transport.
require_fixed 'repos/${TAP_REPO}/contents/Formula/dbmd.rb' "$controller"
require_fixed '--arg sha "$tap_blob_sha"' "$controller"
reject_fixed 'HOMEBREW_TAP_DEPLOY_KEY' "$controller"
reject_fixed 'DBMD_RELEASE_AUTH' "$controller"
reject_fixed 'ssh-keygen' "$controller"
reject_fixed 'git@github.com' "$controller"
reject_fixed 'gh secret' "$controller"

# `latest` is the final mutation, after exact crates.io, release, attestation,
# and Homebrew verification.
latest_line="$(grep -n 'gh release edit "$tag".*--latest' "$controller" | tail -n 1 | cut -d: -f1)"
formula_verify_line="$(grep -n 'tap formula does not exactly match' "$controller" | tail -n 1 | cut -d: -f1)"
[ -n "$latest_line" ] && [ -n "$formula_verify_line" ] &&
    [ "$latest_line" -gt "$formula_verify_line" ] ||
    fail "latest promotion must occur after exact Homebrew convergence"

printf '%s\n' "release security tests passed"
