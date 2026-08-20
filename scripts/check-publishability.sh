#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Package both crates from the exact source tree, then compile the untouched
# extracted package sources against each other. Before dbmd-core is published,
# the CLI package necessarily carries a path-resolved lock entry rather than
# the eventual crates.io source/checksum. The release workflow separately
# performs the registry-bound CLI package proof after core converges.

set -eu
unset CDPATH

repo_root="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$repo_root"

version="$(
    sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml |
        head -n 1
)"
test -n "$version"
grep -Fqx \
    "dbmd-core = { path = \"../dbmd-core\", version = \"=${version}\", features = [\"link\"] }" \
    crates/dbmd-cli/Cargo.toml || {
    printf 'dbmd-cli must require the exact same-release dbmd-core =%s\n' "$version" >&2
    exit 1
}

# Unit tests in the published dbmd-core crate consume package-local copies of
# the public cross-implementation vectors. Refuse a source release if those
# copies drift from the repository-level conformance fixtures.
for vector in \
    linkmd-v2-commit-bridge.json \
    linkmd-v2-content-tree.json \
    linkmd-v2-portable-paths.json
do
    cmp "tests/vectors/$vector" "crates/dbmd-core/tests/vectors/$vector" || {
        printf 'packaged conformance vector drifted: %s\n' "$vector" >&2
        exit 1
    }
done

scratch="$(mktemp -d "${TMPDIR:-/tmp}/dbmd-publish-check.XXXXXXXX")"
cleanup() {
    rm -rf "$scratch"
}
trap cleanup EXIT HUP INT TERM

set --
if [ "${DBMD_PUBLISH_ALLOW_DIRTY:-0}" = 1 ]; then
    set -- --allow-dirty
fi

package_target="$scratch/package-target"
CARGO_TARGET_DIR="$package_target" \
    cargo package -p dbmd-core --locked --no-verify "$@"
CARGO_TARGET_DIR="$package_target" \
    cargo \
        --config "patch.crates-io.dbmd-core.path='${repo_root}/crates/dbmd-core'" \
        package \
        -p dbmd-cli \
        --locked \
        --no-verify \
        "$@"

unpacked="$scratch/unpacked"
mkdir -p "$unpacked"
tar -xzf "$package_target/package/dbmd-core-${version}.crate" -C "$unpacked"
tar -xzf "$package_target/package/dbmd-cli-${version}.crate" -C "$unpacked"

core="$unpacked/dbmd-core-${version}"
cli="$unpacked/dbmd-cli-${version}"
test -f "$core/Cargo.toml"
test -f "$cli/Cargo.toml"

CARGO_TARGET_DIR="$scratch/core-target" \
    cargo check \
        --manifest-path "$core/Cargo.toml" \
        --all-targets \
        --all-features \
        --locked

# Patch crates.io only for this pre-publication compatibility proof. Do not
# rewrite the extracted candidate's manifest or lock: the exact same-release
# core is not in the public index yet, and changing the candidate graph here
# would make this check capable of hiding a bad packaged dependency. The
# protected release job performs a second, registry-bound check after the exact
# core tarball is public and before it uploads the CLI.
cli_lock_before="$(shasum -a 256 "$cli/Cargo.lock" | awk '{print $1}')"
CARGO_TARGET_DIR="$scratch/cli-target" \
    cargo \
        --config "patch.crates-io.dbmd-core.path='${core}'" \
        check \
        --manifest-path "$cli/Cargo.toml" \
        --all-targets \
        --all-features \
        --locked
cli_lock_after="$(shasum -a 256 "$cli/Cargo.lock" | awk '{print $1}')"
test "$cli_lock_after" = "$cli_lock_before" || {
    printf 'packaged dbmd-cli Cargo.lock changed during verification\n' >&2
    exit 1
}

printf 'untouched dbmd-core and dbmd-cli package sources compile together at %s\n' "$version"
