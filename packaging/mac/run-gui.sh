#!/bin/sh
# Build sill-gui on the Mac, sign it to run its VM (ad hoc: this Mac only) and start it. The VM's
# system image comes from dist/mac-image (packaging/mac-image.sh) until the app bundle carries it.
set -eu
cd "$(dirname "$0")/../.."
command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"
cargo build -q -p sill-gui "$@"
codesign --force --sign - --entitlements packaging/mac/sill-vm.entitlements target/debug/sill-gui 2>/dev/null
exec target/debug/sill-gui
