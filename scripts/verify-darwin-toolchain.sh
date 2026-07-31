#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Verify the complete Apple linker boundary used for reproducible release
# builds. Marketing version/build alone is insufficient: SDK stubs and linker
# bytes materially affect the Mach-O executable.

set -eu

developer_dir="${1:-}"
[ -n "$developer_dir" ] || {
    printf 'darwin toolchain verifier: developer directory is required\n' >&2
    exit 1
}
DEVELOPER_DIR="$developer_dir"
export DEVELOPER_DIR

[ "$(uname -m)" = arm64 ] || {
    printf 'darwin toolchain verifier: arm64 host required\n' >&2
    exit 1
}
[ "$(xcodebuild -version)" = "$(printf 'Xcode 26.6\nBuild version 17F113')" ] || {
    printf 'darwin toolchain verifier: Xcode 26.6 build 17F113 required\n' >&2
    exit 1
}
[ "$(xcrun --sdk macosx --show-sdk-version)" = 26.5 ] || {
    printf 'darwin toolchain verifier: macOS 26.5 SDK required\n' >&2
    exit 1
}

sdk_path="$(xcrun --sdk macosx --show-sdk-path)"

verify_sha256() {
    file="$1"
    expected="$2"
    actual="$(shasum -a 256 "$file" | awk '{print $1}')"
    [ "$actual" = "$expected" ] || {
        printf 'darwin toolchain verifier: digest mismatch for %s\n' "$file" >&2
        exit 1
    }
}

verify_sha256 \
    "$(xcrun -f clang)" \
    7def90dd8829726686213a747fc5bff1583df933dae5edc55d755479e0bfe00a
verify_sha256 \
    "$(xcrun -f ld)" \
    5897b275efd93b201b6df5832dd541262b3f20f290859ba78f2200a6a66ef38b
verify_sha256 \
    "$(xcrun -f vtool)" \
    c87bf9bb62dc6a3c5d7faf5c5f8dabc94aba865161a3e08b9f1871150e938fe6
verify_sha256 \
    "$(xcrun -f ar)" \
    e49ffad64ad1cee722540fc5ecb00a230fd8071680682c60d9c851029d20e814
verify_sha256 \
    "$(xcrun -f ranlib)" \
    229eb9d8027953d2aee0590f983eed587d52bdd1ebc21114a62ce693f77b03f1
verify_sha256 \
    "$sdk_path/SDKSettings.json" \
    f8d005f09381389167f9e0aeaa169bc9e7dff162ef22ca2fd8e98df7ff1acafe
verify_sha256 \
    "$sdk_path/usr/lib/libSystem.tbd" \
    20cfce043f11a083e2eb6111efe3579919a8082fa4cc912a7bd839af2010ec57
verify_sha256 \
    "$sdk_path/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation.tbd" \
    f18a90790d05e826fbaad1892be4fe32270fd24cb73ac131049a55c7866a6d8e

sdk_manifest="$(
    (
        cd "$sdk_path"
        find . -type f -print0 |
            LC_ALL=C sort -z |
            xargs -0 shasum -a 256
    ) |
        shasum -a 256 |
        awk '{print $1}'
)"
[ "$sdk_manifest" = 5600799d9ea652e4f6b1a1158d730344388ffa7e3bba32f532beb5011ebcf129 ] || {
    printf 'darwin toolchain verifier: full SDK manifest mismatch\n' >&2
    exit 1
}

printf 'Darwin release toolchain verified.\n'
