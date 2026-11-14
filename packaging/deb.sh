#!/bin/sh
# Build the Debian package (SPEC §4.12): binaries under /usr, the policy as a conffile, the user units,
# postinst = `sill init --no-copy --prefix /usr` (state image, system units; the package's own files
# stay dpkg's). Plain dpkg-deb.
# Output: dist/sill_<ver>_<arch>.deb. Publishing to an apt repository (which needs a signing key) is
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
PKG="dist/deb/sill_${VER}_${DEB_ARCH}"
rm -rf "$PKG"
mkdir -p "$PKG/DEBIAN" "$PKG/usr/bin" "$PKG/etc/sill/policies" "$PKG/etc/systemd/user" \
         "$PKG/usr/share/sill" "$PKG/usr/share/applications" "$PKG/usr/share/icons/hicolor/scalable/apps" \
         "$PKG/usr/share/doc/sill"
install -m 755 target/release/sill target/release/sill-shim target/release/sill-gui "$PKG/usr/bin/"
install -m 644 policies/default.toml "$PKG/etc/sill/policies/"
sed 's|/usr/local/bin/sill|/usr/bin/sill|' packaging/systemd/user/sill-gui.service > "$PKG/etc/systemd/user/sill-gui.service"
sed 's|@PREFIX@|/usr|' packaging/sill.desktop > "$PKG/usr/share/applications/sill.desktop"
chmod 644 "$PKG/etc/systemd/user/sill-gui.service" "$PKG/usr/share/applications/sill.desktop"
install -m 644 packaging/icons/sill.svg "$PKG/usr/share/sill/sill.svg"
install -m 644 packaging/icons/sill.svg "$PKG/usr/share/icons/hicolor/scalable/apps/sill.svg"
install -m 644 README.md SPEC.md "$PKG/usr/share/doc/sill/"
SIZE=$(du -sk "$PKG" --exclude=DEBIAN | cut -f1)
cat > "$PKG/DEBIAN/control" <<CTL
Package: sill
Version: $VER
Section: devel
Priority: optional
Architecture: $DEB_ARCH
Maintainer: Matti Rita-Kasari <matti@rita-kasari.fi>
Installed-Size: $SIZE
Depends: bubblewrap, e2fsprogs, libxkbcommon0, libwayland-client0
Recommends: systemd, libvulkan1
Suggests: weston, xwayland, dbus
Homepage: https://github.com/mattirk/sill
Description: AI agents in a jail the host controls
 AI agents on your own Linux machine: an ACP client, a bubblewrap jail
 with every write recorded in an overlay, and a ledger of every change
 the agent makes. sill-gui is the desktop window; the jail runs as a
 systemd user unit where there is one.
CTL
cat > "$PKG/DEBIAN/conffiles" <<CF
/etc/sill/policies/default.toml
/etc/systemd/user/sill-gui.service
CF
cat > "$PKG/DEBIAN/postinst" <<'PI'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    # the package put the files in place; init does the rest (state image, units, checks)
    /usr/bin/sill init --no-copy --prefix /usr || echo "sill init reported problems; run it again: sudo sill init --no-copy --prefix /usr" >&2
fi
PI
cat > "$PKG/DEBIAN/postrm" <<'PR'
#!/bin/sh
set -e
# The state image /var/lib/sill.img and the units sill init wrote under /etc/systemd/system are left
# alone on remove and purge: they hold every user's pending session changes.
if [ "$1" = "purge" ]; then
    echo "sill: /var/lib/sill.img and /etc/systemd/system/{var-lib-sill.mount,sill-state-*.service} were kept" >&2
fi
PR
chmod 755 "$PKG/DEBIAN/postinst" "$PKG/DEBIAN/postrm"
dpkg-deb --root-owner-group --build "$PKG" "dist/sill_${VER}_${DEB_ARCH}.deb"
rm -rf "$PKG"
echo "dist/sill_${VER}_${DEB_ARCH}.deb"
