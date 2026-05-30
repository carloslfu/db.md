#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Add `SPDX-License-Identifier: Apache-2.0` to source files that don't
# already have one. Idempotent.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

add_header() {
  local file="$1"
  local comment="$2"
  local header="${comment} SPDX-License-Identifier: Apache-2.0"

  if grep -q 'SPDX-License-Identifier' "$file" 2>/dev/null; then
    return 0
  fi

  if head -1 "$file" | grep -q '^#!'; then
    local first_line rest
    first_line="$(head -1 "$file")"
    rest="$(tail -n +2 "$file")"
    {
      printf '%s\n%s\n\n%s\n' "$first_line" "$header" "$rest"
    } > "$file.tmp"
  else
    {
      printf '%s\n\n' "$header"
      cat "$file"
    } > "$file.tmp"
  fi
  mv "$file.tmp" "$file"
  echo "  + $file"
}

while IFS= read -r -d '' f; do
  add_header "$f" "//"
done < <(find . -type f -name '*.go' ! -path './.git/*' -print0)

while IFS= read -r -d '' f; do
  add_header "$f" "#"
done < <(find . -type f -name '*.sh' ! -path './.git/*' -print0)

echo "done."
