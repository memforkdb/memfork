#!/bin/sh
# MemFork installer for Linux and macOS.
#
#   curl -fsSL https://github.com/memforkdb/memfork/releases/latest/download/install.sh | sh
#
# Installs one binary into a directory you own. No root, no package manager,
# nothing outside your home directory. MEMFORK_VERSION pins a release,
# MEMFORK_INSTALL_DIR chooses where it goes, MEMFORK_TARGET picks a build
# other than this machine's, and MEMFORK_DOWNLOAD_BASE points somewhere other
# than GitHub. MEMFORK_GITHUB_BASE stands in for https://github.com itself, for
# a GitHub Enterprise mirror or the installer tests.
#
# Why this is not the installer `dist` generates: an older MemFork may be
# running a daemon over your memory, and it has to be stopped before its
# binary is replaced. A generated installer cannot run that step.
#
# Everything below runs in a subshell. Every instruction for this script pipes
# it to `sh`, which makes it a process of its own; but if it were ever sourced
# into a login shell instead, `set -eu` would change that shell for the rest of
# the session and the first `exit` would close it. Inside `( ... )`, neither can
# reach past the closing parenthesis.
(
set -eu

REPO="memforkdb/memfork"
GITHUB="${MEMFORK_GITHUB_BASE:-https://github.com}"
VERSION="${MEMFORK_VERSION:-latest}"
INSTALL_DIR="${MEMFORK_INSTALL_DIR:-$HOME/.memfork/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'memfork: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "this needs $1, which is not installed"
}

# ---- what to download -------------------------------------------------------

detect_target() {
    # An explicit choice wins: installing the x86-64 build on an Apple Silicon
    # Mac to run under Rosetta, say, or testing this script on a machine it
    # would otherwise refuse.
    if [ -n "${MEMFORK_TARGET:-}" ]; then
        echo "$MEMFORK_TARGET"
        return 0
    fi
    _t_os=$(uname -s)
    _t_arch=$(uname -m)
    case "$_t_os" in
        Linux)
            # musl, so one binary runs on every distribution regardless of
            # which libc it ships.
            case "$_t_arch" in
                x86_64|amd64) echo "x86_64-unknown-linux-musl" ;;
                aarch64|arm64) echo "aarch64-unknown-linux-musl" ;;
                *) die "unsupported architecture $_t_arch" ;;
            esac
            ;;
        Darwin)
            case "$_t_arch" in
                x86_64) echo "x86_64-apple-darwin" ;;
                arm64) echo "aarch64-apple-darwin" ;;
                *) die "unsupported architecture $_t_arch" ;;
            esac
            ;;
        *) die "unsupported system $_t_os; on Windows use install.ps1" ;;
    esac
}

# Turn "latest" into the one version it means right now, and use that exact
# version for every download that follows.
#
# GitHub's `releases/latest/download/<file>` is a redirect answered per
# request, and a release being published, or a stale edge cache, can answer
# two requests with two versions: an archive from one release and a checksum
# from another, or an older build than the release page shows. That happened
# on an earlier release. So the version is resolved once, from the redirect
# `releases/latest` sends, and never again.
resolve_latest() {
    # A HEAD request, not followed: the answer is the redirect's target, which
    # names the tag; the page it points at is never fetched.
    _r_final=$(curl -fsSI -o /dev/null -w '%{redirect_url}' "$GITHUB/$REPO/releases/latest") ||
        die "could not ask $GITHUB which release is the latest"
    _r_tag=${_r_final##*/tag/}
    case "$_r_tag" in
        v[0-9]*) ;;
        *) die "could not work out the latest release from $_r_final; set MEMFORK_VERSION to a release tag of the form vX.Y.Z" ;;
    esac
    printf '%s\n' "$_r_tag"
}

download_url() {
    # MEMFORK_DOWNLOAD_BASE points somewhere other than GitHub: a mirror, a
    # cache inside a network that cannot reach it, or the stand-in release the
    # installer tests serve from disk. Without it these tests would have to
    # rewrite the script, and then they would not be testing this script.
    if [ -n "${MEMFORK_DOWNLOAD_BASE:-}" ]; then
        printf '%s/%s\n' "${MEMFORK_DOWNLOAD_BASE%/}" "$1"
    else
        printf '%s/%s/releases/download/%s/%s\n' "$GITHUB" "$REPO" "$VERSION" "$1"
    fi
}

fetch() {
    # --fail so a 404 is an error rather than a file containing a 404 page.
    if show_progress; then
        curl -fL --progress-bar "$1" -o "$2" || die "could not download $1"
    else
        curl -fsSL "$1" -o "$2" || die "could not download $1"
    fi
}

# A progress bar is for a person watching a terminal: never into a pipe or a
# log, never in CI, and not when NO_COLOR asks for plain output.
# MEMFORK_PROGRESS=1 forces it, which is how the installer tests reach it.
show_progress() {
    [ "${MEMFORK_PROGRESS:-}" = 1 ] && return 0
    [ -t 2 ] && [ -z "${CI:-}" ] && [ -z "${NO_COLOR:-}" ]
}

# ---- checked, always --------------------------------------------------------

# The release publishes one `<archive>.sha256` beside each archive, holding
# `<hash> *<filename>`. Anything that is not an exact match stops the install:
# no checksum, no tool to check with, and a mismatch are all refusals.
#
# Every name in here is prefixed, because POSIX sh has no local variables: a
# plain `archive=` inside a function *is* the caller's `archive`. Not a style
# point — it is the bug this had, where checking the download renamed the file
# the installer then went on to unpack.
verify() {
    _v_archive="$1"
    _v_sums="$2"
    _v_name=$(basename "$_v_archive")

    _v_expected=$(cut -d' ' -f1 < "$_v_sums" | tr -d '\r\n')
    case "$_v_expected" in
        [0-9a-fA-F]*) ;;
        *) die "no usable checksum published for $_v_name; refusing to install" ;;
    esac

    if command -v sha256sum >/dev/null 2>&1; then
        _v_actual=$(sha256sum "$_v_archive" | cut -d' ' -f1)
    elif command -v shasum >/dev/null 2>&1; then
        _v_actual=$(shasum -a 256 "$_v_archive" | cut -d' ' -f1)
    else
        die "no sha256sum or shasum to check the download with; refusing to install"
    fi

    [ "$_v_actual" = "$_v_expected" ] ||
        die "checksum mismatch for $_v_name; refusing to install"
    say "Checksum verified (sha256 $_v_expected)."
}

# ---- replacing a running MemFork -------------------------------------------

stop_existing() {
    _s_existing="$1"
    [ -x "$_s_existing" ] || return 0
    say "Stopping the MemFork already installed here, so its daemon is not left"
    say "serving your memory from an old build."
    # Its failure is not ours: a MemFork with nothing running says so and
    # exits non-zero on some paths, and that is fine.
    "$_s_existing" stop >/dev/null 2>&1 || true
}

# ---- doing it ---------------------------------------------------------------

main() {
    need curl
    need tar

    target=$(detect_target)
    archive="memfork-$target.tar.xz"

    if [ -z "${MEMFORK_DOWNLOAD_BASE:-}" ] && [ "$VERSION" = latest ]; then
        VERSION=$(resolve_latest)
        say "Latest release is $VERSION."
    fi

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t memfork)
    trap 'rm -rf "$tmp"' EXIT INT TERM

    say "Downloading MemFork for $target..."
    fetch "$(download_url "$archive")" "$tmp/$archive"
    fetch "$(download_url "$archive.sha256")" "$tmp/$archive.sha256"
    verify "$tmp/$archive" "$tmp/$archive.sha256"

    # `-xf` rather than `-xzf`: the archives are xz, and both GNU tar and the
    # bsdtar macOS ships work out the compression for themselves.
    # GNU tar hands .xz to the `xz` program, which slim images often lack; say
    # that, rather than leaving tar's own complaint as the last word.
    tar -xf "$tmp/$archive" -C "$tmp" ||
        die "could not unpack $archive; unpacking .tar.xz needs xz support \
(install the xz or xz-utils package) and then run this again"
    binary=$(find "$tmp" -type f -name memfork | head -n 1)
    [ -n "$binary" ] || die "the archive did not contain a memfork binary"

    stop_existing "$INSTALL_DIR/memfork"

    mkdir -p "$INSTALL_DIR"
    # Into place in one step, so an interrupted install never leaves half a
    # binary where a working one was.
    cp "$binary" "$INSTALL_DIR/memfork.new"
    chmod 755 "$INSTALL_DIR/memfork.new"
    mv -f "$INSTALL_DIR/memfork.new" "$INSTALL_DIR/memfork"

    version=$("$INSTALL_DIR/memfork" --version 2>/dev/null || echo "memfork")
    say ""
    say "Installed $version"
    say "  $INSTALL_DIR/memfork"
    say ""

    on_path=no
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) on_path=yes ;;
    esac

    if [ "$on_path" = no ]; then
        # A shell cannot change its parent's environment, and this script is a
        # child of the shell that ran it: there is no way to put the directory
        # on the path from here. So print the exact line, ready to paste,
        # rather than a description of it.
        say "Add it to your PATH by running this now:"
        say ""
        say "  export PATH=\"\$PATH:$INSTALL_DIR\""
        say ""
        say "and adding the same line to $(profile_file) to keep it."
        say ""
    fi

    say "Next:"
    say "  memfork init      register MemFork with the MCP clients you have"
    say "  memfork doctor    check what is installed and what is talking to it"
    say ""
    say "Shell completions: memfork completions bash|zsh|fish, to source or install."
    say ""
    say "To uninstall:"
    say "  memfork stop"
    say "  rm -rf \"$INSTALL_DIR\""
    if [ "$on_path" = no ]; then
        say "  and remove the PATH line above from $(profile_file)"
    else
        say "  and remove $INSTALL_DIR from PATH in your shell's startup file"
    fi
    say ""
    say "That leaves your stored memory, which lives in the data directory"
    say "\`memfork doctor\` prints. Delete that too if you want it gone."
}

# The startup file for the shell that is running, as a name to put in a
# sentence. A guess is fine here — it is printed, never edited.
#
# shellcheck disable=SC2088  # the tildes are read by a person, not by a shell
profile_file() {
    case "${SHELL:-}" in
        */zsh) printf '~/.zshrc\n' ;;
        */fish) printf '~/.config/fish/config.fish\n' ;;
        */bash) printf '~/.bashrc\n' ;;
        *) printf "your shell's startup file\n" ;;
    esac
}

main "$@"
)
