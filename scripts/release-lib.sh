#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Pure, hermetically testable pieces of the trusted release controller.

release_resume_action() {
    pending_record="$1"
    run_status="$2"
    if [ -n "$pending_record" ]; then
        printf '%s\n' approve
        return 0
    fi
    if [ "$run_status" = completed ]; then
        printf '%s\n' resume
        return 0
    fi
    printf '%s\n' invalid
}

compare_immutable_target() {
    rebuilt_binary="$1"
    tarball="$2"
    version="$3"
    target_name="$4"
    notice="$5"
    third_party_notices="$6"
    license="$7"
    comparison_root="$8"
    stage="dbmd-${version}-${target_name}"

    # Workflow artifacts are untrusted until this comparison succeeds. Never
    # extract their caller-controlled paths: an archive can otherwise escape
    # the scratch directory through `..`, an absolute entry, or a symlink
    # pivot. Require exactly one directory plus four regular members, then
    # stream each named member to a fixed local filename.
    expected_entries="$(
        printf '%s\n' \
            "${stage}/" \
            "${stage}/LICENSE" \
            "${stage}/NOTICE" \
            "${stage}/THIRD_PARTY_NOTICES" \
            "${stage}/dbmd" |
            LC_ALL=C sort
    )"
    actual_entries="$(tar -tzf "$tarball" | LC_ALL=C sort)" || return 1
    [ "$actual_entries" = "$expected_entries" ] || return 1

    expected_types="$(
        printf '%s\n' \
            "d ${stage}/" \
            "- ${stage}/LICENSE" \
            "- ${stage}/NOTICE" \
            "- ${stage}/THIRD_PARTY_NOTICES" \
            "- ${stage}/dbmd" |
            LC_ALL=C sort
    )"
    actual_types="$(
        tar -tvzf "$tarball" |
            awk '{ print substr($1, 1, 1) " " $NF }' |
            LC_ALL=C sort
    )" || return 1
    [ "$actual_types" = "$expected_types" ] || return 1

    mkdir -p "$comparison_root"
    tar -xOzf "$tarball" "${stage}/dbmd" >"$comparison_root/dbmd" &&
        tar -xOzf "$tarball" "${stage}/NOTICE" >"$comparison_root/NOTICE" &&
        tar -xOzf "$tarball" "${stage}/THIRD_PARTY_NOTICES" \
            >"$comparison_root/THIRD_PARTY_NOTICES" &&
        tar -xOzf "$tarball" "${stage}/LICENSE" >"$comparison_root/LICENSE" &&
        cmp "$rebuilt_binary" "$comparison_root/dbmd" &&
        cmp "$notice" "$comparison_root/NOTICE" &&
        cmp "$third_party_notices" "$comparison_root/THIRD_PARTY_NOTICES" &&
        cmp "$license" "$comparison_root/LICENSE"
}
