#!/bin/sh
# SPDX-License-Identifier: Apache-2.0

# Hermetic adversarial checks for install.sh's trust split. No network access.
set -eu

repo_root="$(cd -- "$(dirname -- "$0")/.." && pwd)"
installer="$repo_root/scripts/install.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fake="$tmp/fake-bin"
mkdir -p "$fake"
REAL_CP="$(command -v cp)"
export REAL_CP

grep -Fq '"$verified" __install-verified "$verified" "$DBMD_INSTALL_DIR"' "$installer"
if grep -Fq 'mkdir -p "$DBMD_INSTALL_DIR"' "$installer" ||
    grep -Fq 'mktemp -d "$DBMD_INSTALL_DIR' "$installer" ||
    grep -Fq 'mv "$staged" "$DBMD_INSTALL_DIR' "$installer" ||
    grep -Fq 'rm -rf "$install_stage"' "$installer"; then
    printf '%s\n' "installer reopened a mutable destination pathname" >&2
    exit 1
fi

cat >"$fake/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
    -s) printf '%s\n' Linux ;;
    -m) printf '%s\n' x86_64 ;;
    *) exit 1 ;;
esac
EOF

cat >"$fake/curl" <<'EOF'
#!/bin/sh
out=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) out="$2"; shift 2 ;;
        -*) shift ;;
        *) url="$1"; shift ;;
    esac
done
printf '%s\n' "$url" >>"$CURL_LOG"
case "$url" in
    https://trusted.example/latest) payload='1.2.3' ;;
    https://api.github.com/*) payload='{"tag_name":"v9.9.9"}' ;;
    https://trusted.example/1.2.3/dbmd-1.2.3-linux-x86_64-musl.tar.gz)
        case "${FAKE_SCENARIO:-ok}" in
            missing-digest) exit 22 ;;
            mismatched-digest)
                payload='bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' ;;
            *) payload='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' ;;
        esac ;;
    */SHA256SUMS)
        if [ -n "$out" ]; then
            printf '%s  %s\n' \
                aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
                dbmd-1.2.3-linux-x86_64-musl.tar.gz >"$out"
            exit 0
        fi
        exit 22 ;;
    */dbmd-1.2.3-linux-x86_64-musl.tar.gz) payload='fake archive bytes' ;;
    *) printf 'unexpected URL: %s\n' "$url" >&2; exit 22 ;;
esac
if [ -n "$out" ]; then
    printf '%s' "$payload" >"$out"
else
    printf '%s\n' "$payload"
fi
EOF

cat >"$fake/sha256sum" <<'EOF'
#!/bin/sh
printf '%s  %s\n' aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa "$1"
EOF

cat >"$fake/tar" <<'EOF'
#!/bin/sh
dest=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -C) dest="$2"; shift 2 ;;
        *) shift ;;
    esac
done
mkdir -p "$dest/dbmd-1.2.3-linux-x86_64-musl"
cat >"$dest/dbmd-1.2.3-linux-x86_64-musl/dbmd" <<'SCRIPT'
#!/bin/sh
test "$1" = __install-verified
source="$2"
install_dir="$3"
mkdir -p "$install_dir"
"$REAL_CP" "$source" "$install_dir/dbmd"
chmod +x "$install_dir/dbmd"
SCRIPT
EOF
chmod +x "$fake/uname" "$fake/curl" "$fake/sha256sum" "$fake/tar"

# An attacker-controlled GitHub repository/API must not choose "latest".
log="$tmp/requests.log"
: >"$log"
PATH="$fake:$PATH" \
HOME="$tmp/home" \
CURL_LOG="$log" \
DBMD_REPO="attacker/owned" \
DBMD_BASE_URL="https://downloads.example" \
DBMD_TRUSTED_LATEST_URL="https://trusted.example/latest" \
DBMD_TRUSTED_MANIFEST_BASE="https://trusted.example" \
DBMD_INSTALL_DIR="$tmp/install" \
sh "$repo_root/scripts/install.sh" >/dev/null
test -x "$tmp/install/dbmd"
test -z "$(find "$tmp/install" -maxdepth 1 -name '.dbmd-stage.*' -print -quit)"
grep -q '^https://trusted.example/latest$' "$log"
if grep -q 'api.github.com' "$log"; then
    printf '%s\n' "installer consulted GitHub for trusted latest" >&2
    exit 1
fi

# A release asset without an independently served digest must fail closed.
if PATH="$fake:$PATH" \
    HOME="$tmp/home" \
    CURL_LOG="$log" \
    FAKE_SCENARIO="missing-digest" \
    DBMD_VERSION="1.2.3" \
    DBMD_BASE_URL="https://downloads.example" \
    DBMD_TRUSTED_MANIFEST_BASE="https://trusted.example" \
    DBMD_INSTALL_DIR="$tmp/install-missing" \
    sh "$repo_root/scripts/install.sh" >"$tmp/missing.out" 2>"$tmp/missing.err"; then
    printf '%s\n' "release without an independent digest was installed" >&2
    exit 1
fi
grep -q 'request failed: https://trusted.example/1.2.3/' "$tmp/missing.err"
test ! -e "$tmp/install-missing/dbmd"

# Replacing the release asset while leaving the independent digest intact is
# detected before anything is installed.
if PATH="$fake:$PATH" \
    HOME="$tmp/home" \
    CURL_LOG="$log" \
    FAKE_SCENARIO="mismatched-digest" \
    DBMD_VERSION="1.2.3" \
    DBMD_BASE_URL="https://downloads.example" \
    DBMD_TRUSTED_MANIFEST_BASE="https://trusted.example" \
    DBMD_INSTALL_DIR="$tmp/install-mismatch" \
    sh "$repo_root/scripts/install.sh" >"$tmp/mismatch.out" 2>"$tmp/mismatch.err"; then
    printf '%s\n' "release with a mismatched independent digest was installed" >&2
    exit 1
fi
grep -q 'checksum verification failed' "$tmp/mismatch.err"
test ! -e "$tmp/install-mismatch/dbmd"

# A deliberately configured non-GitHub mirror may use its colocated
# SHA256SUMS only through the explicit downgrade opt-in.
PATH="$fake:$PATH" \
HOME="$tmp/home" \
CURL_LOG="$log" \
DBMD_VERSION="1.2.3" \
DBMD_BASE_URL="https://mirror.example/releases" \
DBMD_ALLOW_SAME_ORIGIN_CHECKSUM="1" \
DBMD_INSTALL_DIR="$tmp/install-mirror" \
sh "$repo_root/scripts/install.sh" >/dev/null
test -x "$tmp/install-mirror/dbmd"
test -z "$(find "$tmp/install-mirror" -maxdepth 1 -name '.dbmd-stage.*' -print -quit)"

# The test-only same-origin escape hatch cannot downgrade official releases.
if PATH="$fake:$PATH" \
    HOME="$tmp/home" \
    CURL_LOG="$log" \
    DBMD_VERSION="1.2.3" \
    DBMD_BASE_URL="https://github.com/carloslfu/db.md/releases/download" \
    DBMD_ALLOW_SAME_ORIGIN_CHECKSUM="1" \
    DBMD_INSTALL_DIR="$tmp/install-unsafe" \
    sh "$repo_root/scripts/install.sh" >"$tmp/unsafe.out" 2>"$tmp/unsafe.err"; then
    printf '%s\n' "official GitHub same-origin checksum downgrade was accepted" >&2
    exit 1
fi
grep -q 'forbidden for the official GitHub release origin' "$tmp/unsafe.err"

printf '%s\n' "installer security tests passed"
