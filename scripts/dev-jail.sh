#!/bin/sh
# Reproducible, unprivileged Rust setup for Sill's Linux development jail.
# Usage: scripts/dev-jail.sh setup | doctor | COMMAND [ARG...]
set -eu
cd "$(dirname "$0")/.."
case "$(uname -s)-$(uname -m)" in
    Linux-aarch64) host=aarch64-unknown-linux-gnu ;;
    Linux-x86_64) host=x86_64-unknown-linux-gnu ;;
    *) echo 'This helper supports Linux arm64 and x86_64 jails.' >&2; exit 1 ;;
esac
# Deliberately isolated from the person's toolchains and shell profiles. The jail keeps
# ~/.cache across turns; none of this needs to be applied to the host through Changes.
dev_root=${SILL_DEV_ROOT:-${XDG_CACHE_HOME:-$HOME/.cache}/sill-dev}
case "$dev_root" in /*) ;; *) echo 'SILL_DEV_ROOT must be absolute.' >&2; exit 1 ;; esac
export CARGO_HOME="$dev_root/cargo" RUSTUP_HOME="$dev_root/rustup"
export PATH="$CARGO_HOME/bin:$PATH"
# the version rust-toolchain.toml pins: change both together
export RUSTUP_TOOLCHAIN=1.100.0
export CARGO_TARGET_DIR="$dev_root/target"
# The default Mac VM has 4 GB RAM. Avoid a full CPU-count build exhausting it.
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
case "${1:-doctor}" in
    setup)
        for cmd in curl sha256sum cc pkg-config; do
            command -v "$cmd" >/dev/null || { echo "Missing prerequisite: $cmd" >&2; exit 1; }
        done
        mkdir -p "$dev_root"
        if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
            installer=$(mktemp -d "$dev_root/install.XXXXXX")
            trap 'rm -rf "$installer"' EXIT HUP INT TERM
            url="https://static.rust-lang.org/rustup/dist/$host/rustup-init"
            curl --fail --show-error --location "$url" -o "$installer/rustup-init"
            curl --fail --show-error --location "$url.sha256" -o "$installer/rustup-init.sha256"
            (cd "$installer" && sha256sum -c rustup-init.sha256)
            chmod +x "$installer/rustup-init"
            "$installer/rustup-init" -y --no-modify-path --default-toolchain none
        fi
        rustup toolchain install "$RUSTUP_TOOLCHAIN" --profile minimal --component rustfmt --component clippy
        rustc --version
        cargo --version
        ;;
    doctor)
        for cmd in cc pkg-config git python3; do
            command -v "$cmd" || exit 1
        done
        [ -x "$CARGO_HOME/bin/rustc" ] || { echo 'Run scripts/dev-jail.sh setup first.' >&2; exit 1; }
        rustc --version
        cargo --version
        printf 'Build output: %s\n' "$CARGO_TARGET_DIR"
        python3 - <<'PYTHON'
import ctypes
for name, package in [("libXi.so.6", "libxi6"),
                      ("libxkbcommon-x11.so.0", "libxkbcommon-x11-0"),
                      ("libxcb-xkb.so.1", "libxcb-xkb1")]:
    try:
        ctypes.CDLL(name)
    except OSError:
        print(f"Optional X11 preview dependency missing: {package} ({name})")
PYTHON
        ;;
    *)
        [ -x "$CARGO_HOME/bin/cargo" ] || { echo 'Run scripts/dev-jail.sh setup first.' >&2; exit 1; }
        exec "$@"
        ;;
esac
