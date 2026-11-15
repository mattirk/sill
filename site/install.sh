#!/bin/sh
# sill installer — https://github.com/mattirk/sill
#
#   curl -fsSL https://mattirk.github.io/sill/install.sh | sh
#
# Finds the latest release on GitHub, asks what to install, verifies the download against the
# release's SHA256SUMS and installs it. Nothing is changed before you answer. To read it first:
#   curl -fsSL https://mattirk.github.io/sill/install.sh | less
#
# Environment: SILL_VERSION=0.1.0 (default: the latest release),
#              SILL_DRY_RUN=1 (print the plan, download and change nothing).
set -eu

REPO=mattirk/sill
RELEASES="https://github.com/$REPO/releases"

main() {
    os=$(uname -s); arch=$(uname -m)
    case "$arch" in
        x86_64|amd64) arch=x86_64; deb_arch=amd64 ;;
        aarch64|arm64) arch=aarch64; deb_arch=arm64 ;;
        *) die "unsupported architecture: $arch" ;;
    esac
    command -v curl >/dev/null 2>&1 || die "curl is required"
    ver=${SILL_VERSION:-$(latest)}
    [ -n "$ver" ] || die "no published release found; see $RELEASES"
    ver=${ver#v}
    # only what a version is made of goes into file names and paths
    case "$ver" in *[!0-9A-Za-z.+-]*) die "not a version: $ver" ;; esac
    base="$RELEASES/download/v$ver"
    say "sill $ver for $os $arch"

    case "$os" in
    Darwin)
        [ "$arch" = aarch64 ] || die "the Mac app needs Apple Silicon"
        file="sill-$ver-macos-arm64.dmg"
        choose "Install Sill.app into /Applications (replaces an installed one)" \
               "Only download $file to ~/Downloads" \
               "Cancel"
        case "$choice" in
            1) fetch
               run hdiutil attach -nobrowse -readonly -quiet -mountpoint "$tmp/mnt" "$tmp/$file"
               mounted=$tmp/mnt
               [ -n "${SILL_DRY_RUN:-}" ] || [ -d "$tmp/mnt/Sill.app" ] || die "$file holds no Sill.app; nothing installed"
               # the new app is whole beside the old one before the old one goes
               run rm -rf /Applications/Sill.app.new
               run cp -R "$tmp/mnt/Sill.app" /Applications/Sill.app.new
               run rm -rf /Applications/Sill.app
               run mv /Applications/Sill.app.new /Applications/Sill.app
               run hdiutil detach -quiet "$tmp/mnt"
               mounted=
               say "Installed: open Sill from Applications."
               say "The app is not notarized. If macOS refuses to open it, allow it under"
               say "System Settings → Privacy & Security → Open Anyway." ;;
            2) fetch
               run mv "$tmp/$file" "$HOME/Downloads/"
               say "Saved as ~/Downloads/$file" ;;
            *) say "Nothing changed." ;;
        esac ;;
    Linux)
        [ "$arch" = x86_64 ] || die "releases have no Linux package for $arch yet; build from source: https://github.com/$REPO#build-from-source"
        if command -v apt-get >/dev/null 2>&1; then
            choose "Debian package (apt-get install)" \
                   "Tarball into /usr/local (sudo ./sill init)" \
                   "Only show what would be done" \
                   "Cancel"
        else
            choose "Tarball into /usr/local (sudo ./sill init)" \
                   "Only show what would be done" \
                   "Cancel"
            choice=$((choice + 1))
        fi
        if [ "$choice" = 3 ]; then SILL_DRY_RUN=1; choice=2; fi
        case "$choice" in
            1) file="sill_${ver}_${deb_arch}.deb"; fetch
               run sudo apt-get install -y "$tmp/$file" ;;
            2) file="sill-$ver-$arch.tar.gz"; fetch
               run tar -xzf "$tmp/$file" -C "$tmp"
               run sudo "$tmp/sill-$ver-$arch/sill" init ;;
            *) say "Nothing changed."; return 0 ;;
        esac
        if ask "Start the sill window with your desktop session (sill init --user)?"; then
            run sill init --user
        fi
        say "Done. Show the window with: sill toggle" ;;
    *) die "unsupported system: $os" ;;
    esac
}

say() { printf '%s\n' "$*"; }
die() { printf 'sill install: %s\n' "$*" >&2; exit 1; }
run() {
    if [ -n "${SILL_DRY_RUN:-}" ]; then say "  would run: $*"; else say "  $*"; "$@"; fi
}

# The latest published release, from where github.com sends /releases/latest (…/releases/tag/vX.Y.Z).
latest() {
    url=$(curl -fsSLI --proto '=https' --proto-redir '=https' -o /dev/null -w '%{url_effective}' "$RELEASES/latest") || return 0
    case "$url" in */releases/tag/v*) printf '%s\n' "${url##*/releases/tag/v}" ;; esac
}

# Answers come from the terminal, not from the pipe this script arrives on.
tty_read() {
    ( : </dev/tty ) 2>/dev/null || die "no terminal to ask on; download the script and run it in one"
    read -r REPLY </dev/tty || REPLY=
}
# choose OPTION…: sets $choice to the number picked; anything that is not one of them is the last (Cancel).
choose() {
    n=0
    for o in "$@"; do n=$((n + 1)); say "  $n) $o"; done
    printf 'Choose [1-%s]: ' "$n"
    tty_read
    case "$REPLY" in
        ''|*[!0-9]*) choice=$n ;;
        *) if [ "$REPLY" -ge 1 ] && [ "$REPLY" -le "$n" ]; then choice=$REPLY; else choice=$n; fi ;;
    esac
}
ask() {
    printf '%s [Y/n] ' "$1"
    tty_read
    case "$REPLY" in n*|N*) return 1 ;; esac
}

# fetch: $base/$file into $tmp, verified against the release's SHA256SUMS.
fetch() {
    tmp=$(mktemp -d)
    mounted=
    # a mounted image is let go before its folder is removed
    trap '[ -z "$mounted" ] || hdiutil detach -quiet "$mounted" 2>/dev/null || true; rm -rf "$tmp"' EXIT
    if [ -n "${SILL_DRY_RUN:-}" ]; then say "  would download and verify: $base/$file"; return 0; fi
    say "  downloading $base/$file"
    curl -fL --proto '=https' --proto-redir '=https' --progress-bar -o "$tmp/$file" "$base/$file" || die "download failed: $base/$file"
    curl -fsSL --proto '=https' --proto-redir '=https' -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
    want=$(awk -v f="$file" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
    if command -v sha256sum >/dev/null 2>&1; then
        got=$(sha256sum "$tmp/$file" | awk '{ print $1 }')
    else
        got=$(shasum -a 256 "$tmp/$file" | awk '{ print $1 }')
    fi
    [ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $file; nothing installed"
    say "  checksum ok"
}

# The whole script is one function called on its last line: a download cut short runs nothing.
main "$@"
