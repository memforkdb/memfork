#!/usr/bin/env bash
# Run MemFork's whole command surface where nothing can reach the outside, and
# prove that the block holds.
#
# The `no-network` workflow runs this on each operating system with outbound
# traffic blocked at the firewall — a network namespace on Linux, a pf rule on
# macOS, program rules on Windows. Everything below must then work exactly as
# it does with a network, because MemFork itself never uses one: the daemon
# listens on loopback, `memfork mcp` and the command line talk to it there,
# and the installers download from wherever MEMFORK_DOWNLOAD_BASE points. The
# only network connection anywhere here is the control at the end, which must
# fail, or the block was never in force and the run proves nothing.
#
# It needs a release binary (MEMFORK_BIN, default target/release/memfork) and
# python, for the stand-in release server the installer harness starts.
#
# Runs under bash on all three operating systems (Git Bash on Windows).

set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) exe="memfork.exe" ;;
    *) exe="memfork" ;;
esac
bin="${MEMFORK_BIN:-$root/target/release/$exe}"
[ -x "$bin" ] || { echo "not ok: no binary at $bin; build it first" >&2; exit 1; }

work="${MEMFORK_TEST_WORK:-$(mktemp -d)}"
mkdir -p "$work/data" "$work/home" "$work/repo/.git"
export MEMFORK_DATA_DIR="$work/data"
export MEMFORK_FORBID_PER_USER_DATA_DIR=1
export MEMFORK_HOME="$work/home"
unset MEMFORK_NAMESPACE

fail() {
    printf 'not ok: %s\n' "$*" >&2
    # What the last command said, when it said anything.
    if [ -s "$work/last.err" ]; then sed 's/^/    | /' "$work/last.err" >&2; fi
    "$bin" stop >/dev/null 2>&1 || true
    exit 1
}
ok() { printf 'ok: %s\n' "$*"; }

cd "$work/repo"

# ---- the command line, alone and through the daemon --------------------------

"$bin" --version >/dev/null || fail "--version"
"$bin" doctor --verbose >/dev/null || fail "doctor"
"$bin" init --dry-run >/dev/null || fail "init --dry-run"
"$bin" completions bash >/dev/null || fail "completions"
"$bin" --ephemeral put k v >/dev/null || fail "an ephemeral put"
ok "the command line works alone"

# These start the daemon, which binds loopback, and talk to it there.
"$bin" put repo:decision:store '{"why":"kept on this machine"}' >/dev/null 2>"$work/last.err" || fail "put through the daemon"
"$bin" fork attempt >/dev/null || fail "fork"
"$bin" put --branch attempt repo:note:1 "on the fork" >/dev/null || fail "put on a branch"
"$bin" merge attempt >/dev/null || fail "merge"
"$bin" fork abandoned >/dev/null || fail "fork again"
"$bin" discard abandoned --lesson "the block held" >/dev/null || fail "discard with a lesson"
"$bin" branches >/dev/null || fail "branches"
"$bin" log --graph >/dev/null || fail "log --graph"
"$bin" find decision >/dev/null || fail "find"
"$bin" lessons >/dev/null || fail "lessons"
"$bin" task add "prove nothing reaches out" --id t1 >/dev/null || fail "task add"
"$bin" task claim t1 >/dev/null || fail "task claim"
"$bin" task "done" t1 >/dev/null || fail "task done"
"$bin" stats >/dev/null || fail "stats"
"$bin" flags >/dev/null || fail "flags"
"$bin" --json doctor | grep -q '"port"' || fail "doctor did not see the daemon"
ok "the command line works through the daemon, on loopback"

# The event stream: one line, then exit.
"$bin" watch --json --count 1 > "$work/watch.out" 2>/dev/null &
watch_pid=$!
sleep 1
"$bin" put repo:decision:watched "yes" >/dev/null || fail "put while watching"
wait "$watch_pid" || fail "watch did not exit after one event"
grep -q '"schema":1' "$work/watch.out" || fail "watch printed no event: $(cat "$work/watch.out")"
ok "watch reads the event stream"

# ---- memfork mcp, as a client drives it --------------------------------------

mcp_in=$(printf '%s\n%s\n%s\n%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"no-network-check","version":"1"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memfork_resume","arguments":{}}}')
printf '%s\n' "$mcp_in" | "$bin" mcp > "$work/mcp.out" 2>/dev/null || true
grep -q '"memfork_put"' "$work/mcp.out" || fail "memfork mcp did not list its tools: $(head -c 400 "$work/mcp.out")"
grep -q '"id":3' "$work/mcp.out" || fail "memfork mcp did not answer a tool call: $(head -c 400 "$work/mcp.out")"
ok "memfork mcp shakes hands and answers a tool call"

"$bin" stop >/dev/null || fail "stop"
ok "the daemon stopped"

# ---- the installers, from a release served on loopback ----------------------

# The installer harness builds a stand-in release from the binary and serves
# it from 127.0.0.1; the installers are pointed at it with
# MEMFORK_DOWNLOAD_BASE and MEMFORK_GITHUB_BASE and never see GitHub.
MEMFORK_TEST_WORK="$work/installer" bash "$root/installers/test-installer.sh" > "$work/installer.log" 2>&1 ||
    { cat "$work/installer.log"; fail "the installer harness failed with the network blocked"; }
ok "the installers work from a release served on loopback"

# ---- the control: the outside must be unreachable -----------------------------

# If this succeeds, the block was not in force and nothing above proved
# anything. `--max-time` so a black hole fails fast rather than hanging.
if curl -sS --max-time 10 -o /dev/null https://github.com/ 2>/dev/null; then
    fail "the outside is reachable: the network block is not in force"
fi
ok "the outside is unreachable, so everything above ran without it"

printf '\nall no-network checks passed\n'
