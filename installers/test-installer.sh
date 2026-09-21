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
#     difference between replacing the binary and failing to;
#   * a failure leaves the shell that ran it alive and unchanged;
#   * on Windows, an upgrade blocked by a client names that client, and
#     leaves it running.
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
    restore_user_path
    rm -rf "$work" 2>/dev/null || true
}

# On Windows the installer adds its directory to the *user* PATH in the
# registry, which is exactly what it should do for a real person — and exactly
# what a test must not leave behind. Every run used to add another temporary
# directory to the developer's own PATH, one per run, pointing at nothing once
# the run ended. So the value is saved before anything is installed and put
# back on the way out, however the run ends.
user_path_saved=""
save_user_path() {
    command -v cygpath >/dev/null 2>&1 || return 0
    user_path_saved="$work/user-path.saved"
    powershell -NoProfile -Command \
        "Set-Content -LiteralPath '$(cygpath -w "$user_path_saved")' -NoNewline -Value ([Environment]::GetEnvironmentVariable('Path','User'))" \
        >/dev/null 2>&1 || user_path_saved=""
}
restore_user_path() {
    [ -n "$user_path_saved" ] && [ -f "$user_path_saved" ] || return 0
    powershell -NoProfile -Command \
        "[Environment]::SetEnvironmentVariable('Path', [IO.File]::ReadAllText('$(cygpath -w "$user_path_saved")'), 'User')" \
        >/dev/null 2>&1 || true
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

if [ "$kind" != tar ]; then
    save_user_path
fi

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

# Run whichever installer belongs to this platform, into $work/install, and
# succeed exactly when it installed.
#
# install.ps1 never calls `exit` — under `irm | iex` that would close the
# user's terminal — so on Windows there is no exit code to read. It prints a
# fixed line when it fails instead, and that line is the signal here.
run_installer() {
    log="$1"
    if [ "$kind" = tar ]; then
        sh "$work/install.sh" > "$log" 2>&1
    else
        powershell -NoProfile -ExecutionPolicy Bypass -File "$(win_path "$work/install.ps1")" \
            > "$log" 2>&1
        if grep -q "MemFork was not installed" "$log"; then
            return 1
        fi
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

# ---- the installer cannot take the calling shell down with it -------------

# The documented way to run each installer puts it inside the caller's shell
# (iex on Windows) or could be misread as doing so (sourcing on Unix). Either
# way a failure must leave that shell alive and unchanged: an `exit` in an
# iex'd script closes the person's terminal, and a leaked `set -e` or error
# preference changes every command they type afterwards.
if [ "$kind" = tar ]; then
    sh -c '
        MEMFORK_DOWNLOAD_BASE=http://127.0.0.1:9
        export MEMFORK_DOWNLOAD_BASE
        . "$1"
        echo "SHELL-SURVIVED"
        false
        echo "SET-E-DID-NOT-LEAK"
    ' sh "$work/install.sh" > "$work/sourced.log" 2>&1 || true
    grep -q "SHELL-SURVIVED" "$work/sourced.log" ||
        { cat "$work/sourced.log"; fail "sourcing a failing install.sh ended the calling shell"; }
    grep -q "SET-E-DID-NOT-LEAK" "$work/sourced.log" ||
        { cat "$work/sourced.log"; fail "sourcing install.sh left set -e on in the calling shell"; }
    ok "a failing install.sh cannot end or change a shell that sources it"
else
    # Served from the stand-in release and piped to iex, exactly as the README
    # says to run it — through a download base that cannot answer, so it fails.
    # The caller is a script of its own rather than a -Command string: iex runs
    # in the scope of whatever calls it, so anything install.ps1 leaks lands in
    # this script's scope and the checks after it can see it.
    cp "$root/installers/install.ps1" "$release/install.ps1"
    cat > "$work/caller.ps1" <<PS1
\$env:MEMFORK_DOWNLOAD_BASE = 'http://127.0.0.1:9'
irm '$base/install.ps1' | iex
'HOST-SURVIVED'
'error preference: ' + \$ErrorActionPreference
'leaked function: ' + [bool](Get-Command Get-Target -ErrorAction SilentlyContinue)
PS1
    powershell -NoProfile -ExecutionPolicy Bypass -File "$(win_path "$work/caller.ps1")" \
        > "$work/iex.log" 2>&1 || true
    grep -q "HOST-SURVIVED" "$work/iex.log" ||
        { cat "$work/iex.log"; fail "install.ps1 ended the PowerShell host it was iex'd into"; }
    grep -q "MemFork was not installed" "$work/iex.log" ||
        { cat "$work/iex.log"; fail "install.ps1 did not say the install failed"; }
    grep -q "error preference: Continue" "$work/iex.log" ||
        { cat "$work/iex.log"; fail "install.ps1 changed the caller's error preference"; }
    grep -q "leaked function: False" "$work/iex.log" ||
        { cat "$work/iex.log"; fail "install.ps1 left its functions in the caller's session"; }
    ok "a failing install.ps1 leaves the PowerShell session it was iex'd into alive and unchanged"
fi

# ---- an upgrade blocked by a client that is using MemFork ------------------

# On Windows an MCP client holding a `memfork.exe mcp` child keeps the
# executable locked, and the installer must say which application that is —
# and must not close it. Tested with a real one: a stand-in client that starts
# `memfork mcp` from the installed binary and keeps it running.
if [ "$kind" != tar ]; then
    holder_script="$work/holder.py"
    cat > "$holder_script" <<'PY'
import subprocess, sys, time
child = subprocess.Popen([sys.argv[1], "mcp"], stdin=subprocess.PIPE,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
print(child.pid, flush=True)
time.sleep(600)
PY
    "$py" "$holder_script" "$(win_path "$installed")" > "$work/holder.out" 2>&1 &
    holder_pid=$!
    for _ in $(seq 1 50); do
        [ -s "$work/holder.out" ] && break
        sleep 0.2
    done
    [ -s "$work/holder.out" ] || fail "the stand-in client did not start memfork mcp"
    # The Windows pid of the stand-in client itself, which is what the
    # installer should name — not Git Bash's own numbering of it.
    client_winpid=$(powershell -NoProfile -Command \
        "(Get-CimInstance Win32_Process -Filter \"ProcessId = $(cat "$work/holder.out")\").ParentProcessId")
    client_winpid=$(printf '%s' "$client_winpid" | tr -d '\r\n ')
    ok "a stand-in client (pid $client_winpid) is holding memfork mcp"

    if run_installer "$work/blocked.log"; then
        fail "the installer replaced an executable that a client was running"
    fi
    grep -q "(pid $client_winpid) is using MemFork" "$work/blocked.log" ||
        { cat "$work/blocked.log"; fail "the installer did not name the client holding MemFork"; }
    grep -q "Close it, then run this installer again" "$work/blocked.log" ||
        { cat "$work/blocked.log"; fail "the installer did not say what to do"; }
    powershell -NoProfile -Command \
        "if (Get-Process -Id $client_winpid -ErrorAction SilentlyContinue) { 'alive' } else { 'gone' }" \
        > "$work/holder.state" 2>&1
    grep -q "alive" "$work/holder.state" ||
        fail "the installer closed the client application; it must never do that"
    ok "a blocked upgrade names the client, says what to do, and leaves it running"

    # Now let it go, and the same upgrade goes through.
    kill "$holder_pid" 2>/dev/null || true
    powershell -NoProfile -Command \
        "Stop-Process -Id $client_winpid -Force -ErrorAction SilentlyContinue; \
         Get-CimInstance Win32_Process -Filter \"Name = 'memfork.exe'\" |
           Where-Object { \$_.ParentProcessId -eq $client_winpid } |
           ForEach-Object { Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue }" \
        > /dev/null 2>&1 || true
    run_installer "$work/unblocked.log" ||
        { cat "$work/unblocked.log"; fail "the upgrade failed after the client let go"; }
    ok "once the client lets go, the upgrade goes through"
fi

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
