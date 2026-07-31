#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Resumable official-release controller. CI builds and attests draft artifacts;
# this trusted local controller independently rebuilds every target byte-for-byte
# before approving the protected publishing environment. After crates.io and the
# immutable GitHub release converge, it updates Homebrew with the caller's
# existing `gh` authority through the optimistic Contents API, then and only then
# promotes the GitHub release to latest. No deploy key or CI write secret exists.

set -euo pipefail

# shellcheck source=scripts/release-lib.sh
source "$(cd -- "$(dirname -- "$0")" && pwd)/release-lib.sh"

SOURCE_REPO="${DBMD_SOURCE_REPO:-carloslfu/db.md}"
TAP_REPO="${DBMD_TAP_REPO:-carloslfu/homebrew-tap}"
RELEASE_WORKFLOW="${DBMD_RELEASE_WORKFLOW:-release.yml}"
RELEASE_ENV="${DBMD_RELEASE_ENV:-release-publishing}"
RUST_TOOLCHAIN="${DBMD_RELEASE_RUST_TOOLCHAIN:-1.88.0}"

die() {
    printf 'release: %s\n' "$*" >&2
    exit 1
}

for command_name in git gh jq shasum cargo rustup cross tar xcrun cmp curl openssl; do
    command -v "$command_name" >/dev/null 2>&1 ||
        die "required command not found: $command_name"
done

version="${1:-}"
printf '%s\n' "$version" |
    grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$' ||
    die "usage: scripts/release.sh X.Y.Z"
tag="v${version}"

test "$(git branch --show-current)" = "main" ||
    die "official releases must be cut from main"
test -z "$(git status --porcelain)" ||
    die "worktree must be clean before a release"

git fetch --no-tags origin '+refs/heads/main:refs/remotes/origin/main'
source_sha="$(git rev-parse HEAD)"
test "$source_sha" = "$(git rev-parse origin/main)" ||
    die "local main must exactly match origin/main"
cargo_version="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
test "$cargo_version" = "$version" ||
    die "Cargo.toml version $cargo_version does not match requested $version"
gh auth status >/dev/null

release_tmp="$(mktemp -d "${TMPDIR:-/tmp}/dbmd-release.XXXXXXXX")"
chmod 700 "$release_tmp"
cleanup() {
    rm -rf "$release_tmp"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

release_source="$release_tmp/source"
mkdir -p "$release_source"
# Freeze the reviewed commit into an isolated archive immediately. The
# controller may wait on external workflow/approval gates; no later edit in the
# caller's working tree can enter the independent rebuild or trusted notices.
# `git archive` is read-only and leaves no linked-worktree metadata to clean up.
git archive --format=tar "$source_sha" | tar -xf - -C "$release_source"

# Immutable release enforcement is repository state, not workflow convention.
if ! gh api \
    -H 'Accept: application/vnd.github+json' \
    -H 'X-GitHub-Api-Version: 2026-03-10' \
    "repos/${SOURCE_REPO}/immutable-releases" >/dev/null 2>&1; then
    gh api --method PUT \
        -H 'Accept: application/vnd.github+json' \
        -H 'X-GitHub-Api-Version: 2026-03-10' \
        "repos/${SOURCE_REPO}/immutable-releases" >/dev/null
fi
test "$(
    gh api \
        -H 'Accept: application/vnd.github+json' \
        -H 'X-GitHub-Api-Version: 2026-03-10' \
        "repos/${SOURCE_REPO}/immutable-releases" --jq .enabled
)" = true || die "immutable releases are not enabled on $SOURCE_REPO"

# Make the publishing environment an explicit local-controller approval gate.
# Re-applying this exact policy is idempotent and removes any dependence on an
# environment secret that can survive a killed controller.
reviewer_id="$(gh api user --jq .id)"
jq -n \
    --argjson reviewer_id "$reviewer_id" \
    '{
      wait_timer: 0,
      prevent_self_review: false,
      reviewers: [{type: "User", id: $reviewer_id}],
      deployment_branch_policy: null
    }' |
    gh api --method PUT \
        "repos/${SOURCE_REPO}/environments/${RELEASE_ENV}" \
        --input - >/dev/null

remote_tag_sha="$(
    git ls-remote origin "refs/tags/${tag}^{}" |
        awk 'NR == 1 { print $1 }'
)"
if [ -z "$remote_tag_sha" ]; then
    remote_tag_sha="$(
        git ls-remote origin "refs/tags/${tag}" |
            awk 'NR == 1 { print $1 }'
    )"
fi
if [ -n "$remote_tag_sha" ]; then
    test "$remote_tag_sha" = "$source_sha" ||
        die "remote tag $tag names $remote_tag_sha, expected $source_sha"
else
    if git rev-parse --verify --quiet "refs/tags/${tag}" >/dev/null; then
        test "$(git rev-list -n1 "$tag")" = "$source_sha" ||
            die "local tag $tag does not name the release source"
    else
        git tag "$tag" "$source_sha"
    fi
    git push origin "refs/tags/${tag}"
fi

find_run() {
    gh api "repos/${SOURCE_REPO}/actions/runs?event=push&head_sha=${source_sha}" \
        --jq ".workflow_runs[] |
              select(.path == \".github/workflows/${RELEASE_WORKFLOW}\") |
              select(.head_branch == \"${tag}\") |
              [.id, .run_attempt, .status, (.conclusion // \"\")] | @tsv" |
        head -n 1
}

run_record=""
for _ in $(seq 1 180); do
    run_record="$(find_run)"
    [ -n "$run_record" ] && break
    sleep 2
done
[ -n "$run_record" ] ||
    die "release workflow run did not appear for $tag at $source_sha"
IFS="$(printf '\t')" read -r run_id run_attempt run_status run_conclusion <<EOF
$run_record
EOF

if [ "$run_status" = completed ] && [ "$run_conclusion" != success ]; then
    printf 'Re-running failed release jobs for resumable convergence.\n'
    gh run rerun "$run_id" --failed --repo "$SOURCE_REPO"
    previous_attempt="$run_attempt"
    for _ in $(seq 1 180); do
        run_attempt="$(
            gh api "repos/${SOURCE_REPO}/actions/runs/${run_id}" --jq .run_attempt
        )"
        [ "$run_attempt" -gt "$previous_attempt" ] && break
        sleep 2
    done
    [ "$run_attempt" -gt "$previous_attempt" ] ||
        die "release workflow did not start a new attempt"
fi

wait_for_pending_approval() {
    for _ in $(seq 1 900); do
        pending="$(
            gh api \
                "repos/${SOURCE_REPO}/actions/runs/${run_id}/pending_deployments" \
                --jq ".[] | select(.environment.name == \"${RELEASE_ENV}\") |
                      [.environment.id, .current_user_can_approve] | @tsv"
        )"
        if [ -n "$pending" ]; then
            printf '%s\n' "$pending"
            return 0
        fi
        status="$(
            gh api "repos/${SOURCE_REPO}/actions/runs/${run_id}" --jq .status
        )"
        if [ "$status" = completed ]; then
            return 1
        fi
        sleep 2
    done
    return 1
}

rebuild_and_compare() {
    ci_dir="$release_tmp/ci"
    rebuilt_dir="$release_tmp/rebuilt-target"
    mkdir -p "$ci_dir" "$rebuilt_dir"

    for target_name in \
        darwin-x86_64 \
        darwin-aarch64 \
        linux-x86_64-musl \
        linux-aarch64-musl; do
        mkdir -p "$ci_dir/$target_name"
        gh run download "$run_id" \
            --repo "$SOURCE_REPO" \
            --name "tarball-${target_name}" \
            --dir "$ci_dir/$target_name"
    done

    (
        cd "$release_source"
        rustup target add \
            --toolchain "$RUST_TOOLCHAIN" \
            x86_64-apple-darwin aarch64-apple-darwin >/dev/null

        for rust_target in x86_64-apple-darwin aarch64-apple-darwin; do
            CARGO_TARGET_DIR="$rebuilt_dir" \
            MACOSX_DEPLOYMENT_TARGET=11.0 \
            RUSTFLAGS='-C link-arg=-Wl,-no_uuid' \
                cargo "+${RUST_TOOLCHAIN}" build \
                    --release --locked --target "$rust_target" -p dbmd-cli
            binary="$rebuilt_dir/$rust_target/release/dbmd"
            xcrun vtool -set-build-version macos 11.0 11.0 \
                -replace -output "${binary}.normalized" "$binary"
            mv "${binary}.normalized" "$binary"
            chmod +x "$binary"
        done

        for rust_target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
            CARGO_TARGET_DIR="$rebuilt_dir" \
            RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN" \
                cross build --release --locked --target "$rust_target" -p dbmd-cli
        done
    )

    compare_target() {
        target_name="$1"
        rust_target="$2"
        artifact_dir="$ci_dir/$target_name"
        tarball="$artifact_dir/dbmd-${version}-${target_name}.tar.gz"
        compare_immutable_target \
            "$rebuilt_dir/$rust_target/release/dbmd" \
            "$tarball" "$version" "$target_name" \
            "$release_source/NOTICE" \
            "$release_source/THIRD_PARTY_NOTICES" \
            "$release_source/LICENSE" \
            "$artifact_dir/compared" ||
            die "independent rebuild or archive shape differs for $target_name"
    }

    compare_target darwin-x86_64 x86_64-apple-darwin
    compare_target darwin-aarch64 aarch64-apple-darwin
    compare_target linux-x86_64-musl x86_64-unknown-linux-musl
    compare_target linux-aarch64-musl aarch64-unknown-linux-musl
    printf 'Independent rebuild matched all four CI binaries byte-for-byte.\n'
}

pending_record="$(wait_for_pending_approval || true)"
# This comparison is unconditional. A fresh pending approval, a manually
# approved run, a completed/resumed run, and a rerun all pass through the same
# independent four-target rebuild before any permanent-channel convergence.
rebuild_and_compare
if [ -n "$pending_record" ]; then
    IFS="$(printf '\t')" read -r environment_id can_approve <<EOF
$pending_record
EOF
    test "$can_approve" = true ||
        die "current gh identity cannot approve $RELEASE_ENV"
    jq -n \
        --argjson environment_id "$environment_id" \
        '{
          environment_ids: [$environment_id],
          state: "approved",
          comment: "Independent four-target byte-for-byte rebuild verified"
        }' |
        gh api --method POST \
            "repos/${SOURCE_REPO}/actions/runs/${run_id}/pending_deployments" \
            --input - >/dev/null
    printf 'Approved publishing for workflow %s.%s.\n' "$run_id" "$run_attempt"
else
    # A completed run is resumable: re-verify published bytes below. A running
    # run without a pending deployment is either already approved or malformed.
    status="$(gh api "repos/${SOURCE_REPO}/actions/runs/${run_id}" --jq .status)"
    [ "$(release_resume_action "$pending_record" "$status")" = resume ] ||
        die "release run has no protected publishing approval to review"
fi

conclusion=""
for _ in $(seq 1 900); do
    run_json="$(gh api "repos/${SOURCE_REPO}/actions/runs/${run_id}")"
    status="$(printf '%s' "$run_json" | jq -r .status)"
    if [ "$status" = completed ]; then
        conclusion="$(printf '%s' "$run_json" | jq -r .conclusion)"
        break
    fi
    sleep 2
done
test "$conclusion" = success ||
    die "release workflow $run_id concluded: ${conclusion:-not completed}"

release_json="$(
    gh api \
        -H 'Accept: application/vnd.github+json' \
        -H 'X-GitHub-Api-Version: 2026-03-10' \
        "repos/${SOURCE_REPO}/releases/tags/${tag}"
)"
test "$(printf '%s' "$release_json" | jq -r .immutable)" = true ||
    die "published release $tag is not immutable"
test "$(gh api "repos/${SOURCE_REPO}/commits/${tag}" --jq .sha)" = "$source_sha" ||
    die "published tag no longer resolves to the reviewed source commit"

actual_assets="$(
    printf '%s' "$release_json" |
        jq -r '.assets[].name' |
        LC_ALL=C sort
)"
expected_assets="$(
    printf '%s\n' \
        SHA256SUMS \
        "dbmd-${version}-darwin-aarch64.tar.gz" \
        "dbmd-${version}-darwin-x86_64.tar.gz" \
        "dbmd-${version}-linux-aarch64-musl.tar.gz" \
        "dbmd-${version}-linux-x86_64-musl.tar.gz" |
        LC_ALL=C sort
)"
test "$actual_assets" = "$expected_assets" ||
    die "immutable release asset set does not match the four reviewed targets"

verify_dir="$release_tmp/verify"
mkdir -p "$verify_dir"
gh release download "$tag" --repo "$SOURCE_REPO" --dir "$verify_dir"
(
    cd "$verify_dir"
    shasum -a 256 -c SHA256SUMS
    for tarball in dbmd-*.tar.gz; do
        gh attestation verify "$tarball" \
            --repo "$SOURCE_REPO" \
            --signer-workflow "${SOURCE_REPO}/.github/workflows/${RELEASE_WORKFLOW}" \
            --source-digest "$source_sha" \
            --source-ref "refs/tags/${tag}" \
            --deny-self-hosted-runners >/dev/null
    done
)

# CI artifacts are only an intermediate. Re-open the exact immutable release
# assets that Homebrew will name, extract them independently, and compare every
# final binary and bundled notice to the local rebuild before touching the tap
# or `latest`. This also protects completed/manual-approved resume paths where
# the controller did not perform the environment approval itself.
compare_final_target() {
    target_name="$1"
    rust_target="$2"
    compare_immutable_target \
        "$rebuilt_dir/$rust_target/release/dbmd" \
        "$verify_dir/dbmd-${version}-${target_name}.tar.gz" \
        "$version" "$target_name" \
        "$release_source/NOTICE" \
        "$release_source/THIRD_PARTY_NOTICES" \
        "$release_source/LICENSE" \
        "$verify_dir/extracted-${target_name}" ||
        die "immutable release contents differ from the independent rebuild for $target_name"
}

compare_final_target darwin-x86_64 x86_64-apple-darwin
compare_final_target darwin-aarch64 aarch64-apple-darwin
compare_final_target linux-x86_64-musl x86_64-unknown-linux-musl
compare_final_target linux-aarch64-musl aarch64-unknown-linux-musl
printf 'Independent rebuild matched all four immutable release binaries byte-for-byte.\n'

# Verify crates.io converged to the exact local package bytes, not merely the
# requested version label.
(
    cd "$release_source"
    CARGO_TARGET_DIR="$release_tmp/package-target" \
        cargo "+${RUST_TOOLCHAIN}" package --workspace --locked
)
for crate_name in dbmd-core dbmd-cli; do
    local_crate="$release_tmp/package-target/package/${crate_name}-${version}.crate"
    local_checksum="$(shasum -a 256 "$local_crate" | awk '{print $1}')"
    published_checksum="$(
        curl -fsS \
            -H 'User-Agent: db.md release controller' \
            "https://crates.io/api/v1/crates/${crate_name}/${version}" |
            jq -r .version.checksum
    )"
    test "$published_checksum" = "$local_checksum" ||
        die "crates.io checksum mismatch for ${crate_name} ${version}"
done

# Render and update the tap with the caller's existing GitHub authorization.
# The blob SHA is an optimistic concurrency token; exact bytes make retries a
# no-op. The returned commit must be a direct child of the head we reviewed.
formula="$release_tmp/dbmd.rb"
"$release_source/HomebrewFormula/render.sh" \
    "$version" "$verify_dir/SHA256SUMS" > "$formula"
tap_head="$(
    gh api "repos/${TAP_REPO}/git/ref/heads/main" --jq .object.sha
)"
tap_content="$(
    gh api "repos/${TAP_REPO}/contents/Formula/dbmd.rb?ref=main"
)"
tap_blob_sha="$(printf '%s' "$tap_content" | jq -r .sha)"
printf '%s' "$tap_content" | jq -r .content |
    tr -d '\n' |
    openssl base64 -d -A > "$release_tmp/current-dbmd.rb"

if ! cmp -s "$formula" "$release_tmp/current-dbmd.rb"; then
    formula_base64="$(
        openssl base64 -A < "$formula"
    )"
    update_result="$(
        jq -n \
            --arg message "dbmd: update to ${tag}" \
            --arg content "$formula_base64" \
            --arg sha "$tap_blob_sha" \
            '{message: $message, content: $content, sha: $sha, branch: "main"}' |
            gh api --method PUT \
                "repos/${TAP_REPO}/contents/Formula/dbmd.rb" \
                --input -
    )"
    tap_commit="$(printf '%s' "$update_result" | jq -r .commit.sha)"
    test -n "$tap_commit" && test "$tap_commit" != null ||
        die "tap Contents API did not return a commit"
    test "$(
        gh api "repos/${TAP_REPO}/commits/${tap_commit}" --jq '.parents[0].sha'
    )" = "$tap_head" ||
        die "tap changed concurrently; formula landed safely but rerun to reconverge"
    test "$(
        gh api "repos/${TAP_REPO}/git/ref/heads/main" --jq .object.sha
    )" = "$tap_commit" ||
        die "tap main advanced after formula update; rerun to verify convergence"
fi

gh api "repos/${TAP_REPO}/contents/Formula/dbmd.rb?ref=main" --jq .content |
    tr -d '\n' |
    openssl base64 -d -A > "$release_tmp/final-dbmd.rb"
cmp "$formula" "$release_tmp/final-dbmd.rb" ||
    die "tap formula does not exactly match the immutable release"

# Latest is the final convergence signal. Every permanent channel is already
# exact, so an interrupted rerun reaches this step idempotently.
gh release edit "$tag" --repo "$SOURCE_REPO" --latest
printf 'Release %s converged: independent rebuild, crates.io, immutable assets, attestations, Homebrew, latest.\n' \
    "$tag"
