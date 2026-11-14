#!/bin/sh
# Sill.app and Sill.dmg (docs/sill-stick.md section 9): the window, its icon and the VM's system
# image in one app, signed for this Mac only (ad hoc, with the virtualization entitlement:
# local signing first, the person's decision of 2026-11-12). Run on an Apple Silicon Mac after
# packaging/mac-image.sh. Output: dist/Sill.app, dist/Sill.dmg.
#
# The image travels compressed (Resources/vm/system.raw.gz): a sparse 16 GB file copied out of a
# disk image would take all 16 GB. sill-gui unpacks it sparsely on first start, and again when a
# newer app brings a newer image; the data disks are never touched.
set -eu
cd "$(dirname "$0")/../.."
[ "$(uname -s)-$(uname -m)" = Darwin-arm64 ] || { echo 'Run on an Apple Silicon Mac.' >&2; exit 1; }
command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"
image=dist/mac-image
[ -f "$image/system.raw" ] && [ -f "$image/commit" ] || {
    echo 'No system image: build it first with packaging/mac-image.sh' >&2; exit 1;
}

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
build=$(git rev-list --count HEAD)
commit=$(git rev-parse --short HEAD)
echo "Sill $version ($build, $commit); image $(cut -c1-7 "$image/commit")"

cargo build -q --release -p sill-gui
# the image compressed once per image build, kept beside it
if [ ! -f "$image/system.raw.gz" ] || [ "$(cat "$image/system.raw.gz.commit" 2>/dev/null)" != "$(cat "$image/commit")" ]; then
    echo 'Compressing the system image (about a minute)'
    gzip -6 -c "$image/system.raw" > "$image/system.raw.gz.new"
    mv "$image/system.raw.gz.new" "$image/system.raw.gz"
    # its size, so sill-gui can make the file whole before writing into it (sparse on APFS)
    stat -f %z "$image/system.raw" > "$image/system.raw.gz.size"
    cp "$image/commit" "$image/system.raw.gz.commit"
fi
cargo run -q -p sill-gui --example mac_icon -- packaging/icons/sill-tray.svg target/Sill.iconset

app=dist/Sill.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/vm"
cp target/release/sill-gui "$app/Contents/MacOS/sill-gui"
iconutil -c icns -o "$app/Contents/Resources/Sill.icns" target/Sill.iconset
cp -c "$image/system.raw.gz" "$app/Contents/Resources/vm/system.raw.gz"
cp "$image/system.raw.gz.size" "$app/Contents/Resources/vm/system.raw.gz.size"
cp "$image/commit" "$app/Contents/Resources/vm/commit"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Sill</string>
    <key>CFBundleDisplayName</key><string>Sill</string>
    <key>CFBundleIdentifier</key><string>com.github.mattirk.sill</string>
    <key>CFBundleExecutable</key><string>sill-gui</string>
    <key>CFBundleIconFile</key><string>Sill</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$build</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSHumanReadableCopyright</key><string>Sill $version ($commit)</string>
</dict>
</plist>
EOF
codesign --force --options runtime --entitlements packaging/mac/sill-vm.entitlements \
    --sign - "$app" 2>/dev/null
codesign --verify --strict "$app"

# the disk image: the app beside a link to Applications, to drag it there
stage=target/Sill.dmg.stage
rm -rf "$stage" dist/Sill.dmg
mkdir -p "$stage"
cp -Rc "$app" "$stage/"
ln -s /Applications "$stage/Applications"
hdiutil create -quiet -volname Sill -srcfolder "$stage" -format UDZO -ov dist/Sill.dmg
rm -rf "$stage"
echo "$app: $(du -sh "$app" | cut -f1); dist/Sill.dmg: $(du -h dist/Sill.dmg | cut -f1)"
