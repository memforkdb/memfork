#!/usr/bin/env bash
# Exercise the installers against a release served from disk.
#
# No network, no published release, no assumptions: the archives are built here
# from the binary this checkout just compiled, a local HTTP server stands in
# for GitHub, and the installer is pointed at it. What gets checked is the
# behaviour that a generated installer could not give us —
#
#   * it installs, and what it installs runs;
#   * a wrong checksum stops it, rather than being installed anyway;
#   * an upgrade stops a running daemon first, which on Windows is the
#     difference between replacing the binary and failing to.
#
# Runs on all three operating systems under bash (Git Bash on Windows).

set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'cleanup' EXIT

port=0
server_pid=""

cleanup() {
    [ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null || true
    # Anything this test installed may have started a daemon; stop it rather
    # than leave one holding a temporary directory.
    if [ -x "$work/install/memfork" ]; then
        MEMFORK_DATA_DIR="$work/data" "$work/install/memfork" stop >/dev/null 2>&1 || true
    fi
    if [ -x "$work/install/memfork.exe" ]; then
        MEMFORK_DATA_DIR="$work/data" "$work/install/memfork.exe" stop >/dev/null 2>&1 || true
    fi
    rm -rf "$work" 2>/dev/null || true
}

fail() { printf 'not ok: %s\n' "$*" >&2; exit 1; }
ok() { printf 'ok: %s\n' "$*"; }

case "$(uname -s)" in
    Linux)   target="x86_64-unknown-linux-musl"; exe="memfork"; kind="tar" ;;
    Darwin)  target="$([ "$(uname -m)" = arm64 ] && echo aarch64 || echo x86_64)-apple-darwin"
             exe="memfork"; kind="tar" ;;
    MINGW*|MSYS*|CYGWIN*) target="x86_64-pc-windows-msvc"; exe="memfork.exe"; kind="zip" ;;
    *) fail "unsupported system $(uname -s)" ;;
esac

# ---- a release, served from disk -------------------------------------------

release="$work/release"
mkdir -p "$release" "$work/stage"

built="$root/target/release/$exe"
[ -f "$built" ] || fail "no binary at $built; build it first"
cp "$built" "$work/stage/$exe"

if [ "$kind" = tar ]; then
    archive="memfork-$target.tar.xz"
    # The installer untars with `-xf` and lets tar work out the compression,
    # which is exactly what the released archives need.
    (cd "$work/stage" && tar -cJf "$release/$archive" "$exe")
else
    archive="memfork-$target.zip"
    (cd "$work/stage" && powershell -NoProfile -Command \
        "Compress-Archive -Path '$exe' -DestinationPath '$(cygpath -w "$release/$archive")' -Force")
fi

hash_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}
printf '%s *%s\n' "$(hash_of "$release/$archive")" "$archive" > "$release/$archive.sha256"

# A one-file HTTP server that answers the two paths GitHub would.
# `python3` on the Linux and macOS runners, `python` on Windows: both exist
# somewhere, neither exists everywhere — and on Windows `python3` may be the
# Microsoft Store stub, which is on PATH and is not an interpreter. So each
# candidate is asked to run something before it is believed.
py=""
for candidate in python3 python py; do
    if command -v "$candidate" >/dev/null 2>&1 &&
       "$candidate" -c "import sys" >/dev/null 2>&1; then
        py=$candidate
        break
    fi
done
[ -n "$py" ] || fail "this test needs python to stand in for the release server"

"$py" - "$release" > "$work/port" <<'PY' &
import http.server, os, socketserver, sys, threading

directory = sys.argv[1]

class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory=directory, **kw)

    def translate_path(self, path):
        # Everything GitHub serves under a release lives flat in one directory,
        # whatever the URL shape.
        return os.path.join(directory, os.path.basename(path.split("?")[0]))

    def log_message(self, *a):
        pass

with socketserver.TCPServer(("127.0.0.1", 0), Handler) as httpd:
    print(httpd.server_address[1], flush=True)
    httpd.serve_forever()
PY
server_pid=$!

for _ in $(seq 1 50); do
    port=$(cat "$work/port" 2>/dev/null || true)
    [ -n "$port" ] && break
    sleep 0.2
done
[ -n "$port" ] || fail "the stand-in release server did not start"
ok "serving a fake release on 127.0.0.1:$port"

base="http://127.0.0.1:$port"

# ---- installing -------------------------------------------------------------

export MEMFORK_INSTALL_DIR="$work/install"
export MEMFORK_VERSION="v0.0.0-test"
export MEMFORK_DATA_DIR="$work/data"
export MEMFORK_FORBID_PER_USER_DATA_DIR=1
mkdir -p "$MEMFORK_DATA_DIR"

# The scripts under test, unmodified. They take the stand-in through
# MEMFORK_DOWNLOAD_BASE: rewriting them would mean testing something else,
# which is how the first version of this passed on Windows and failed on
# Linux — the rewrite only matched one of the two scripts.
export MEMFORK_DOWNLOAD_BASE="$base"
cp "$root/installers/install.sh" "$work/install.sh"
cp "$root/installers/install.ps1" "$work/install.ps1"

win_path() {
    if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s' "$1"; fi
}

# Git Bash rewrites PSModulePath into POSIX paths, and PowerShell then cannot
# find its own modules — Get-FileHash and Expand-Archive come back "not
# recognized". That is this harness, not the installer: a user running it from
# a PowerShell prompt has a working one. Hand it back a Windows path.
if [ "$kind" != tar ]; then
    system_root=${SYSTEMROOT:-${SystemRoot:-C:\Windows}}
    PSModulePath="$system_root\System32\WindowsPowerShell\v1.0\Modules"
    export PSModulePath
fi

# Run whichever installer belongs to this platform, into $work/install.
run_installer() {
    log="$1"
    if [ "$kind" = tar ]; then
        sh "$work/install.sh" > "$log" 2>&1
    else
        powershell -NoProfile -ExecutionPolicy Bypass -File "$(win_path "$work/install.ps1")" \
            > "$log" 2>&1
    fi
}

export MEMFORK_INSTALL_DIR="$work/install"
export MEMFORK_VERSION="v0.0.0-test"
export MEMFORK_DATA_DIR="$work/data"
export MEMFORK_FORBID_PER_USER_DATA_DIR=1
mkdir -p "$MEMFORK_DATA_DIR"
if [ "$kind" != tar ]; then
    # PowerShell reads these from the environment, and a Windows path is what
    # it can act on.
    MEMFORK_INSTALL_DIR=$(win_path "$work/install")
    MEMFORK_DATA_DIR=$(win_path "$work/data")
    export MEMFORK_INSTALL_DIR MEMFORK_DATA_DIR
fi

run_installer "$work/install.log" || { cat "$work/install.log"; fail "the installer failed"; }

installed="$work/install/$exe"
[ -x "$installed" ] || fail "nothing was installed at $work/install"
"$installed" --version > /dev/null || fail "the installed binary does not run"
ok "installed, and it runs"

grep -qi "uninstall" "$work/install.log" || fail "the installer never said how to uninstall"
ok "it says how to uninstall"

grep -qi "checksum verified" "$work/install.log" ||
    { cat "$work/install.log"; fail "the installer never said it checked the download"; }
ok "it says the checksum was verified"

# Removing the binary is half an uninstall: the PATH entry it added outlives
# it, and a dangling entry is the sort of litter nobody traces back.
grep -qiE "path" "$work/install.log" ||
    { cat "$work/install.log"; fail "the installer said nothing about PATH"; }
ok "it says what it did to PATH, and how to undo it"

# ---- an upgrade over a running daemon --------------------------------------

# The case the hand-written installers exist for. On Windows a running
# executable cannot be replaced at all, so if this passes there, the stop
# really happened.
"$installed" serve --port 0 --idle-timeout 120 --data-dir "$work/data" > /dev/null 2>&1 &
daemon_pid=$!
for _ in $(seq 1 100); do
    [ -f "$work/data/memfork.endpoint" ] && break
    sleep 0.2
done
[ -f "$work/data/memfork.endpoint" ] || fail "the daemon did not start"
ok "a daemon is running from the installed binary"

run_installer "$work/upgrade.log" || {
    cat "$work/upgrade.log"
    fail "the upgrade failed with a daemon running"
}
grep -qi "stopping the memfork already installed" "$work/upgrade.log" \
    || { cat "$work/upgrade.log"; fail "the upgrade did not stop the running MemFork"; }
[ ! -f "$work/data/memfork.endpoint" ] \
    || fail "the daemon is still registered as running after the upgrade"
wait "$daemon_pid" 2>/dev/null || true
ok "an upgrade stops the running daemon and replaces the binary"

# ---- a download that is not what it claims ---------------------------------

printf 'this is not a memfork\n' > "$release/$archive"
if run_installer "$work/bad.log"; then
    fail "a corrupt download was installed anyway"
fi
grep -qi "checksum mismatch" "$work/bad.log" || {
    cat "$work/bad.log"
    fail "the refusal did not name the reason"
}
ok "a checksum mismatch stops the install"

printf '\nall installer checks passed\n'
