#!/bin/sh
# Weston for the Mac app's VM, built from source (docs/sill-stick.md section 9): Debian 13 has
# Weston 14, and sill's test desktop needs `--debug-scopes` (Weston 15) so the debug extension
# its live view and screenshots use offers no scope that would show one client another's
# traffic. Only what the test desktop uses: the headless backend, software rendering, Xwayland,
# the desktop shell and weston-screenshooter.
#
# Run as root in the image's build guest: `sh weston-build.sh OUT`. Leaves OUT/weston.tar.gz (to
# extract at /) and OUT/runtime-packages (the Debian packages it needs at run time), and takes
# its build tools away again (pkg-config stays: the image has it for building sill). packaging/mac-image.sh keys its cache of the result by this file's
# hash: change anything here (versions, checksums, options) and it is built again.
set -eu
out=$1
WESTON=16.0.2
WESTON_SHA256=49dd87d18cb229384264d2f69cb5a51f14f61a111e9227bfedb91b3ca8c39643
# Weston 16 needs wayland-protocols 1.46, Debian 13 has 1.44; only its XML, at build time
PROTOCOLS=1.46
PROTOCOLS_SHA256=fd0de056a895fa48bd1aa5f0b8dfeed454101b88bc7e1c61a953422eb71db167
BUILD_DEPS="meson ninja-build libwayland-dev wayland-protocols libpixman-1-dev
    libxkbcommon-dev libinput-dev libdrm-dev libcairo2-dev libpng-dev libdisplay-info-dev
    libxcb1-dev libxcb-composite0-dev libxcb-xfixes0-dev libxcb-render0-dev libxcb-shape0-dev
    libxcb-xkb-dev libxcb-cursor-dev libxcursor-dev libx11-xcb-dev libpango1.0-dev libseat-dev
    libevdev-dev libudev-dev"

work=$(mktemp -d)
cd "$work"
apt-get update -qq
# shellcheck disable=SC2086
apt-get install -y -qq $BUILD_DEPS >/dev/null
fetch() { # url sha256
    curl -fsSL -o "$(basename "$1")" "$1"
    echo "$2  $(basename "$1")" | sha256sum -c - >/dev/null
}
fetch "https://gitlab.freedesktop.org/wayland/weston/-/releases/$WESTON/downloads/weston-$WESTON.tar.xz" "$WESTON_SHA256"
fetch "https://gitlab.freedesktop.org/wayland/wayland-protocols/-/releases/$PROTOCOLS/downloads/wayland-protocols-$PROTOCOLS.tar.xz" "$PROTOCOLS_SHA256"
tar -xf "weston-$WESTON.tar.xz"
mkdir -p "weston-$WESTON/subprojects"
tar -xf "wayland-protocols-$PROTOCOLS.tar.xz" -C "weston-$WESTON/subprojects"
mv "weston-$WESTON/subprojects/wayland-protocols-$PROTOCOLS" "weston-$WESTON/subprojects/wayland-protocols"
cd "weston-$WESTON"
meson setup build --prefix=/usr/local --buildtype=release --strip \
    -Dbackend-drm=false -Dbackend-pipewire=false -Dbackend-rdp=false -Dbackend-vnc=false \
    -Dbackend-wayland=false -Dbackend-x11=false -Dbackend-default=headless \
    -Drenderer-gl=false -Drenderer-vulkan=false -Dxwayland=true -Dsystemd=false \
    -Dshell-desktop=true -Dshell-ivi=false -Dshell-kiosk=false -Dshell-lua=false \
    -Dcolor-management-lcms=false -Dimage-jpeg=false -Dimage-webp=false \
    -Dtools= -Ddemo-clients=false -Dsimple-clients= -Dtests=false -Ddoc=false >/dev/null
ninja -C build >/dev/null
root="$work/root"
DESTDIR="$root" meson install -C build --quiet >/dev/null
# what nothing runs: headers, pkg-config files, protocol XML, the session file
rm -rf "$root/usr/local/include" "$root/usr/local/lib/aarch64-linux-gnu/pkgconfig" \
    "$root/usr/local/share/pkgconfig" "$root/usr/local/share/wayland-protocols" \
    "$root/usr/local/share/libweston-16" "$root/usr/local/share/wayland-sessions"
# the Debian packages its programs and modules load from (theirs, not its own); ldd says
# /lib/…, which Debian 13 merges into /usr/lib, where dpkg knows the files: resolve them first
find "$root" -type f \( -perm -u+x -o -name '*.so*' \) -print0 \
    | xargs -0 env LD_LIBRARY_PATH="$root/usr/local/lib/aarch64-linux-gnu" ldd 2>/dev/null \
    | awk '/=> \// { print $3 }' | grep -v "^$root" | sort -u | xargs -r realpath | sort -u \
    | xargs -r dpkg -S 2>/dev/null | cut -d: -f1 | sort -u > runtime-packages
grep -q '^libwayland-server0' runtime-packages || { echo 'weston-build: no runtime packages found' >&2; exit 1; }
mkdir -p "$out"
tar -czf "$out/weston.tar.gz" -C "$root" .
cp runtime-packages "$out/runtime-packages"
# the build tools go again; what Weston needs stays, marked as wanted
# shellcheck disable=SC2046
apt-get install -y -qq $(cat runtime-packages) >/dev/null
apt-mark manual $(cat runtime-packages) >/dev/null
# shellcheck disable=SC2086
apt-get purge -y -qq $BUILD_DEPS >/dev/null
apt-get autoremove -y -qq --purge >/dev/null
cd /
rm -rf "$work"
