#!/bin/sh
# Build the release tarball for this machine's architecture (SPEC §4.12 "Channels"):
#   sill-<ver>-<arch>.tar.gz   the three binaries, the built-in policy, icon, desktop file, units, README
#   SHA256SUMS                 over the tarball
# Output in dist/. Signing (minisign) is a separate, later step and is not done here.
#
#   packaging/release.sh            # cargo build --release, then package
#   packaging/release.sh --no-build # package what target/release already has
set -eu
cd "$(dirname "$0")/.."
VER=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(uname -m)
case "$ARCH" in x86_64) ARCH=x86_64 ;; aarch64|arm64) ARCH=aarch64 ;; esac
NAME="sill-$VER-$ARCH"
if [ "${1:-}" != "--no-build" ]; then
    cargo build --release
fi
for b in sill sill-shim sill-gui; do
    [ -x "target/release/$b" ] || { echo "target/release/$b missing" >&2; exit 1; }
    # the desktop libraries (Wayland, xkbcommon, X11, Vulkan) are dlopen'd at run time; a binary that
    # links more than libc is a stale build from before the GTK removal
    extra=$(ldd "target/release/$b" | awk '{print $1}' | grep -v -e linux-vdso -e ld-linux -e '^libc\.so' -e '^libm\.so' -e '^libgcc_s' || true)
    [ -z "$extra" ] || { echo "target/release/$b links $(echo $extra): stale build, run without --no-build" >&2; exit 1; }
done
rm -rf "dist/$NAME"
mkdir -p "dist/$NAME/policies" "dist/$NAME/systemd/user" "dist/$NAME/icons"
install -m 755 target/release/sill target/release/sill-shim target/release/sill-gui "dist/$NAME/"
install -m 644 policies/default.toml "dist/$NAME/policies/"
install -m 644 packaging/sill.desktop "dist/$NAME/"
install -m 644 packaging/icons/sill.svg "dist/$NAME/icons/"
install -m 644 packaging/systemd/*.mount packaging/systemd/*.service "dist/$NAME/systemd/"
install -m 644 packaging/systemd/user/sill-gui.service "dist/$NAME/systemd/user/"
install -m 644 README.md "dist/$NAME/"
cat > "dist/$NAME/INSTALL" <<'TXT'
sill — install from this tarball

    sudo ./sill init              # checks, binaries to /usr/local, policy, state filesystem, units
    sill init --user              # per account: sill-gui starts with your session
    sudo ./sill init --dry-run    # shows the plan without changing anything
    sudo ./sill init --prefix /opt/sill

Then bind `sill toggle` to a key (Alt+Space is the usual choice). Needs bubblewrap, a kernel >= 6.2,
unprivileged user namespaces and cgroup v2; `sill init` reports what is missing.
TXT
( cd dist && tar --owner=0 --group=0 --numeric-owner -czf "$NAME.tar.gz" "$NAME" )
( cd dist && sha256sum "$NAME.tar.gz" > SHA256SUMS )
rm -rf "dist/$NAME"
echo "dist/$NAME.tar.gz"
cat dist/SHA256SUMS
