#!/bin/sh
# Build the Debian package (SPEC §4.12): binaries under /usr, the policy as a conffile, the user units,
# postinst = `silli init --no-copy --prefix /usr` (state image, system units; the package's own files
# stay dpkg's). Plain dpkg-deb.
# Output: dist/silli_<ver>_<arch>.deb. Publishing to an apt repository (which needs a signing key) is
# not done here. No debhelper: the package is three binaries and a few files.
#
#   packaging/deb.sh            # cargo build --release, then package
#   packaging/deb.sh --no-build
set -eu
cd "$(dirname "$0")/.."
VER=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DEB_ARCH=$(dpkg --print-architecture)
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
PKG="dist/deb/silli_${VER}_${DEB_ARCH}"
rm -rf "$PKG"
mkdir -p "$PKG/DEBIAN" "$PKG/usr/bin" "$PKG/etc/silli/policies" "$PKG/etc/systemd/user" \
         "$PKG/usr/share/silli" "$PKG/usr/share/applications" "$PKG/usr/share/icons/hicolor/scalable/apps" \
         "$PKG/usr/share/doc/silli"
install -m 755 target/release/silli target/release/silli-shim target/release/silli-gui "$PKG/usr/bin/"
install -m 644 policies/default.toml "$PKG/etc/silli/policies/"
sed 's|/usr/local/bin/silli|/usr/bin/silli|' packaging/systemd/user/silli-gui.service > "$PKG/etc/systemd/user/silli-gui.service"
sed 's|@PREFIX@|/usr|' packaging/silli.desktop > "$PKG/usr/share/applications/silli.desktop"
chmod 644 "$PKG/etc/systemd/user/silli-gui.service" "$PKG/usr/share/applications/silli.desktop"
install -m 644 packaging/icons/silli.svg "$PKG/usr/share/silli/silli.svg"
install -m 644 packaging/icons/silli.svg "$PKG/usr/share/icons/hicolor/scalable/apps/silli.svg"
install -m 644 README.md LICENSE "$PKG/usr/share/doc/silli/"
SIZE=$(du -sk "$PKG" --exclude=DEBIAN | cut -f1)
# Depends: the binaries link only libc (ldd), the desktop libraries are dlopen'd (winit, x11rb, wgpu),
# so the list is by hand from `strings target/release/silli-gui | grep -o 'lib.*\.so\.[0-9]*'`:
# Wayland + xkbcommon and the X11 client libraries (either session), the Vulkan loader and a driver
# for the GPU renderer (iced draws in software without them), EGL for wgpu's GL fallback.
cat > "$PKG/DEBIAN/control" <<CTL
Package: silli
Version: $VER
Section: devel
Priority: optional
Architecture: $DEB_ARCH
Maintainer: Matti Rita-Kasari <matti@rita-kasari.fi>
Installed-Size: $SIZE
Depends: bubblewrap, e2fsprogs, curl, unzip, libwayland-client0, libxkbcommon0, libxkbcommon-x11-0, libx11-6, libx11-xcb1, libxcb1, libxcursor1, libxi6
Recommends: systemd, libvulkan1, mesa-vulkan-drivers, libegl1, libwayland-egl1
Suggests: weston, xwayland, dbus
Homepage: https://github.com/mattirk/silli
Description: AI agents in a jail the host controls
 AI agents on your own Linux machine: an ACP client, a bubblewrap jail
 with every write recorded in an overlay, and a ledger of every change
 the agent makes. silli-gui is the desktop window; the jail runs as a
 systemd user unit where there is one.
CTL
cat > "$PKG/DEBIAN/conffiles" <<CF
/etc/silli/policies/default.toml
/etc/systemd/user/silli-gui.service
CF
cat > "$PKG/DEBIAN/postinst" <<'PI'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    # the package put the files in place; init does the rest (state image, units, checks)
    /usr/bin/silli init --no-copy --prefix /usr || echo "silli init reported problems; run it again: sudo silli init --no-copy --prefix /usr" >&2
fi
PI
cat > "$PKG/DEBIAN/postrm" <<'PR'
#!/bin/sh
set -e
# The state image /var/lib/silli.img and the units silli init wrote under /etc/systemd/system are left
# alone on remove and purge: they hold every user's pending session changes.
if [ "$1" = "purge" ]; then
    echo "silli: /var/lib/silli.img and /etc/systemd/system/{var-lib-silli.mount,silli-state-*.service} were kept" >&2
fi
PR
chmod 755 "$PKG/DEBIAN/postinst" "$PKG/DEBIAN/postrm"
dpkg-deb --root-owner-group --build "$PKG" "dist/silli_${VER}_${DEB_ARCH}.deb"
rm -rf "$PKG"
echo "dist/silli_${VER}_${DEB_ARCH}.deb"
