#!/bin/sh
# RandDB installer for macOS, Linux, and WSL.
#
#   curl -fsSL https://raw.githubusercontent.com/wchiway/randdb/main/install.sh | sh
#
# The installer resolves a release, downloads the archive for this platform,
# verifies it against the release's SHA256SUMS, installs the binary, and
# creates the configuration template unless --no-init is given.
#
# Environment:
#   RANDDB_VERSION            release tag to install (default: newest release)
#   RANDDB_INSTALL_DIR        installation directory (default: ~/.local/bin)
#   RANDDB_RELEASE_BASE_URL   release host (default: https://github.com)
#   RANDDB_API_BASE_URL       API host (default: https://api.github.com)
set -eu

REPO="wchiway/randdb"
BIN="randdb"
RELEASE_BASE="${RANDDB_RELEASE_BASE_URL:-https://github.com}"
RELEASE_BASE="${RELEASE_BASE%/}"
API="${RANDDB_API_BASE_URL:-https://api.github.com}"
API="${API%/}/repos/${REPO}"

version="${RANDDB_VERSION:-}"
install_dir="${RANDDB_INSTALL_DIR:-}"
do_init=1
new_binary=""
tmp=""

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

say() {
    printf '%s\n' "$*"
}

have() {
    command -v "$1" >/dev/null 2>&1
}

cleanup() {
    [ -z "$new_binary" ] || rm -f "$new_binary"
    [ -z "$tmp" ] || rm -rf "$tmp"
}

usage() {
    cat <<'EOF'
Install the RandDB MCP server from GitHub releases.

Usage: install.sh [options]

Options:
  --version TAG   Install a specific release tag, for example v0.1.0-alpha.1.
                  Default: the newest release that provides a build for this
                  platform.
  --dir DIR       Install the binary into DIR. Default: ~/.local/bin, or
                  /usr/local/bin when running as root.
  --no-init       Do not create the configuration template (~/.randdb/.env).
  -h, --help      Show this help.

Environment:
  RANDDB_VERSION, RANDDB_INSTALL_DIR, RANDDB_RELEASE_BASE_URL, RANDDB_API_BASE_URL

Supported platforms: Linux x86_64, macOS arm64, Windows x86_64 (use install.ps1).
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
    --version)
        [ $# -ge 2 ] || die "--version requires a value"
        version="$2"
        shift 2
        ;;
    --version=*)
        version="${1#--version=}"
        shift
        ;;
    --dir)
        [ $# -ge 2 ] || die "--dir requires a value"
        install_dir="$2"
        shift 2
        ;;
    --dir=*)
        install_dir="${1#--dir=}"
        shift
        ;;
    --no-init)
        do_init=0
        shift
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        die "unknown option: $1 (try --help)"
        ;;
    esac
done

# Platform detection. Only the targets published by the release workflow are
# accepted; everything else has to build from source.
os=$(uname -s 2>/dev/null || echo unknown)
arch=$(uname -m 2>/dev/null || echo unknown)
case "$os/$arch" in
Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-gnu ;;
Darwin/arm64) target=aarch64-apple-darwin ;;
*) target="" ;;
esac
if [ -z "$target" ]; then
    die "no prebuilt binary for $os/$arch
Prebuilt binaries exist for:
  Linux x86_64   (x86_64-unknown-linux-gnu)
  macOS arm64    (aarch64-apple-darwin)
  Windows x86_64 (x86_64-pc-windows-msvc)
Build from source instead:
  cargo install --git https://github.com/${REPO} --locked"
fi

have curl || have wget || die "curl or wget is required"

if have sha256sum; then
    sha_tool=sha256sum
elif have shasum; then
    sha_tool=shasum
elif have openssl; then
    sha_tool=openssl
else
    die "sha256sum, shasum, or openssl is required to verify the download"
fi

have tar || die "tar is required to unpack the release archive"

if [ -z "$install_dir" ]; then
    if [ "$(id -u)" = 0 ]; then
        install_dir=/usr/local/bin
    else
        install_dir="$HOME/.local/bin"
    fi
fi

asset_for() {
    printf 'randdb-%s-%s.tar.gz' "$1" "$target"
}

asset_url() {
    printf '%s/%s/releases/download/%s/%s' "$RELEASE_BASE" "$REPO" "$1" "$(asset_for "$1")"
}

fetch() { # url destination
    if have curl; then
        if [ -t 2 ]; then
            curl -fL --progress-bar --retry 3 --retry-delay 2 --connect-timeout 15 -o "$2" "$1"
        else
            curl -fsSL --retry 3 --retry-delay 2 --connect-timeout 15 -o "$2" "$1"
        fi
    else
        wget -q --tries=3 --timeout=15 -O "$2" "$1"
    fi
}

fetch_stdout() { # url
    if have curl; then
        curl -fsSL --connect-timeout 15 "$1"
    else
        wget -q --timeout=15 -O - "$1"
    fi
}

release_tags() {
    body=$(fetch_stdout "${API}/releases?per_page=30") || return 1
    if have jq; then
        printf '%s' "$body" | jq -r '.[].tag_name'
    else
        printf '%s\n' "$body" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p'
    fi
}

asset_exists() { # tag
    url=$(asset_url "$1")
    if have curl; then
        code=$(curl -sIL -o /dev/null -w '%{http_code}' "$url" 2>/dev/null) || code=000
        case "$code" in
        2*) return 0 ;;
        404 | 403) return 1 ;;
        *) return 0 ;;
        esac
    else
        wget --spider -q "$url" 2>/dev/null
    fi
}

newest_release_with_asset() {
    tags=$(release_tags) || return 1
    [ -n "$tags" ] || return 2
    for tag in $tags; do
        if asset_exists "$tag"; then
            printf '%s' "$tag"
            return 0
        fi
    done
    return 3
}

if [ -z "$version" ]; then
    say "Looking up the newest release with a ${target} build..."
    status=0
    version=$(newest_release_with_asset) || status=$?
    case "$status" in
    0) ;;
    1) die "could not list releases from ${API}
Check your network connection, or pass --version to install a specific tag." ;;
    2) die "no releases found in ${REPO}" ;;
    *) die "no release provides a ${target} build
Pass --version to install a specific tag, or build from source." ;;
    esac
fi

asset=$(asset_for "$version")
archive_url=$(asset_url "$version")
sums_url="${RELEASE_BASE}/${REPO}/releases/download/${version}/SHA256SUMS"

say "Installing RandDB ${version} (${target})"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/randdb-install.XXXXXX")
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

say "Downloading ${asset}..."
fetch "$archive_url" "${tmp}/${asset}" ||
    die "could not download ${archive_url}
If the repository is private, the release assets are not publicly reachable."
fetch "$sums_url" "${tmp}/SHA256SUMS" || die "could not download ${sums_url}"

expected=$(awk -v n="$asset" '{ name = $2; sub(/^\*/, "", name) } name == n { print $1; exit }' "${tmp}/SHA256SUMS")
[ -n "$expected" ] || die "SHA256SUMS does not list ${asset}"

case "$sha_tool" in
sha256sum) actual=$(sha256sum "${tmp}/${asset}" | cut -d' ' -f1) ;;
shasum) actual=$(shasum -a 256 "${tmp}/${asset}" | cut -d' ' -f1) ;;
*) actual=$(openssl dgst -sha256 "${tmp}/${asset}" | awk '{ print $NF }') ;;
esac
if [ "$actual" != "$expected" ]; then
    die "checksum mismatch for ${asset}
expected ${expected}
got      ${actual}"
fi

tar -xzf "${tmp}/${asset}" -C "$tmp" || die "could not unpack ${asset}"
[ -f "${tmp}/${BIN}" ] || die "${asset} does not contain ${BIN}"

mkdir -p "$install_dir" || die "could not create ${install_dir}"
dest="${install_dir}/${BIN}"
if [ -e "$dest" ] && [ ! -w "$dest" ]; then
    die "${dest} is not writable; re-run with --dir DIR or with sudo"
fi
if [ ! -w "$install_dir" ]; then
    die "${install_dir} is not writable; re-run with --dir DIR or with sudo"
fi

# Replace the binary with a same-directory rename so a running MCP server keeps
# its old inode and the new file appears atomically.
new_binary="${install_dir}/.${BIN}.new.$$"
cp "${tmp}/${BIN}" "$new_binary" || die "could not write to ${install_dir}"
chmod 0755 "$new_binary"
mv -f "$new_binary" "$dest" || die "could not install to ${dest}"
new_binary=""

if ! "$dest" --version >/dev/null 2>&1; then
    warn "${dest} was installed but could not be executed"
    case "$os" in
    Linux) warn "the Linux build is produced on Ubuntu 24.04; older distributions may lack a compatible glibc" ;;
    Darwin) warn "if macOS blocked it, run: xattr -d com.apple.quarantine ${dest}" ;;
    esac
    exit 1
fi

if [ "$do_init" = 1 ]; then
    "$dest" init </dev/null || warn "randdb init failed; run '${BIN} init' manually"
fi

case ":${PATH:-}:" in
*":${install_dir}:"*) on_path=1 ;;
*) on_path=0 ;;
esac

say ""
say "RandDB ${version} installed to ${dest}"

if [ "$on_path" = 0 ]; then
    say ""
    say "${install_dir} is not on your PATH. Add it with:"
    say ""
    say "  export PATH=\"${install_dir}:\$PATH\""
    case "${SHELL:-}" in
    */zsh) say "Add that line to ~/.zshrc to make it permanent." ;;
    */bash) say "Add that line to ~/.bashrc to make it permanent." ;;
    *) say "Add that line to your shell profile to make it permanent." ;;
    esac
fi

if [ "$on_path" = 1 ]; then
    mcp_command="$BIN"
else
    mcp_command="$dest"
fi

say ""
say "Next steps:"
if [ "$do_init" = 1 ]; then
    say "  1. Add your API keys to ${RANDDB_HOME:-$HOME/.randdb}/.env"
else
    say "  1. Run '${BIN} init' and add your API keys to ${RANDDB_HOME:-$HOME/.randdb}/.env"
fi
say "  2. Add the server to your MCP client:"
say ""
say '     {'
say '       "mcpServers": {'
say "         \"${BIN}\": { \"command\": \"${mcp_command}\", \"args\": [\"mcp\"] }"
say '       }'
say '     }'
