#!/bin/sh
# Native ARM64 Debian builds for the QEMU image path; no compilation under emulation.
# Outputs target/mac-prebuilt/{sill,sill-shim,commit} and the shared Weston cache.
set -eu
cd "$(dirname "$0")/../.."
[ "$(uname -s)-$(uname -m)" = Linux-aarch64 ] || { echo 'Requires ARM64 Linux and Docker.' >&2; exit 1; }
cache=${SILL_IMAGE_CACHE:-$HOME/.cache/sill/image}
mkdir -p "$cache" target/mac-prebuilt
cache=$(cd "$cache" && pwd)
key=$(shasum -a 256 packaging/mac/guest/weston-build.sh | cut -c1-16)
# Only committed source, just as the original on-Mac guest build uses.
git archive --format=tar -o target/mac-prebuilt/source.tar HEAD
docker run --rm --platform linux/arm64 \
    -v "$PWD/target/mac-prebuilt:/out" -v "$cache:/cache" \
    -e "WESTON_KEY=$key" -e "BUILD_UID=$(id -u)" -e "BUILD_GID=$(id -g)" \
    debian:13 sh -eu -c '
        apt-get update -qq
        apt-get install -y --no-install-recommends ca-certificates curl build-essential pkg-config git xz-utils
        mkdir /src
        tar -xf /out/source.tar -C /src
        cd /src
        curl -fsSL https://sh.rustup.rs -o /tmp/rustup.sh
        sh /tmp/rustup.sh -y --profile minimal --no-modify-path
        . "$HOME/.cargo/env"
        cargo build --locked --release -p sill -p sill-shim
        cp target/release/sill target/release/sill-shim /out/
        weston=/cache/weston-$WESTON_KEY
        if [ ! -f "$weston/weston.tar.gz" ] || [ ! -f "$weston/runtime-packages" ]; then
            rm -rf "$weston.new"
            sh packaging/mac/guest/weston-build.sh "$weston.new"
            rm -rf "$weston"
            mv "$weston.new" "$weston"
        fi
        chown -R "$BUILD_UID:$BUILD_GID" /out "$weston"
    '
git rev-parse HEAD > target/mac-prebuilt/commit
rm target/mac-prebuilt/source.tar
