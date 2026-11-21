#!/bin/sh
# Build silli-gui on the Mac, sign it to run its VM (this Mac only: packaging/mac/signing-identity.sh's
# certificate when there is one, else ad hoc) and start it. The VM's
# system image comes from dist/mac-image (packaging/mac-image.sh) until the app bundle carries it.
set -eu
cd "$(dirname "$0")/../.."
command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"
cargo build -q -p silli-gui "$@"
identity=-
security find-identity -v -p codesigning 2>/dev/null | grep -q '"Silli local development"' && identity="Silli local development"
codesign --force --sign "$identity" --entitlements packaging/mac/silli-vm.entitlements target/debug/silli-gui 2>/dev/null
exec target/debug/silli-gui
