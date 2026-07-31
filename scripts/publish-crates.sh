#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Resumable two-crate publisher. crates.io cannot atomically publish a library
# and its dependent CLI, so each permanent step is followed by authoritative
# checksum + yank-state convergence before the next step is allowed.

set -eu
unset CDPATH

repo_root="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$repo_root"

# shellcheck source=scripts/crates-release-lib.sh
. "$repo_root/scripts/crates-release-lib.sh"

die() {
    printf 'publish crates: %s\n' "$*" >&2
    exit 1
}

for command_name in cargo curl jq shasum tar awk grep mktemp rm sleep; do
    command -v "$command_name" >/dev/null 2>&1 ||
        die "required command not found: $command_name"
done

version="${1:-}"
printf '%s\n' "$version" |
    grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$' ||
    die "usage: scripts/publish-crates.sh X.Y.Z"

workspace_version="$(
    sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml |
        head -n 1
)"
[ "$workspace_version" = "$version" ] ||
    die "workspace version $workspace_version does not match $version"
grep -Fqx \
    "dbmd-core = { path = \"../dbmd-core\", version = \"=${version}\", features = [\"link\"] }" \
    crates/dbmd-cli/Cargo.toml ||
    die "dbmd-cli must require exact same-release dbmd-core =$version"

poll_attempts="${DBMD_CRATES_POLL_ATTEMPTS:-60}"
poll_delay="${DBMD_CRATES_POLL_DELAY_SECONDS:-2}"
case "$poll_attempts" in
    '' | *[!0-9]* | 0) die "DBMD_CRATES_POLL_ATTEMPTS must be a positive integer" ;;
esac
case "$poll_delay" in
    '' | *[!0-9]*) die "DBMD_CRATES_POLL_DELAY_SECONDS must be a non-negative integer" ;;
esac

scratch="$(mktemp -d "${TMPDIR:-/tmp}/dbmd-publish-crates.XXXXXXXX")"
cleanup() {
    rm -rf "$scratch"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

version_response="$scratch/version-response.json"

request_version() {
    crate_name="$1"
    if ! http_status="$(
        curl \
            --silent \
            --show-error \
            --location \
            --proto '=https' \
            --proto-redir '=https' \
            --output "$version_response" \
            --write-out '%{http_code}' \
            --connect-timeout 10 \
            --max-time 30 \
            -H 'User-Agent: db.md release CI' \
            "https://crates.io/api/v1/crates/$crate_name/$version"
    )"; then
        printf '%s\n' network-error
        return 0
    fi
    printf '%s\n' "$http_status"
}

require_response_exact() {
    crate_name="$1"
    expected_checksum="$2"
    state="$(crates_version_state "$version_response" "$expected_checksum" "$version")"
    case "$state" in
        exact) return 0 ;;
        wrong-version) die "$crate_name response did not bind requested version $version" ;;
        yanked) die "$crate_name $version exists but is yanked" ;;
        mismatch) die "$crate_name $version exists with a different checksum" ;;
        *) die "$crate_name $version returned a malformed crates.io response" ;;
    esac
}

registry_has_exact() {
    crate_name="$1"
    expected_checksum="$2"
    status="$(request_version "$crate_name")"
    case "$status" in
        200)
            require_response_exact "$crate_name" "$expected_checksum"
            return 0
            ;;
        404) return 1 ;;
        *) die "cannot establish pre-publish state for $crate_name $version (HTTP $status)" ;;
    esac
}

wait_for_exact() {
    crate_name="$1"
    expected_checksum="$2"
    attempt=1
    while [ "$attempt" -le "$poll_attempts" ]; do
        status="$(request_version "$crate_name")"
        case "$status" in
            200)
                require_response_exact "$crate_name" "$expected_checksum"
                return 0
                ;;
            404 | 429 | 5?? | network-error)
                ;;
            *)
                die "unexpected crates.io response for $crate_name $version (HTTP $status)"
                ;;
        esac
        if [ "$attempt" -lt "$poll_attempts" ]; then
            sleep "$poll_delay"
        fi
        attempt=$((attempt + 1))
    done
    die "$crate_name $version did not converge to its exact checksum"
}

publish_or_resume() {
    crate_name="$1"
    expected_checksum="$2"
    target_dir="$3"

    if registry_has_exact "$crate_name" "$expected_checksum"; then
        printf '%s %s already exists with the exact checksum; resuming.\n' \
            "$crate_name" "$version"
    else
        printf 'Publishing %s %s.\n' "$crate_name" "$version"
        CARGO_TARGET_DIR="$target_dir" \
            cargo publish -p "$crate_name" --locked
    fi
    wait_for_exact "$crate_name" "$expected_checksum"
    printf '%s %s converged to %s and is not yanked.\n' \
        "$crate_name" "$version" "$expected_checksum"
}

# Phase 1: establish the exact independently publishable core package, then
# publish or resume it and wait for authoritative registry convergence.
core_target="$scratch/core-package-target"
CARGO_TARGET_DIR="$core_target" \
    cargo package -p dbmd-core --locked
core_crate="$core_target/package/dbmd-core-${version}.crate"
[ -f "$core_crate" ] || die "dbmd-core package was not created"
core_checksum="$(shasum -a 256 "$core_crate" | awk '{print $1}')"
publish_or_resume dbmd-core "$core_checksum" "$core_target"

# Phase 2 starts only after crates.io serves the exact core. This plain,
# unpatched package operation creates and verifies the actual registry-bound
# CLI tarball. Its lock must bind the exact core version and checksum.
cli_target="$scratch/cli-package-target"
CARGO_TARGET_DIR="$cli_target" \
    cargo package -p dbmd-cli --locked
cli_crate="$cli_target/package/dbmd-cli-${version}.crate"
[ -f "$cli_crate" ] || die "dbmd-cli package was not created"

cli_unpacked="$scratch/cli-unpacked"
mkdir -p "$cli_unpacked"
tar -xzf "$cli_crate" -C "$cli_unpacked"
cli="$cli_unpacked/dbmd-cli-${version}"
[ -f "$cli/Cargo.toml" ] || die "packaged dbmd-cli Cargo.toml is missing"
[ -f "$cli/Cargo.lock" ] || die "packaged dbmd-cli Cargo.lock is missing"

core_lock_block="$(
    sed -n '/^name = "dbmd-core"$/,/^$/p' "$cli/Cargo.lock"
)"
printf '%s\n' "$core_lock_block" |
    grep -Fqx "version = \"${version}\"" ||
    die "packaged dbmd-cli lock does not bind dbmd-core $version"
printf '%s\n' "$core_lock_block" |
    grep -Fqx 'source = "registry+https://github.com/rust-lang/crates.io-index"' ||
    die "packaged dbmd-cli lock is not registry-bound"
printf '%s\n' "$core_lock_block" |
    grep -Fqx "checksum = \"${core_checksum}\"" ||
    die "packaged dbmd-cli lock does not bind the published core checksum"

cli_lock_before="$(shasum -a 256 "$cli/Cargo.lock" | awk '{print $1}')"
CARGO_TARGET_DIR="$scratch/cli-exact-check-target" \
    cargo check \
        --manifest-path "$cli/Cargo.toml" \
        --all-targets \
        --all-features \
        --locked
cli_lock_after="$(shasum -a 256 "$cli/Cargo.lock" | awk '{print $1}')"
[ "$cli_lock_after" = "$cli_lock_before" ] ||
    die "registry-bound dbmd-cli Cargo.lock changed during exact verification"

cli_checksum="$(shasum -a 256 "$cli_crate" | awk '{print $1}')"
publish_or_resume dbmd-cli "$cli_checksum" "$cli_target"

printf 'Both crates converged exactly at %s.\n' "$version"
