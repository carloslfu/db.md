#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Pure classification for the crates.io version response used by the release
# publisher. Kept separate so exact/mismatch/yanked/malformed states have
# hermetic regression coverage without network or publishing authority.

crates_version_state() {
    response_file="$1"
    expected_checksum="$2"
    expected_version="$3"

    actual_version="$(
        jq -er '.version.num | select(type == "string")' \
            "$response_file" 2>/dev/null
    )" || {
        printf '%s\n' invalid
        return 0
    }
    actual_checksum="$(
        jq -er \
            '.version.checksum |
             select(type == "string" and test("^[0-9a-f]{64}$"))' \
            "$response_file" 2>/dev/null
    )" || {
        printf '%s\n' invalid
        return 0
    }
    yanked="$(
        jq -er \
            'if (.version.yanked | type) == "boolean"
             then (.version.yanked | tostring)
             else empty
             end' \
            "$response_file" 2>/dev/null
    )" || {
        printf '%s\n' invalid
        return 0
    }

    if [ "$actual_version" != "$expected_version" ]; then
        printf '%s\n' wrong-version
    elif [ "$yanked" = true ]; then
        printf '%s\n' yanked
    elif [ "$actual_checksum" = "$expected_checksum" ]; then
        printf '%s\n' exact
    else
        printf '%s\n' mismatch
    fi
}
