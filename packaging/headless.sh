#!/bin/sh
# Build the two static Linux binaries for a box: silli and silli-shim, no silli-gui and no desktop
# libraries. Install the Rust musl target and a musl C compiler first (bundled SQLite needs it).
#   packaging/headless.sh                     # native architecture's musl target
#   SILLI_TARGET=aarch64-unknown-linux-musl CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc packaging/headless.sh
set -eu
cd "$(dirname "$0")/.."
case "$(uname -m)" in
    x86_64) default_target=x86_64-unknown-linux-musl ;;
    aarch64|arm64) default_target=aarch64-unknown-linux-musl ;;
    *) echo 'Set SILLI_TARGET to a supported Linux musl target' >&2; exit 1 ;;
esac
silli_target=${SILLI_TARGET:-$default_target}
case "$silli_target" in *-unknown-linux-musl) ;; *) echo 'SILLI_TARGET must be a Linux musl target' >&2; exit 1 ;; esac
cargo build --release --target "$silli_target" -p silli -p silli-shim
mkdir -p "dist/headless/$silli_target"
install -m755 "target/$silli_target/release/silli" "target/$silli_target/release/silli-shim" "dist/headless/$silli_target/"
echo "dist/headless/$silli_target/silli"
echo "dist/headless/$silli_target/silli-shim"
