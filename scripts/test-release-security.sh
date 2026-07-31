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
publish_workflow="$repo_root/.github/workflows/publish-check.yml"
test_workflow="$repo_root/.github/workflows/test.yml"
controller="$repo_root/scripts/release.sh"
controller_lib="$repo_root/scripts/release-lib.sh"
crates_controller_lib="$repo_root/scripts/crates-release-lib.sh"
crates_publisher="$repo_root/scripts/publish-crates.sh"
publishability="$repo_root/scripts/check-publishability.sh"
cross_config="$repo_root/Cross.toml"
linkmd_source="$repo_root/crates/dbmd-core/src/linkmd.rs"
darwin_verifier="$repo_root/scripts/verify-darwin-toolchain.sh"

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

# Every main commit must build the two exact static Linux artifacts that ship.
# The release-only v0.8.7 cross build caught a musl API mismatch after tagging;
# this gate prevents a tag from being the first musl compilation.
require_fixed 'name: release target (${{ matrix.rust_target }})' "$test_workflow"
require_fixed 'x86_64-unknown-linux-musl' "$test_workflow"
require_fixed 'aarch64-unknown-linux-musl' "$test_workflow"
require_fixed \
    'cross build --release --locked --target ${{ matrix.rust_target }} -p dbmd-cli' \
    "$test_workflow"
require_fixed 'tool: cross@0.2.5' "$test_workflow"
require_fixed 'libc::syscall(' "$linkmd_source"
require_fixed 'libc::SYS_renameat2' "$linkmd_source"
reject_fixed 'libc::renameat2(' "$linkmd_source"

# Darwin CI and the trusted controller must link with the exact same Apple
# toolchain. Normalizing LC_BUILD_VERSION alone cannot erase SDK stub and
# linker differences from the executable.
require_fixed 'os: macos-26' "$workflow"
[ "$(grep -Fc 'os: macos-26' "$workflow")" -eq 2 ] ||
    fail "both Darwin release targets must use macos-26"
require_fixed 'release-darwin-targets:' "$test_workflow"
require_fixed 'x86_64-apple-darwin' "$test_workflow"
require_fixed 'aarch64-apple-darwin' "$test_workflow"
require_fixed '/Applications/Xcode_26.6.app/Contents/Developer' "$workflow"
require_fixed '/Applications/Xcode_26.6.app/Contents/Developer' "$test_workflow"
require_fixed 'sh scripts/verify-darwin-toolchain.sh "$DEVELOPER_DIR"' "$workflow"
require_fixed 'sh scripts/verify-darwin-toolchain.sh "$DEVELOPER_DIR"' "$test_workflow"
require_fixed 'Build shipped release target twice' "$test_workflow"
require_fixed 'cmp "$first" "$second"' "$test_workflow"
require_fixed 'CARGO_TARGET_DIR="$target_dir"' "$test_workflow"
require_fixed 'DBMD_RELEASE_DARWIN_DEVELOPER_DIR:-$(xcode-select -p)' "$controller"
require_fixed 'sh scripts/verify-darwin-toolchain.sh "$DARWIN_DEVELOPER_DIR"' "$controller"
require_fixed 'Xcode 26.6\nBuild version 17F113' "$darwin_verifier"
require_fixed 'xcrun --sdk macosx --show-sdk-version)" = 26.5' "$darwin_verifier"
require_fixed '7def90dd8829726686213a747fc5bff1583df933dae5edc55d755479e0bfe00a' "$darwin_verifier"
require_fixed '5897b275efd93b201b6df5832dd541262b3f20f290859ba78f2200a6a66ef38b' "$darwin_verifier"
require_fixed 'c87bf9bb62dc6a3c5d7faf5c5f8dabc94aba865161a3e08b9f1871150e938fe6' "$darwin_verifier"
require_fixed 'e49ffad64ad1cee722540fc5ecb00a230fd8071680682c60d9c851029d20e814' "$darwin_verifier"
require_fixed '229eb9d8027953d2aee0590f983eed587d52bdd1ebc21114a62ce693f77b03f1' "$darwin_verifier"
require_fixed 'f8d005f09381389167f9e0aeaa169bc9e7dff162ef22ca2fd8e98df7ff1acafe' "$darwin_verifier"
require_fixed '20cfce043f11a083e2eb6111efe3579919a8082fa4cc912a7bd839af2010ec57' "$darwin_verifier"
require_fixed 'f18a90790d05e826fbaad1892be4fe32270fd24cb73ac131049a55c7866a6d8e' "$darwin_verifier"
require_fixed 'find . -type f -print0' "$darwin_verifier"
require_fixed 'LC_ALL=C sort -z' "$darwin_verifier"
require_fixed 'xargs -0 shasum -a 256' "$darwin_verifier"
require_fixed '5600799d9ea652e4f6b1a1158d730344388ffa7e3bba32f532beb5011ebcf129' "$darwin_verifier"
require_fixed 'expected %s, actual %s' "$darwin_verifier"
require_fixed 'rejected %s mismatched release input(s)' "$darwin_verifier"
reject_fixed 'os: macos-15-intel' "$workflow"
reject_fixed 'os: macos-14' "$workflow"

# A mismatched toolchain remains fail-closed while reporting every public
# fingerprint needed to diagnose runner-image drift. The diagnostic must not
# stop at the first mismatch, which previously hid whether the linker and SDK
# were also different.
darwin_mock="$(mktemp -d)"
cleanup_darwin_mock() {
    rm -rf -- "$darwin_mock"
}
trap cleanup_darwin_mock EXIT HUP INT TERM
mkdir -p \
    "$darwin_mock/bin" \
    "$darwin_mock/sdk/usr/lib" \
    "$darwin_mock/sdk/System/Library/Frameworks/CoreFoundation.framework"
for tool in clang ld vtool ar ranlib; do
    printf 'mismatched-%s\n' "$tool" >"$darwin_mock/$tool"
done
printf 'mismatched SDK settings\n' >"$darwin_mock/sdk/SDKSettings.json"
printf 'mismatched libSystem\n' >"$darwin_mock/sdk/usr/lib/libSystem.tbd"
printf 'mismatched CoreFoundation\n' \
    >"$darwin_mock/sdk/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation.tbd"
cat >"$darwin_mock/bin/uname" <<'EOF'
#!/bin/sh
printf 'arm64\n'
EOF
cat >"$darwin_mock/bin/xcodebuild" <<'EOF'
#!/bin/sh
printf 'Xcode 26.6\nBuild version 17F113\n'
EOF
cat >"$darwin_mock/bin/xcrun" <<'EOF'
#!/bin/sh
case "$*" in
    "--sdk macosx --show-sdk-version")
        printf '26.5\n'
        ;;
    "--sdk macosx --show-sdk-path")
        printf '%s\n' "$DBMD_DARWIN_MOCK_ROOT/sdk"
        ;;
    "-f clang"|"-f ld"|"-f vtool"|"-f ar"|"-f ranlib")
        printf '%s/%s\n' "$DBMD_DARWIN_MOCK_ROOT" "$2"
        ;;
    *)
        exit 1
        ;;
esac
EOF
chmod 700 \
    "$darwin_mock/bin/uname" \
    "$darwin_mock/bin/xcodebuild" \
    "$darwin_mock/bin/xcrun"
darwin_diagnostic="$darwin_mock/diagnostic"
if DBMD_DARWIN_MOCK_ROOT="$darwin_mock" \
    PATH="$darwin_mock/bin:/usr/bin:/bin" \
    sh "$darwin_verifier" "$darwin_mock/developer" \
    >"$darwin_mock/stdout" 2>"$darwin_diagnostic"; then
    fail "mismatched Darwin release inputs were accepted"
fi
[ "$(grep -c 'digest mismatch for ' "$darwin_diagnostic")" -eq 8 ] ||
    fail "Darwin verifier did not report all eight file mismatches"
require_fixed 'full SDK manifest mismatch (expected ' "$darwin_diagnostic"
require_fixed 'rejected 9 mismatched release input(s)' "$darwin_diagnostic"
reject_fixed 'Darwin release toolchain verified.' "$darwin_mock/stdout"
cleanup_darwin_mock
trap - EXIT HUP INT TERM

build_job="$(
    sed -n '/^  build:$/,/^  [a-zA-Z0-9_-]*:$/p' "$workflow"
)"
printf '%s\n' "$build_job" | grep -Fq 'Swatinem/rust-cache' &&
    fail "official release builders must start from fresh target directories"

# Version-specific dtolnay action commits encode the toolchain in action.yml;
# a `toolchain:` input is ignored and creates a false claim about the compiler.
rust_188_action='dtolnay/rust-toolchain@4e529fb27e59237866a6523e61ab248308c068b4'
require_fixed "$rust_188_action" "$workflow"
require_fixed "$rust_188_action" "$publish_workflow"
require_fixed "$rust_188_action" "$test_workflow"
reject_fixed 'toolchain:' "$workflow"
reject_fixed 'toolchain:' "$publish_workflow"
require_fixed 'CARGO_TARGET_DIR="${RUNNER_TEMP}/dbmd-clippy-release"' "$workflow"
require_fixed 'CARGO_TARGET_DIR="${RUNNER_TEMP}/dbmd-clippy-msrv"' "$test_workflow"
require_fixed 'CARGO_TARGET_DIR="${RUNNER_TEMP}/dbmd-clippy-stable"' "$test_workflow"

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
require_fixed 'target_dir="$rebuilt_dir/$rust_target"' "$controller"
require_fixed 'CARGO_TARGET_DIR="$target_dir"' "$controller"
require_fixed '"$rebuilt_dir/$rust_target/$rust_target/release/dbmd"' "$controller"
[ "$(grep -Fc '"$rebuilt_dir/$rust_target/$rust_target/release/dbmd"' "$controller")" -eq 2 ] ||
    fail "both CI-artifact and immutable-release comparisons must use the isolated rebuild path"
reject_fixed 'CARGO_TARGET_DIR="$rebuilt_dir"' "$controller"
require_fixed 'pending_deployments' "$controller"
require_fixed 'state: "approved"' "$controller"
reject_fixed 'state: "rejected"' "$controller"
reject_fixed 'release_pending_cleanup_action' "$controller"
require_fixed 'release_approval_state_matches' "$controller"
require_fixed '--signer-workflow "${SOURCE_REPO}/.github/workflows/${RELEASE_WORKFLOW}"' "$controller"
require_fixed '--source-digest "$source_sha"' "$controller"
require_fixed '--source-ref "refs/tags/${tag}"' "$controller"
require_fixed '--deny-self-hosted-runners' "$controller"

compare_line="$(grep -n 'rebuild_and_compare$' "$controller" | head -n 1 | cut -d: -f1)"
artifact_state_line="$(grep -n 'release_artifact_state ' "$controller" | head -n 1 | cut -d: -f1)"
approve_line="$(grep -n 'state: "approved"' "$controller" | head -n 1 | cut -d: -f1)"
approval_revalidate_line="$(
    grep -n 'release_approval_state_matches' "$controller" |
        head -n 1 |
        cut -d: -f1
)"
[ -n "$artifact_state_line" ] && [ -n "$compare_line" ] &&
    [ "$artifact_state_line" -lt "$compare_line" ] ||
    fail "failed workflow state must be rejected before artifact download"
[ -n "$compare_line" ] && [ -n "$approve_line" ] &&
    [ "$compare_line" -lt "$approve_line" ] ||
    fail "publishing approval is not ordered after independent rebuild comparison"
[ -n "$approval_revalidate_line" ] &&
    [ "$compare_line" -lt "$approval_revalidate_line" ] &&
    [ "$approval_revalidate_line" -lt "$approve_line" ] ||
    fail "approval state must be revalidated after rebuild and before approval"

# The rebuild call is outside the pending-approval branch, so fresh,
# manually-approved, completed/resumed, and rerun states cannot bypass it.
pending_branch_line="$(grep -n '^if \[ -n "\$pending_record" \]; then$' "$controller" | tail -n 1 | cut -d: -f1)"
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
