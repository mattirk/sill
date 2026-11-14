#!/bin/sh
# Build the two static Linux binaries for a box. No GTK, X11, VTE or desktop libraries.
# Install the Rust musl target and a musl C compiler first (bundled SQLite needs it).
#   packaging/headless.sh                     # native architecture's musl target
#   SILL_TARGET=aarch64-unknown-linux-musl CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc packaging/headless.sh
set -eu
cd "$(dirname "$0")/.."
case "$(uname -m)" in
    x86_64) default_target=x86_64-unknown-linux-musl ;;
    aarch64|arm64) default_target=aarch64-unknown-linux-musl ;;
    *) echo 'Set SILL_TARGET to a supported Linux musl target' >&2; exit 1 ;;
esac
sill_target=${SILL_TARGET:-$default_target}
case "$sill_target" in *-unknown-linux-musl) ;; *) echo 'SILL_TARGET must be a Linux musl target' >&2; exit 1 ;; esac
cargo build --release --target "$sill_target" --no-default-features -p sill-shelf -p sill-shim
mkdir -p "dist/headless/$sill_target"
install -m755 "target/$sill_target/release/sill" "target/$sill_target/release/sill-shim" "dist/headless/$sill_target/"
echo "dist/headless/$sill_target/sill"
echo "dist/headless/$sill_target/sill-shim"
