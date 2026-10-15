#!/bin/sh
# Muster installer: downloads a verified release tarball from GitHub.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/oliveralerubio/muster/main/install.sh | sh
#
# Environment:
#   MUSTER_VERSION      release tag to install, with or without the leading "v"
#                       (default: latest published release)
#   MUSTER_INSTALL_DIR  where to place the binary (default: ~/.local/bin)
#   MUSTER_REPO         GitHub repository to download from
#                       (default: oliveralerubio/muster)
#
# POSIX sh. Requires curl or wget, tar, and sha256sum or shasum.
set -eu

MUSTER_REPO="${MUSTER_REPO:-oliveralerubio/muster}"
MUSTER_VERSION="${MUSTER_VERSION:-}"
MUSTER_INSTALL_DIR="${MUSTER_INSTALL_DIR:-$HOME/.local/bin}"

log() {
    printf '%s\n' "$*"
}

err() {
    printf 'install.sh: error: %s\n' "$*" >&2
}

die() {
    err "$*"
    exit 1
}

have() {
    command -v "$1" >/dev/null 2>&1
}

# Echo the operating system ("linux" or "darwin"); reject anything else.
detect_os() {
    _os="$(uname -s 2>/dev/null)" || die "cannot detect the operating system"
    case "$_os" in
        Linux* | linux*) printf 'linux' ;;
        Darwin* | darwin*) printf 'darwin' ;;
        MINGW* | MSYS* | CYGWIN* | msys* | cygwin*)
            die "Windows is not supported natively. Install Muster inside WSL instead: https://learn.microsoft.com/windows/wsl/install"
            ;;
        *)
            die "unsupported operating system: $_os (Muster supports Linux and macOS)"
            ;;
    esac
}

# Echo the CPU architecture ("x86_64" or "aarch64"); reject anything else.
detect_arch() {
    _arch="$(uname -m 2>/dev/null)" || die "cannot detect the CPU architecture"
    case "$_arch" in
        x86_64 | amd64 | x64) printf 'x86_64' ;;
        aarch64 | arm64) printf 'aarch64' ;;
        *)
            die "unsupported CPU architecture: $_arch (Muster supports x86_64/amd64 and aarch64/arm64)"
            ;;
    esac
}

# Echo the Rust target triple for this machine.
resolve_target() {
    _t_arch="$(detect_arch)"
    _t_os="$(detect_os)"
    case "$_t_os" in
        linux)
            case "$_t_arch" in
                x86_64) printf 'x86_64-unknown-linux-gnu' ;;
                aarch64) printf 'aarch64-unknown-linux-gnu' ;;
            esac
            ;;
        darwin)
            case "$_t_arch" in
                x86_64) printf 'x86_64-apple-darwin' ;;
                aarch64) printf 'aarch64-apple-darwin' ;;
            esac
            ;;
    esac
}

# Download $1 to file $2.
http_download() {
    if have curl; then
        curl -fsSL -o "$2" "$1" || die "download failed: $1"
    elif have wget; then
        wget -q -O "$2" "$1" || die "download failed: $1"
    else
        die "curl or wget is required to download Muster"
    fi
}

# Echo the tag of the latest published release, e.g. "v0.1.0".
resolve_latest_tag() {
    _latest_url="https://github.com/$MUSTER_REPO/releases/latest"
    if have curl; then
        _final_url="$(curl -fsSL -o /dev/null -w '%{url_effective}' "$_latest_url")" ||
            die "could not resolve the latest Muster release; check your network or set MUSTER_VERSION"
    elif have wget; then
        _final_url="$(wget -q -O /dev/null -S --max-redirect=10 "$_latest_url" 2>&1 |
            sed -n 's/^[[:space:]]*[Ll]ocation:[[:space:]]*//p' | tail -n 1)"
    else
        die "curl or wget is required to download Muster"
    fi
    [ -n "${_final_url:-}" ] || die "could not resolve the latest Muster release; set MUSTER_VERSION explicitly"
    printf '%s\n' "${_final_url##*/}"
}

# Echo the lowercase hex SHA-256 of file $1.
sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        die "sha256sum or shasum is required to verify the download"
    fi
}

# Verify that file $1 matches the "SHA256SUMS" entry listed as $3.
verify_checksum() {
    _file="$1"
    _sums="$2"
    _name="$3"
    _expected="$(awk -v f="$_name" '$2 == f || $2 == "*" f { print $1; found = 1 } END { if (!found) exit 1 }' "$_sums")" ||
        die "SHA256SUMS has no entry for $_name"
    _actual="$(sha256_of "$_file")"
    [ "$_actual" = "$_expected" ] ||
        die "checksum mismatch for $_name: expected $_expected, got $_actual. The download is corrupted or was tampered with; nothing was installed."
}

# Warn when the install directory is not on $PATH.
warn_if_not_in_path() {
    case ":$PATH:" in
        *":$MUSTER_INSTALL_DIR:"*) ;;
        *)
            log ""
            log "NOTE: $MUSTER_INSTALL_DIR is not in your PATH. Add this to your shell profile:"
            log "  export PATH=\"$MUSTER_INSTALL_DIR:\$PATH\""
            ;;
    esac
}

main() {
    log "Installing Muster ($MUSTER_REPO)..."

    target="$(resolve_target)"
    if [ -n "$MUSTER_VERSION" ]; then
        version="$MUSTER_VERSION"
    else
        version="$(resolve_latest_tag)"
    fi
    case "$version" in
        v*) tag="$version" ;;
        *) tag="v$version" ;;
    esac

    base_url="https://github.com/$MUSTER_REPO/releases/download/$tag"
    tarball_name="muster-$tag-$target.tar.gz"

    tmpdir="$(mktemp -d 2>/dev/null)" || die "cannot create a temporary directory"
    trap 'rm -rf "$tmpdir"' EXIT
    trap 'exit 1' INT TERM

    log "Downloading $tarball_name ..."
    http_download "$base_url/$tarball_name" "$tmpdir/$tarball_name"
    http_download "$base_url/SHA256SUMS" "$tmpdir/SHA256SUMS"
    verify_checksum "$tmpdir/$tarball_name" "$tmpdir/SHA256SUMS" "$tarball_name"
    log "Checksum verified."

    mkdir -p "$tmpdir/extract"
    tar -xzf "$tmpdir/$tarball_name" -C "$tmpdir/extract"
    [ -f "$tmpdir/extract/muster" ] || die "the archive does not contain a muster binary"

    mkdir -p "$MUSTER_INSTALL_DIR"
    cp "$tmpdir/extract/muster" "$MUSTER_INSTALL_DIR/muster"
    chmod +x "$MUSTER_INSTALL_DIR/muster"

    log "Installed $MUSTER_INSTALL_DIR/muster ($tag, $target)."
    warn_if_not_in_path
}

# When sourced (tests, or shell integration), only define functions.
if [ "${MUSTER_INSTALL_SKIP_MAIN:-0}" = "1" ]; then
    :
else
    main "$@"
fi
