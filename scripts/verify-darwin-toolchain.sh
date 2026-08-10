#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Verify the complete Apple native-build boundary used for reproducible release
# builds. Marketing version/build alone is insufficient: compiler tools,
# headers, SDK stubs, archives, objects, and symlink resolution all affect the
# Mach-O executable.

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
# The hosted builders carry the complete Xcode package while the trusted local
# controller carries Apple's smaller Command Line Tools package. Both expose
# the same signed native-build inputs, which are authenticated below. Pin the
# package identity too so a future Apple update cannot silently enter either
# lane before its bytes are reviewed.
toolchain_package=""
if [ "$(xcodebuild -version 2>/dev/null || true)" = "$(printf 'Xcode 26.6\nBuild version 17F113')" ]; then
    toolchain_package=xcode-26.6-17F113
elif [ "$(
    pkgutil --pkg-info com.apple.pkg.CLTools_Executables 2>/dev/null |
        sed -n 's/^version: //p'
)" = 26.6.0.0.1781586589 ]; then
    toolchain_package=clt-26.6.0.0.1781586589
else
    printf 'darwin toolchain verifier: Xcode 26.6 build 17F113 or CLT 26.6.0.0.1781586589 required\n' >&2
    exit 1
fi
[ "$(xcrun --sdk macosx --show-sdk-version)" = 26.5 ] || {
    printf 'darwin toolchain verifier: macOS 26.5 SDK required\n' >&2
    exit 1
}

sdk_path="$(xcrun --sdk macosx --show-sdk-path)"
mismatch_count=0
reviewed_package_shape=""
canonical_dir="$(mktemp -d "${TMPDIR:-/tmp}/dbmd-darwin-verify.XXXXXX")"
cleanup() {
    rm -rf -- "$canonical_dir"
}
trap cleanup EXIT HUP INT TERM

verify_sha256() {
    file="$1"
    expected="$2"
    actual="$(shasum -a 256 "$file" | awk '{print $1}')"
    if [ "$actual" != "$expected" ]; then
        printf 'darwin toolchain verifier: digest mismatch for %s (expected %s, actual %s)\n' \
            "$file" "$expected" "$actual" >&2
        mismatch_count=$((mismatch_count + 1))
    fi
}

verify_tool_arm64() {
    tool_name="$1"
    expected_sha256="$2"
    expected_cdhash="$3"
    tool_file="$(xcrun -f "$tool_name")"
    arm64_file="$canonical_dir/$tool_name"
    arches="$(lipo -archs "$tool_file")"

    # Apple distributes both arm64-thin and universal packages. Accept only
    # those reviewed shapes, authenticate the Apple signature and CDHash, then
    # remove the package-specific signature blob before comparing executable
    # bytes. Full Xcode and CLT have the same unsigned arm64 code but different
    # signature-envelope bytes; those envelopes cannot affect compilation.
    case "$arches" in
        arm64)
            package_shape=thin-arm64
            cp -L "$tool_file" "$arm64_file"
            ;;
        "x86_64 arm64")
            package_shape=universal-x86_64-arm64
            lipo "$tool_file" -thin arm64 -output "$arm64_file"
            ;;
        *)
            printf 'darwin toolchain verifier: unsupported architectures for %s: %s\n' \
                "$tool_name" "$arches" >&2
            mismatch_count=$((mismatch_count + 1))
            return
            ;;
    esac
    if [ -z "$reviewed_package_shape" ]; then
        reviewed_package_shape="$package_shape"
    elif [ "$reviewed_package_shape" != "$package_shape" ]; then
        printf 'darwin toolchain verifier: inconsistent package shape for %s (expected %s, actual %s)\n' \
            "$tool_name" "$reviewed_package_shape" "$package_shape" >&2
        mismatch_count=$((mismatch_count + 1))
    fi

    if ! codesign --verify --strict "$arm64_file" >/dev/null 2>&1; then
        printf 'darwin toolchain verifier: invalid Apple signature for %s arm64 slice\n' \
            "$tool_name" >&2
        mismatch_count=$((mismatch_count + 1))
    fi
    signature="$(
        codesign -dvvv "$arm64_file" 2>&1 || true
    )"
    team_identifier="$(
        printf '%s\n' "$signature" |
            sed -n 's/^TeamIdentifier=//p'
    )"
    cdhash="$(
        printf '%s\n' "$signature" |
            sed -n 's/^CDHash=//p'
    )"
    if [ "$team_identifier" != 59GAB85EFG ]; then
        printf 'darwin toolchain verifier: unexpected Apple team for %s: %s\n' \
            "$tool_name" "${team_identifier:-missing}" >&2
        mismatch_count=$((mismatch_count + 1))
    fi
    if [ "$cdhash" != "$expected_cdhash" ]; then
        printf 'darwin toolchain verifier: CDHash mismatch for %s (expected %s, actual %s)\n' \
            "$tool_name" "$expected_cdhash" "${cdhash:-missing}" >&2
        mismatch_count=$((mismatch_count + 1))
    fi
    chmod u+w "$arm64_file"
    codesign --remove-signature "$arm64_file"
    verify_sha256 "$arm64_file" "$expected_sha256"
}

verify_tool_arm64 \
    clang \
    5bbebcabb7dde1aade0a479ef3788ef65edbd975af444db345f3020f6be7c29c \
    20a014a92f165cfaf456ba661639d0535043c4b7
verify_tool_arm64 \
    ld \
    c671cedbd64871318c377e0a25c0725fcc84bca5c6cfd73a5ab8aa2a1118e2ad \
    08fef69d7476af67d62beb5a812153a22d356ffe
verify_tool_arm64 \
    vtool \
    d75943f54dedbdcd0b889222df3073ab43ef9785ce0116a2d222fd34d783317c \
    6654bac92b216bea76b6a9256460d95b5dbdf1ef
verify_tool_arm64 \
    ar \
    f1ab6041e05473409044c79dd37bc87eb8b26804dcf4ad3e80f9f87388ed7493 \
    03e32556e2d501e2807bdd24f94be27776a7f20b
verify_tool_arm64 \
    ranlib \
    0ce41502412f3f421fa62ad8cf1a8d7078890063566f1189cc23d690ac9796c5 \
    a2fb8aa7d6b66628c00f2d7c179dabf2639b31bf
verify_sha256 \
    "$sdk_path/SDKSettings.json" \
    f8d005f09381389167f9e0aeaa169bc9e7dff162ef22ca2fd8e98df7ff1acafe
verify_sha256 \
    "$sdk_path/usr/lib/libSystem.tbd" \
    20cfce043f11a083e2eb6111efe3579919a8082fa4cc912a7bd839af2010ec57
verify_sha256 \
    "$sdk_path/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation.tbd" \
    f18a90790d05e826fbaad1892be4fe32270fd24cb73ac131049a55c7866a6d8e

# Full Xcode and CLT differ in exactly seven reviewed, non-build files: two
# embedded WebKit helper executables whose package signatures differ, one
# private-framework stub, and four Swift package interfaces absent from full
# Xcode. Exclude only those exact paths. The remaining canonical manifest is
# byte-identical across both trusted packages and still pins every C header,
# modulemap, public framework stub, archive, and object available to native
# dependency builds (including ring).
sdk_regular_manifest="$canonical_dir/sdk-regular-files"
(
    cd "$sdk_path"
    find . -type f \
        ! -path './System/Cryptexes/OS/System/iOSSupport/System/Library/Frameworks/SafariServices.framework/PlugIns/SafariServices.wkbundle/Contents/MacOS/SafariServices' \
        ! -path './System/Library/PrivateFrameworks/EmailCore.framework/Versions/A/PlugIns/EmailCore.wkbundle/Contents/MacOS/EmailCore' \
        ! -path './System/Library/PrivateFrameworks/CoreOCModules.framework/Versions/A/CoreOCModules.tbd' \
        ! -path './usr/lib/swift/XPC.swiftmodule/arm64e-apple-ios-macabi.package.swiftinterface' \
        ! -path './usr/lib/swift/XPC.swiftmodule/arm64e-apple-macos.package.swiftinterface' \
        ! -path './usr/lib/swift/XPC.swiftmodule/x86_64-apple-ios-macabi.package.swiftinterface' \
        ! -path './usr/lib/swift/XPC.swiftmodule/x86_64-apple-macos.package.swiftinterface' \
        -print0 |
        LC_ALL=C sort -z |
        xargs -0 shasum -a 256
) >"$sdk_regular_manifest"
sdk_regular_count="$(wc -l <"$sdk_regular_manifest" | tr -d ' ')"
sdk_regular_sha256="$(
    shasum -a 256 "$sdk_regular_manifest" |
        awk '{print $1}'
)"
expected_sdk_regular_count=32343
expected_sdk_regular_sha256=fc36805b79a681ab56883bd36b6c70abad259ac04043c3084bf3a67599dfa176
if [ "$sdk_regular_count" != "$expected_sdk_regular_count" ] ||
    [ "$sdk_regular_sha256" != "$expected_sdk_regular_sha256" ]; then
    printf 'darwin toolchain verifier: canonical SDK regular-file manifest mismatch for %s/%s (expected %s files/%s, actual %s files/%s)\n' \
        "$toolchain_package" "${reviewed_package_shape:-unknown}" \
        "$expected_sdk_regular_count" "$expected_sdk_regular_sha256" \
        "$sdk_regular_count" "$sdk_regular_sha256" >&2
    mismatch_count=$((mismatch_count + 1))
fi

# Regular-file hashes do not authenticate which byte object a framework/library
# alias resolves to. Pin every SDK symlink path + raw target, and reject any
# absolute, broken/cyclic, or SDK-escaping target before accepting the topology.
sdk_real_path="$(realpath "$sdk_path")"
sdk_symlink_manifest="$canonical_dir/sdk-symlinks"
if ! (
    cd "$sdk_path"
    find . -type l -print |
        LC_ALL=C sort |
        while IFS= read -r link_name; do
            link_target="$(readlink "$link_name")" || {
                printf 'darwin toolchain verifier: unreadable SDK symlink: %s\n' \
                    "$link_name" >&2
                exit 1
            }
            case "$link_target" in
                /*)
                    printf 'darwin toolchain verifier: absolute SDK symlink refused: %s -> %s\n' \
                        "$link_name" "$link_target" >&2
                    exit 1
                    ;;
            esac
            resolved_target="$(realpath "$link_name")" || {
                printf 'darwin toolchain verifier: broken or cyclic SDK symlink refused: %s -> %s\n' \
                    "$link_name" "$link_target" >&2
                exit 1
            }
            case "$resolved_target" in
                "$sdk_real_path" | "$sdk_real_path"/*) ;;
                *)
                    printf 'darwin toolchain verifier: SDK-escaping symlink refused: %s -> %s\n' \
                        "$link_name" "$link_target" >&2
                    exit 1
                    ;;
            esac
            case "$link_name" in
                './System/Library/PrivateFrameworks/CoreOCModules.framework/CoreOCModules.tbd' | \
                './System/Library/PrivateFrameworks/CoreOCModules.framework/Versions/Current')
                    # CLT-only aliases for the reviewed, unused private stub.
                    ;;
                *)
                    printf '%s\t%s\n' "$link_name" "$link_target"
                    ;;
            esac
        done
) >"$sdk_symlink_manifest"; then
    mismatch_count=$((mismatch_count + 1))
else
    sdk_symlink_count="$(wc -l <"$sdk_symlink_manifest" | tr -d ' ')"
    sdk_symlink_sha256="$(
        shasum -a 256 "$sdk_symlink_manifest" |
            awk '{print $1}'
    )"
    expected_sdk_symlink_count=7448
    expected_sdk_symlink_sha256=6f3445524cef60cf2d718453a1c3298432317de3148be1786892554f664b2100
    if [ "$sdk_symlink_count" != "$expected_sdk_symlink_count" ] ||
        [ "$sdk_symlink_sha256" != "$expected_sdk_symlink_sha256" ]; then
        printf 'darwin toolchain verifier: SDK symlink manifest mismatch (expected %s links/%s, actual %s links/%s)\n' \
            "$expected_sdk_symlink_count" "$expected_sdk_symlink_sha256" \
            "$sdk_symlink_count" "$sdk_symlink_sha256" >&2
        mismatch_count=$((mismatch_count + 1))
    fi
fi

if [ "$mismatch_count" -ne 0 ]; then
    printf 'darwin toolchain verifier: rejected %s mismatched release input(s)\n' \
        "$mismatch_count" >&2
    exit 1
fi

printf 'Darwin release toolchain verified.\n'
