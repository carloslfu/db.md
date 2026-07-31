#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Package each crate exactly as crates.io will receive it, then compile the
# extracted tarballs against each other. `cargo package --workspace` alone is
# not a pre-publication proof when one workspace crate depends on a new,
# unpublished version of another: Cargo may verify the CLI against the latest
# registry version instead of the just-packaged core.

set -eu
unset CDPATH

repo_root="$(cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$repo_root"

version="$(
    sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml |
        head -n 1
)"
test -n "$version"

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
    cargo package -p dbmd-cli --locked --no-verify "$@"

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

# Patch crates.io only for this extracted-tarball verification. The published
# CLI keeps its normal registry dependency; this proves its exact source against
# the exact core bytes that the same release will publish first.
CARGO_TARGET_DIR="$scratch/cli-target" \
    cargo \
        --config "patch.crates-io.dbmd-core.path='${core}'" \
        update \
        --manifest-path "$cli/Cargo.toml" \
        -p dbmd-core \
        --precise "$version"
CARGO_TARGET_DIR="$scratch/cli-target" \
    cargo \
        --config "patch.crates-io.dbmd-core.path='${core}'" \
        check \
        --manifest-path "$cli/Cargo.toml" \
        --all-targets \
        --all-features \
        --locked

printf 'packaged dbmd-core and dbmd-cli tarballs compile together at %s\n' "$version"
