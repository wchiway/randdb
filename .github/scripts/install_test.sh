#!/usr/bin/env bash
# Exercises install.sh and install.ps1 against a locally served release tree.
#
#   install_test.sh --binary PATH --target TRIPLE [options]
#
# The harness lays out a GitHub-like tree (release assets, SHA256SUMS, and a
# releases API response) under a scratch directory, serves it over HTTP, runs
# the installer for the current platform against it, and checks the result.
# The install scripts accept RANDDB_RELEASE_BASE_URL and RANDDB_API_BASE_URL
# overrides for exactly this purpose; mirrors use the same settings.
#
# Options:
#   --binary PATH          built randdb (randdb.exe on Windows)
#   --target TRIPLE        release target triple, for example x86_64-unknown-linux-gnu
#   --tag TAG              release tag to fake (default: v0.0.0-install-test)
#   --expect-version V     version printed by `randdb --version`
#                          (default: the tag without a leading v)
#   --port N               HTTP port for the local server (default: 8731)
#   --root DIR             scratch directory (default: a new temporary directory)
set -euo pipefail

binary=""
target=""
tag="v0.0.0-install-test"
expect_version=""
port=8731
root=""

while [ $# -gt 0 ]; do
    case "$1" in
    --binary)
        binary="${2:?--binary requires a value}"
        shift 2
        ;;
    --target)
        target="${2:?--target requires a value}"
        shift 2
        ;;
    --tag)
        tag="${2:?--tag requires a value}"
        shift 2
        ;;
    --expect-version)
        expect_version="${2:?--expect-version requires a value}"
        shift 2
        ;;
    --port)
        port="${2:?--port requires a value}"
        shift 2
        ;;
    --root)
        root="${2:?--root requires a value}"
        shift 2
        ;;
    -h | --help)
        sed -n '2,20p' "$0"
        exit 0
        ;;
    *)
        printf 'error: unknown option: %s\n' "$1" >&2
        exit 2
        ;;
    esac
done

[ -n "$binary" ] || {
    printf 'error: --binary is required\n' >&2
    exit 2
}
[ -n "$target" ] || {
    printf 'error: --target is required\n' >&2
    exit 2
}
[ -f "$binary" ] || {
    printf 'error: %s does not exist; build it first\n' "$binary" >&2
    exit 2
}
[ -n "$expect_version" ] || expect_version="${tag#v}"

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
[ -n "$root" ] || root=$(mktemp -d)

is_windows() {
    case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) return 0 ;;
    *) return 1 ;;
    esac
}

# Native programs such as python need Windows paths when running under Git Bash.
native_path() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -w "$1"
    else
        printf '%s' "$1"
    fi
}

if is_windows; then
    bin_name="randdb.exe"
else
    bin_name="randdb"
fi

failures=0
check() { # description command...
    local description="$1"
    shift
    if "$@" >/dev/null 2>&1; then
        printf 'ok   %s\n' "$description"
    else
        printf 'FAIL %s\n' "$description"
        failures=$((failures + 1))
    fi
}

archive="randdb-${tag}-${target}.tar.gz"
release_dir="$root/www/wchiway/randdb/releases/download/$tag"
bad_dir="$root/www/bad/wchiway/randdb/releases/download/$tag"
api_dir="$root/www/api/repos/wchiway/randdb"

mkdir -p "$release_dir" "$bad_dir" "$api_dir" "$root/stage"

# Package the binary the way .github/workflows/publish.yml does. Keep the
# archive name, its contents, and SHA256SUMS in step with that workflow.
cp "$binary" "$root/stage/$bin_name"
tar -czf "$release_dir/$archive" -C "$root/stage" "$bin_name"
if command -v sha256sum >/dev/null 2>&1; then
    digest=$(sha256sum "$release_dir/$archive" | cut -d' ' -f1)
else
    digest=$(shasum -a 256 "$release_dir/$archive" | cut -d' ' -f1)
fi
printf '%s  %s\n' "$digest" "$archive" >"$release_dir/SHA256SUMS"

# A second tree whose checksums do not match, to prove the installer refuses it.
cp "$release_dir/$archive" "$bad_dir/$archive"
printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "$archive" >"$bad_dir/SHA256SUMS"

# Newest tag first, without assets, so version resolution has to walk the list.
cat >"$api_dir/releases" <<JSON
[
  {
    "tag_name": "v0.0.0-not-published"
  },
  {
    "tag_name": "$tag"
  }
]
JSON

if is_windows; then
    candidates=(python python3)
else
    candidates=(python3 python)
fi
python_cmd=""
for candidate in "${candidates[@]}"; do
    # The Microsoft Store shim is on PATH but cannot serve anything.
    if command -v "$candidate" >/dev/null 2>&1 && "$candidate" -c 'import http.server' >/dev/null 2>&1; then
        python_cmd="$candidate"
        break
    fi
done
[ -n "$python_cmd" ] || {
    printf 'error: python is required to serve the test tree\n' >&2
    exit 2
}

"$python_cmd" -m http.server "$port" --directory "$(native_path "$root/www")" >"$root/server.log" 2>&1 &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null || true' EXIT

base="http://127.0.0.1:$port"
ready=0
attempt=0
while [ "$attempt" -lt 60 ]; do
    if curl -sf -o /dev/null "$base/"; then
        ready=1
        break
    fi
    attempt=$((attempt + 1))
    sleep 0.5
done
if [ "$ready" -ne 1 ]; then
    printf 'error: the local server did not start\n' >&2
    cat "$root/server.log" >&2
    exit 1
fi

# Runs the installer for this platform against the local tree.
run_installer() { # bin_dir release_base
    local bin_dir="$1"
    local release_base="$2"
    export RANDDB_RELEASE_BASE_URL="$release_base"
    export RANDDB_API_BASE_URL="$base/api"
    if is_windows; then
        local windows_home
        windows_home=$(native_path "$root/home")
        export RANDDB_HOME="$windows_home"
        local powershell=powershell
        if command -v powershell.exe >/dev/null 2>&1; then
            powershell=powershell.exe
        fi
        "$powershell" -NoProfile -ExecutionPolicy Bypass -File "$(native_path "$repo_root/install.ps1")" \
            -InstallDir "$(native_path "$bin_dir")"
    else
        export RANDDB_HOME="$root/home"
        sh "$repo_root/install.sh" --dir "$bin_dir"
    fi
}

echo "== install (version resolved from the API, then downloaded and verified) =="
mkdir -p "$root/bin"
set +e
output=$(run_installer "$root/bin" "$base" 2>&1)
status=$?
set -e
printf '%s\n' "$output" | sed 's/^/    | /'

check "installer exits 0" test "$status" -eq 0
check "reports the resolved tag" grep -qF "Installing RandDB $tag" <<<"$output"
check "installs $bin_name" test -f "$root/bin/$bin_name"
check "creates the configuration template" test -f "$root/home/.env"

set +e
installed_version=$("$root/bin/$bin_name" --version 2>&1)
version_status=$?
set -e
check "installed binary runs" test "$version_status" -eq 0
check "reports version $expect_version" grep -qF "$expect_version" <<<"$installed_version"

echo "== install (tampered checksums must abort) =="
mkdir -p "$root/bin-bad"
set +e
output=$(run_installer "$root/bin-bad" "$base/bad" 2>&1)
status=$?
set -e
printf '%s\n' "$output" | sed 's/^/    | /'

check "installer fails" test "$status" -ne 0
check "explains the mismatch" grep -qF 'checksum mismatch' <<<"$output"
check "installs nothing" test ! -e "$root/bin-bad/$bin_name"

if [ "$failures" -ne 0 ]; then
    printf '\n%d check(s) failed\n' "$failures" >&2
    exit 1
fi
printf '\nall installer checks passed\n'
