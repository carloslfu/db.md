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

release_artifact_state() {
    pending_record="$1"
    run_status="$2"
    run_conclusion="$3"
    if [ -n "$pending_record" ]; then
        printf '%s\n' ready
        return 0
    fi
    if [ "$run_status" = completed ]; then
        if [ "$run_conclusion" = success ]; then
            printf '%s\n' ready
        else
            printf '%s\n' failed
        fi
        return 0
    fi
    printf '%s\n' invalid
}

# A workflow run id and environment id survive GitHub reruns. Approval is safe
# only if the exact attempt and pending-deployment record reviewed before the
# independent rebuild are still current at the point of use.
release_approval_state_matches() {
    captured_attempt="$1"
    current_attempt="$2"
    captured_pending="$3"
    current_pending="$4"

    [ -n "$captured_attempt" ] &&
        [ "$captured_attempt" = "$current_attempt" ] &&
        [ -n "$captured_pending" ] &&
        [ "$captured_pending" = "$current_pending" ]
}

# Classify a candidate release relative to a version already serving a mutable
# distribution channel. `comparison_status` is GitHub's compare status for
# `v<current>...v<candidate>`: `ahead` means the candidate descends from the
# current channel source on trunk.
release_channel_transition() {
    current_version="$1"
    candidate_version="$2"
    comparison_status="$3"

    if [ "$current_version" = "$candidate_version" ]; then
        if [ "$comparison_status" = identical ]; then
            printf '%s\n' exact
        else
            printf '%s\n' invalid
        fi
        return 0
    fi

    case "$comparison_status" in
        ahead) printf '%s\n' advance ;;
        behind) printf '%s\n' stale ;;
        *) printf '%s\n' invalid ;;
    esac
}

# Extract the one exact version declaration from a rendered Homebrew formula.
# Ambiguous or malformed input is rejected instead of letting a stale
# controller choose whichever line happened to match first.
release_formula_version() {
    formula_file="$1"
    awk '
        /^[[:space:]]*version "[^"]+"[[:space:]]*$/ {
            value = $0
            sub(/^[[:space:]]*version "/, "", value)
            sub(/"[[:space:]]*$/, "", value)
            versions[++count] = value
        }
        END {
            if (count != 1) {
                exit 1
            }
            print versions[1]
        }
    ' "$formula_file"
}

# Decide what to do after a `latest` edit using the Homebrew head as its
# ordering fence. An unchanged head proves the exact formula reviewed before
# the edit is still current. If the head changed, only a strict descendant may
# be repaired forward; every other race fails without choosing a tag.
release_latest_fence_action() {
    tap_head_before="$1"
    tap_head_after="$2"
    raced_transition="$3"

    if [ "$tap_head_before" = "$tap_head_after" ]; then
        printf '%s\n' stable
    elif [ "$raced_transition" = advance ]; then
        printf '%s\n' repair-forward
    else
        printf '%s\n' invalid
    fi
}

# Bind a final-channel mutation to the exact tap commit whose formula was
# inspected. Sampling a new head after that inspection is unsafe: a newer
# controller may already have advanced the tap in the gap.
release_latest_preflight_action() {
    verified_tap_head="$1"
    live_tap_head="$2"

    if [ "$verified_tap_head" = "$live_tap_head" ]; then
        printf '%s\n' proceed
    else
        printf '%s\n' stale
    fi
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
