#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Capture public SDK file/symlink fingerprints after the release verifier has
# already rejected a hosted runner. This is diagnostic-only and never changes
# acceptance.
set -eu

developer_dir="${1:-}"
output_dir="${2:-}"
[ -n "$developer_dir" ] && [ -n "$output_dir" ] || {
    printf 'usage: %s DEVELOPER_DIR OUTPUT_DIR\n' "$0" >&2
    exit 2
}
DEVELOPER_DIR="$developer_dir"
export DEVELOPER_DIR
mkdir -p "$output_dir"

sdk_path="$(xcrun --sdk macosx --show-sdk-path)"
(
    cd "$sdk_path"
    find . -type f -print0 |
        LC_ALL=C sort -z |
        xargs -0 shasum -a 256
) >"$output_dir/sdk-regular-files.sha256"
(
    cd "$sdk_path"
    find . -type l -print |
        LC_ALL=C sort |
        while IFS= read -r link_name; do
            printf '%s\t%s\n' "$link_name" "$(readlink "$link_name")"
        done
) >"$output_dir/sdk-symlinks.tsv"

printf 'darwin SDK capture: regular files=%s symlinks=%s\n' \
    "$(wc -l <"$output_dir/sdk-regular-files.sha256" | tr -d ' ')" \
    "$(wc -l <"$output_dir/sdk-symlinks.tsv" | tr -d ' ')"
