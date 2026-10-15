#!/usr/bin/env bash
# Test suite for install.sh.
#
# Runs without network access: curl/wget/uname are mocked in a fake PATH that
# serves fixture release assets from a local directory.
#
# Usage: bash tests/test_install_script.sh
set -euo pipefail

# Hermetic even with ambient MUSTER_* in the environment.
unset MUSTER_VERSION MUSTER_INSTALL_DIR MUSTER_REPO

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAKE_BIN="$WORK/bin"
FIXTURE="$WORK/fixture"
mkdir -p "$FAKE_BIN" "$FIXTURE"

PASS=0
FAIL=0

ok() {
    PASS=$((PASS + 1))
    printf 'ok   - %s\n' "$1"
}

ko() {
    FAIL=$((FAIL + 1))
    printf 'FAIL - %s\n' "$1" >&2
}

assert_eq() { # description actual expected
    if [ "$2" = "$3" ]; then
        ok "$1"
    else
        ko "$1 (expected: $3, got: $2)"
    fi
}

assert_contains() { # description haystack needle
    case "$2" in
        *"$3"*) ok "$1" ;;
        *) ko "$1 (output does not contain: $3)" ;;
    esac
}

hash_of() { # sha-256 of $1, portable
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# ---------------------------------------------------------------- fixtures --

TARGETS=(
    x86_64-unknown-linux-gnu
    aarch64-unknown-linux-gnu
    x86_64-apple-darwin
    aarch64-apple-darwin
)

for target in "${TARGETS[@]}"; do
    staging="$WORK/staging-$target"
    mkdir -p "$staging"
    # Fake but executable "binary" that identifies its target when run.
    printf '#!/bin/sh\necho muster-fixture-%s\n' "$target" >"$staging/muster"
    chmod +x "$staging/muster"
    cp "$REPO_ROOT/LICENSE" "$REPO_ROOT/README.md" "$staging/"
    tar -czf "$FIXTURE/muster-v0.1.0-$target.tar.gz" -C "$staging" muster LICENSE README.md
done

# SHA256SUMS with the hashes of the *good* tarballs.
(
    cd "$FIXTURE"
    for f in ./*.tar.gz; do
        echo "$(hash_of "$f")  $(basename "$f")"
    done
) >"$FIXTURE/SHA256SUMS"

# ------------------------------------------------------------------- mocks --

cat >"$FAKE_BIN/curl" <<'MOCK'
#!/usr/bin/env bash
# mock curl backed by $MOCK_FIXTURE; logs every URL to $MOCK_LOG
set -u
url=""
out="/dev/null"
mode=""
args=("$@")
for i in "${!args[@]}"; do
    case "${args[$i]}" in
        http*) url="${args[$i]}" ;;
        -o) out="${args[$((i + 1))]}" ;;
        -w) mode="effective-url" ;;
    esac
done
: "${MOCK_FIXTURE:?}"; : "${MOCK_LOG:?}"
echo "$url" >>"$MOCK_LOG"
case "$url" in
    */releases/latest)
        if [ "$mode" = "effective-url" ]; then
            printf '%s' "https://github.com/oliveralerubio/muster/releases/tag/v0.1.0"
        fi
        ;;
    */SHA256SUMS)
        cp "$MOCK_FIXTURE/SHA256SUMS" "$out"
        ;;
    *muster-*.tar.gz)
        if [ "${MOCK_CORRUPT:-0}" = "1" ]; then
            { cat "$MOCK_FIXTURE/$(basename "$url")"; printf 'CORRUPTION'; } >"$out"
        else
            cp "$MOCK_FIXTURE/$(basename "$url")" "$out"
        fi
        ;;
    *)
        printf 'mock curl: unexpected url: %s\n' "$url" >&2
        exit 22
        ;;
esac
exit 0
MOCK
chmod +x "$FAKE_BIN/curl"

cat >"$FAKE_BIN/wget" <<'MOCK'
#!/usr/bin/env bash
# mock wget backed by $MOCK_FIXTURE; logs every URL to $MOCK_LOG
set -u
url=""
out="/dev/null"
args=("$@")
for i in "${!args[@]}"; do
    case "${args[$i]}" in
        http*) url="${args[$i]}" ;;
        -O) out="${args[$((i + 1))]}" ;;
    esac
done
: "${MOCK_FIXTURE:?}"; : "${MOCK_LOG:?}"
echo "$url" >>"$MOCK_LOG"
case "$url" in
    */releases/latest)
        printf '  HTTP/1.1 302 Found\n' >&2
        printf '  Location: https://github.com/oliveralerubio/muster/releases/tag/v0.1.0\n' >&2
        ;;
    */SHA256SUMS)
        cp "$MOCK_FIXTURE/SHA256SUMS" "$out"
        ;;
    *muster-*.tar.gz)
        if [ "${MOCK_CORRUPT:-0}" = "1" ]; then
            { cat "$MOCK_FIXTURE/$(basename "$url")"; printf 'CORRUPTION'; } >"$out"
        else
            cp "$MOCK_FIXTURE/$(basename "$url")" "$out"
        fi
        ;;
    *)
        printf 'mock wget: unexpected url: %s\n' "$url" >&2
        exit 8
        ;;
esac
exit 0
MOCK
chmod +x "$FAKE_BIN/wget"

cat >"$FAKE_BIN/uname" <<'MOCK'
#!/usr/bin/env bash
set -u
case "${1:-}" in
    -m) printf '%s\n' "${MOCK_UNAME_M:?}" ;;
    *) printf '%s\n' "${MOCK_UNAME_S:?}" ;;
esac
MOCK
chmod +x "$FAKE_BIN/uname"

# ------------------------------------------------------------------ runner --

# run_install <shell> [extra env as NAME=VALUE...] ; sets $status, $output, $requests.
# Set RUN_INSTALL_PATH to replace the whole PATH given to install.sh.
run_install() {
    local shell_bin="$1"
    shift
    local path_spec="${RUN_INSTALL_PATH:-$FAKE_BIN:$PATH}"
    local log_file="$WORK/requests.log"
    : >"$log_file"
    set +e
    output="$(
        env PATH="$path_spec" \
            MOCK_FIXTURE="$FIXTURE" MOCK_LOG="$log_file" \
            MOCK_UNAME_S="${MOCK_UNAME_S:-Linux}" MOCK_UNAME_M="${MOCK_UNAME_M:-x86_64}" \
            "$@" \
            "$shell_bin" "$INSTALL_SH" 2>&1
    )"
    status=$?
    set -e
    requests="$log_file"
}

# ------------------------------------------------------------------- tests --

echo "# detection logic (source install.sh and call the functions)"

detect_with() { # MOCK_UNAME_S MOCK_UNAME_M -> echoes "os|arch|target"
    (
        export PATH="$FAKE_BIN:$PATH" MOCK_UNAME_S="$1" MOCK_UNAME_M="$2"
        MUSTER_INSTALL_SKIP_MAIN=1 . "$INSTALL_SH"
        printf '%s|%s|%s\n' "$(detect_os)" "$(detect_arch)" "$(resolve_target)"
    )
}

while IFS='|' read -r u m os arch target; do
    actual="$(detect_with "$u" "$m")"
    assert_eq "detect $u/$m -> $target" "$actual" "$os|$arch|$target"
done <<'CASES'
Linux|x86_64|linux|x86_64|x86_64-unknown-linux-gnu
Linux|amd64|linux|x86_64|x86_64-unknown-linux-gnu
Linux|aarch64|linux|aarch64|aarch64-unknown-linux-gnu
Darwin|x86_64|darwin|x86_64|x86_64-apple-darwin
Darwin|arm64|darwin|aarch64|aarch64-apple-darwin
CASES

# Unsupported platforms must fail (run as subprocess so exit() stays contained).
for bad_os in CYGWIN_NT-10.0 MINGW64_NT-10.0 MSYS_NT-10.0 FreeBSD; do
    run_install sh MOCK_UNAME_S="$bad_os" MUSTER_VERSION=0.1.0
    if [ "$status" -ne 0 ]; then
        ok "rejects unsupported OS $bad_os with non-zero exit"
    else
        ko "unsupported OS $bad_os was accepted"
    fi
done
run_install sh MOCK_UNAME_S=CYGWIN_NT-10.0 MUSTER_VERSION=0.1.0
assert_contains "Windows rejection message mentions WSL" "$output" "WSL"

run_install sh MOCK_UNAME_M=mips MUSTER_VERSION=0.1.0
if [ "$status" -ne 0 ]; then
    ok "rejects unsupported architecture with non-zero exit"
else
    ko "unsupported architecture mips was accepted"
fi
assert_contains "architecture rejection message is clear" "$output" "architecture"

echo "# happy path"

dest="$WORK/dest"
run_install sh MOCK_UNAME_S=Darwin MOCK_UNAME_M=arm64 \
    MUSTER_VERSION=0.1.0 MUSTER_INSTALL_DIR="$dest"
assert_eq "install exits 0" "$status" "0"
assert_contains "install output confirms success" "$output" "Installed $dest/muster"
assert_contains "install normalizes version without v prefix" "$(cat "$requests")" \
    "https://github.com/oliveralerubio/muster/releases/download/v0.1.0/muster-v0.1.0-aarch64-apple-darwin.tar.gz"
assert_contains "install downloads SHA256SUMS" "$(cat "$requests")" "SHA256SUMS"
installed_marker="$("$dest/muster" 2>/dev/null || true)"
assert_eq "installed binary is the fixture binary and is executable" \
    "$installed_marker" "muster-fixture-aarch64-apple-darwin"

dest="$WORK/dest2"
run_install sh MOCK_UNAME_S=Linux MOCK_UNAME_M=x86_64 \
    MUSTER_VERSION=v0.1.0 MUSTER_INSTALL_DIR="$dest"
assert_eq "install (Linux, explicit v-tag) exits 0" "$status" "0"
installed_marker="$("$dest/muster" 2>/dev/null || true)"
assert_eq "installed Linux binary works" "$installed_marker" "muster-fixture-x86_64-unknown-linux-gnu"

echo "# latest version resolution"

dest="$WORK/dest3"
run_install sh MOCK_UNAME_S=Linux MOCK_UNAME_M=x86_64 MUSTER_INSTALL_DIR="$dest"
assert_eq "install without MUSTER_VERSION resolves latest and exits 0" "$status" "0"
assert_contains "latest resolution downloads the v0.1.0 asset" "$(cat "$requests")" \
    "releases/download/v0.1.0/muster-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"

echo "# checksum failure"

dest="$WORK/dest4"
run_install sh MOCK_UNAME_S=Linux MOCK_UNAME_M=x86_64 MOCK_CORRUPT=1 \
    MUSTER_VERSION=v0.1.0 MUSTER_INSTALL_DIR="$dest"
if [ "$status" -ne 0 ]; then
    ok "corrupted tarball aborts with non-zero exit"
else
    ko "corrupted tarball was accepted"
fi
assert_contains "checksum mismatch is reported" "$output" "checksum mismatch"
if [ -e "$dest/muster" ]; then
    ko "binary was installed despite checksum mismatch"
else
    ok "nothing is installed when the checksum fails"
fi

echo "# wget fallback"

# PATH that contains no curl at all: the fake bin dir plus symlinks to the
# system tools install.sh needs, forcing the wget code path.
wget_only="$WORK/bin-wget-only"
mkdir -p "$wget_only"
cp "$FAKE_BIN/wget" "$FAKE_BIN/uname" "$wget_only/"
for tool in sh bash mktemp tar gzip gunzip mkdir cp chmod rm sed awk cut tail sha256sum shasum basename cat; do
    src="$(command -v "$tool" 2>/dev/null || true)"
    if [ -n "$src" ]; then
        ln -s "$src" "$wget_only/$tool"
    fi
done

dest="$WORK/dest5"
RUN_INSTALL_PATH="$wget_only" run_install sh MOCK_UNAME_S=Linux MOCK_UNAME_M=x86_64 \
    MUSTER_VERSION=v0.1.0 MUSTER_INSTALL_DIR="$dest"
assert_eq "install via wget exits 0" "$status" "0"
assert_contains "wget resolves the latest tag via Location header" "$(cat "$requests")" \
    "releases/download/v0.1.0/muster-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
installed_marker="$("$dest/muster" 2>/dev/null || true)"
assert_eq "wget-installed binary works" "$installed_marker" "muster-fixture-x86_64-unknown-linux-gnu"

echo "# wget latest version resolution"

# Same wget-only PATH, but with MUSTER_VERSION unset: install.sh must resolve
# the latest release through the /releases/latest redirect.
dest="$WORK/dest5b"
RUN_INSTALL_PATH="$wget_only" run_install sh MOCK_UNAME_S=Linux MOCK_UNAME_M=x86_64 \
    MUSTER_INSTALL_DIR="$dest"
assert_eq "install via wget without MUSTER_VERSION exits 0" "$status" "0"
assert_contains "wget without MUSTER_VERSION requests /releases/latest" "$(cat "$requests")" \
    "releases/latest"
assert_contains "wget latest resolution downloads the v0.1.0 asset" "$(cat "$requests")" \
    "releases/download/v0.1.0/muster-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
installed_marker="$([[ -e "$dest/muster" ]] && "$dest/muster" 2>/dev/null || true)"
assert_eq "wget latest-installed binary works" "$installed_marker" "muster-fixture-x86_64-unknown-linux-gnu"

echo "# PATH advice"

dest="$WORK/dest6"
run_install sh MOCK_UNAME_S=Linux MOCK_UNAME_M=x86_64 \
    MUSTER_VERSION=v0.1.0 MUSTER_INSTALL_DIR="$dest"
assert_contains "advice to extend PATH is shown when needed" "$output" \
    'export PATH="'"$dest"':$PATH"'

echo "# POSIX compliance"
if command -v dash >/dev/null 2>&1; then
    if dash -n "$INSTALL_SH" 2>/dev/null; then
        ok "install.sh parses under dash"
    else
        ko "install.sh is not valid POSIX sh for dash"
    fi
else
    if sh -n "$INSTALL_SH"; then
        ok "install.sh parses under sh"
    else
        ko "install.sh does not parse under sh"
    fi
fi

# ----------------------------------------------------------------- summary --

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
