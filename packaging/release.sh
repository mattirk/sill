#!/bin/sh
# Build the release tarball for this machine's architecture (SPEC §4.12 "Channels"):
#   silli-<ver>-<arch>.tar.gz   the three binaries, the built-in policy, icon, desktop file, units, README
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
NAME="silli-$VER-$ARCH"
if [ "${1:-}" != "--no-build" ]; then
    cargo build --release
fi
for b in silli silli-shim silli-gui; do
    [ -x "target/release/$b" ] || { echo "target/release/$b missing" >&2; exit 1; }
    # the desktop libraries (Wayland, xkbcommon, X11, Vulkan) are dlopen'd at run time; a binary that
    # links more than libc is a stale build from before the GTK removal
    extra=$(ldd "target/release/$b" | awk '{print $1}' | grep -v -e linux-vdso -e ld-linux -e '^libc\.so' -e '^libm\.so' -e '^libgcc_s' || true)
    [ -z "$extra" ] || { echo "target/release/$b links $(echo $extra): stale build, run without --no-build" >&2; exit 1; }
done
rm -rf "dist/$NAME"
mkdir -p "dist/$NAME/policies" "dist/$NAME/systemd/user" "dist/$NAME/icons"
install -m 755 target/release/silli target/release/silli-shim target/release/silli-gui "dist/$NAME/"
install -m 644 policies/default.toml "dist/$NAME/policies/"
install -m 644 packaging/silli.desktop "dist/$NAME/"
install -m 644 packaging/icons/silli.svg "dist/$NAME/icons/"
install -m 644 packaging/systemd/*.mount packaging/systemd/*.service "dist/$NAME/systemd/"
install -m 644 packaging/systemd/user/silli-gui.service "dist/$NAME/systemd/user/"
install -m 644 README.md LICENSE "dist/$NAME/"
cat > "dist/$NAME/INSTALL" <<'TXT'
silli — install from this tarball

    sudo ./silli init              # checks, binaries to /usr/local, policy, state filesystem, units
    silli init --user              # per account: silli-gui starts with your session
    sudo ./silli init --dry-run    # shows the plan without changing anything
    sudo ./silli init --prefix /opt/silli

Then bind `silli toggle` to a key (Alt+Space is the usual choice). Needs bubblewrap, a kernel >= 6.2,
unprivileged user namespaces and cgroup v2; `silli init` reports what is missing.
TXT
( cd dist && tar --owner=0 --group=0 --numeric-owner -czf "$NAME.tar.gz" "$NAME" )
( cd dist && sha256sum "$NAME.tar.gz" > SHA256SUMS )
rm -rf "dist/$NAME"
echo "dist/$NAME.tar.gz"
cat dist/SHA256SUMS
