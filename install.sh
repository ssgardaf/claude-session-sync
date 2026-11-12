#!/bin/sh
# claude-session-sync installer / updater for macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/ssgardaf/claude-session-sync/main/install.sh | sh
#
# Downloads the latest release for this system, checks its SHA-256, and runs
# "claude-session-sync install". Run it again at any time to update: your settings are kept.
# To set options (for example --delete-sync), pass them to the install:
#
#   curl -fsSL https://raw.githubusercontent.com/ssgardaf/claude-session-sync/main/install.sh | sh -s -- --delete-sync
#
# Nothing here needs root, and no shell profile is changed. (Windows: use install.ps1.)
#
# Everything is inside one function that is called on the last line, so that a download of
# this script that was cut off runs nothing at all.

set -eu

fail() {
    echo "claude-session-sync: $*" >&2
    exit 1
}

main() {
    repo='ssgardaf/claude-session-sync'

    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Darwin)
            asset='claude-session-sync-macos-universal.tar.gz' ;;
        Linux)
            case "$arch" in
                x86_64 | amd64) asset='claude-session-sync-linux-x64.tar.gz' ;;
                aarch64 | arm64) asset='claude-session-sync-linux-arm64.tar.gz' ;;
                *) fail "there is no build for Linux on $arch. To build it yourself: https://github.com/$repo#build-from-source" ;;
            esac ;;
        *)
            fail "$os is not supported (macOS and Linux here; Windows has install.ps1)." ;;
    esac

    # The daemon is the user's own: its files and its autostart belong to the account that
    # uses the Claude app.
    if [ "$(id -u)" = 0 ]; then
        fail 'run this as yourself, not as root (no sudo): it installs for the user who runs it.'
    fi

    for tool in curl tar awk mktemp; do
        command -v "$tool" >/dev/null 2>&1 || fail "'$tool' is needed and was not found."
    done
    if command -v shasum >/dev/null 2>&1; then
        sha256='shasum -a 256'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256='sha256sum'
    else
        fail "neither 'shasum' nor 'sha256sum' was found: the download cannot be checked."
    fi

    base="https://github.com/$repo/releases/latest/download"
    work=$(mktemp -d "${TMPDIR:-/tmp}/claude-session-sync.XXXXXXXX")
    trap 'rm -rf "$work"' EXIT
    trap 'exit 1' HUP INT TERM

    echo "Downloading $asset ..."
    curl -fsSL --proto '=https' --tlsv1.2 -o "$work/$asset" "$base/$asset" ||
        fail "the download failed ($base/$asset). Nothing was installed."
    curl -fsSL --proto '=https' --tlsv1.2 -o "$work/$asset.sha256" "$base/$asset.sha256" ||
        fail "the checksum could not be downloaded. Nothing was installed."

    # Guards against a damaged or cut-off download. It is not a signature: the checksum
    # comes from the same release as the file.
    want=$(awk '{ print tolower($1); exit }' "$work/$asset.sha256")
    have=$(cd "$work" && $sha256 "$asset" | awk '{ print tolower($1); exit }')
    if [ -z "$want" ] || [ "$want" != "$have" ]; then
        fail "checksum mismatch: expected $want, got $have. Nothing was installed."
    fi

    mkdir "$work/x"
    tar -xzf "$work/$asset" -C "$work/x" || fail 'the download could not be unpacked. Nothing was installed.'
    exe="$work/x/claude-session-sync"
    [ -f "$exe" ] || fail 'the download does not contain claude-session-sync. Nothing was installed.'
    chmod +x "$exe"

    version=$("$exe" --version </dev/null) ||
        fail "the downloaded program does not run here. (If $work is on a file system that forbids running programs, set TMPDIR to a folder that allows it and try again.) Nothing was installed."
    echo "Installing claude-session-sync $version ..."
    # (its input is not this script: nothing of the script is left for it to read)
    "$exe" install "$@" </dev/null || fail "install failed (exit code $?)."

    echo
    if [ "$os" = Darwin ]; then
        echo 'Next: quit the Claude app completely (Cmd+Q) and open it again.'
    else
        echo 'Next: quit the Claude app completely and open it again.'
    fi
    echo 'Then check on it any time with:  claude-session-sync status'
}

main "$@"
