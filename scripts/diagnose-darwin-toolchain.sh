#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Emit only public, content-derived diagnostics after the release verifier has
# already rejected a Darwin builder. This script never changes acceptance.
set -eu

developer_dir="${1:-}"
[ -n "$developer_dir" ] || {
    printf 'darwin toolchain diagnostics: developer directory is required\n' >&2
    exit 1
}
DEVELOPER_DIR="$developer_dir"
export DEVELOPER_DIR

diagnostic_dir="$(mktemp -d "${TMPDIR:-/tmp}/dbmd-darwin-diagnostic.XXXXXX")"
cleanup() {
    rm -rf -- "$diagnostic_dir"
}
trap cleanup EXIT HUP INT TERM

diagnose_tool() {
    tool_name="$1"
    tool_file="$(xcrun -f "$tool_name")"
    arm64_file="$diagnostic_dir/$tool_name"
    arches="$(lipo -archs "$tool_file")"

    printf 'darwin toolchain diagnostics: %s file: ' "$tool_name"
    file "$tool_file"
    printf 'darwin toolchain diagnostics: %s arches: %s\n' \
        "$tool_name" "$arches"
    printf 'darwin toolchain diagnostics: %s full sha256: ' "$tool_name"
    shasum -a 256 "$tool_file" | awk '{print $1}'
    codesign -dvvv "$tool_file" 2>&1 |
        sed -n \
            "s/^CDHash=/darwin toolchain diagnostics: $tool_name CDHash: /p;
             s/^TeamIdentifier=/darwin toolchain diagnostics: $tool_name TeamIdentifier: /p"

    case " $arches " in
        *" arm64 "*)
            if [ "$arches" = arm64 ]; then
                cp -L "$tool_file" "$arm64_file"
            else
                lipo "$tool_file" -thin arm64 -output "$arm64_file"
            fi
            ;;
        *)
            printf 'darwin toolchain diagnostics: %s has no arm64 slice\n' \
                "$tool_name" >&2
            exit 1
            ;;
    esac
    chmod u+w "$arm64_file"
    printf 'darwin toolchain diagnostics: %s arm64-signed sha256: ' \
        "$tool_name"
    shasum -a 256 "$arm64_file" | awk '{print $1}'
    codesign --remove-signature "$arm64_file"
    printf 'darwin toolchain diagnostics: %s arm64-unsigned sha256: ' \
        "$tool_name"
    shasum -a 256 "$arm64_file" | awk '{print $1}'
}

for tool in clang ld vtool ar ranlib; do
    diagnose_tool "$tool"
done

sdk_path="$(xcrun --sdk macosx --show-sdk-path)"
regular_manifest="$diagnostic_dir/sdk-regular-files"
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
) >"$regular_manifest"
printf 'darwin toolchain diagnostics: canonical SDK regular files: %s\n' \
    "$(wc -l <"$regular_manifest" | tr -d ' ')"
printf 'darwin toolchain diagnostics: canonical SDK regular-file manifest sha256: '
shasum -a 256 "$regular_manifest" | awk '{print $1}'

symlink_manifest="$diagnostic_dir/sdk-symlinks"
(
    cd "$sdk_path"
    find . -type l -print |
        LC_ALL=C sort |
        while IFS= read -r link_name; do
            case "$link_name" in
                './System/Library/PrivateFrameworks/CoreOCModules.framework/CoreOCModules.tbd' | \
                './System/Library/PrivateFrameworks/CoreOCModules.framework/Versions/Current')
                    ;;
                *)
                    printf '%s\t%s\n' "$link_name" "$(readlink "$link_name")"
                    ;;
            esac
        done
) >"$symlink_manifest"
printf 'darwin toolchain diagnostics: canonical SDK symlinks: %s\n' \
    "$(wc -l <"$symlink_manifest" | tr -d ' ')"
printf 'darwin toolchain diagnostics: SDK symlink manifest sha256: '
shasum -a 256 "$symlink_manifest" | awk '{print $1}'
