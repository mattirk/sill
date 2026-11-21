#!/bin/sh
# Silli.app and Silli.dmg (docs/silli-stick.md section 9): the window, its icon and the VM's system
# image in one app, signed for this Mac only with the virtualization entitlement (local signing
# first, the person's decision of 2026-11-12): with packaging/mac/signing-identity.sh's
# certificate when there is one, else ad hoc. Run on an Apple Silicon Mac after
# packaging/mac-image.sh. Output: dist/Silli.app, dist/Silli.dmg.
#
# The image travels compressed (Resources/vm/system.raw.gz): a sparse 16 GB file copied out of a
# disk image would take all 16 GB. silli-gui unpacks it sparsely on first start, and again when a
# newer app brings a newer image; the data disks are never touched.
set -eu
cd "$(dirname "$0")/../.."
[ "$(uname -s)-$(uname -m)" = Darwin-arm64 ] || { echo 'Run on an Apple Silicon Mac.' >&2; exit 1; }
command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"
image=dist/mac-image
[ -f "$image/commit" ] && { [ -f "$image/system.raw" ] || [ -f "$image/system.raw.gz" ]; } || {
    echo 'No system image: build it first with packaging/mac-image.sh' >&2; exit 1;
}

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
build=$(git rev-list --count HEAD)
commit=$(git rev-parse --short HEAD)
echo "Silli $version ($build, $commit); image $(cut -c1-7 "$image/commit")"

cargo build --locked -q --release -p silli-gui
# the image compressed once per image build, kept beside it
if [ ! -f "$image/system.raw.gz" ] || [ "$(cat "$image/system.raw.gz.commit" 2>/dev/null)" != "$(cat "$image/commit")" ]; then
    [ -f "$image/system.raw" ] || { echo 'Compressed image has stale or missing commit metadata.' >&2; exit 1; }
    echo 'Compressing the system image (about a minute)'
    gzip -6 -c "$image/system.raw" > "$image/system.raw.gz.new"
    mv "$image/system.raw.gz.new" "$image/system.raw.gz"
    # its size, so silli-gui can make the file whole before writing into it (sparse on APFS)
    stat -f %z "$image/system.raw" > "$image/system.raw.gz.size"
    cp "$image/commit" "$image/system.raw.gz.commit"
fi
[ -s "$image/system.raw.gz.size" ] || { echo 'Missing uncompressed image size.' >&2; exit 1; }
[ -z "${SILLI_IMAGE_EXPECT_COMMIT:-}" ] || [ "$(cat "$image/commit")" = "$SILLI_IMAGE_EXPECT_COMMIT" ] || {
    echo 'System image is from another commit; rebuild it before packaging.' >&2; exit 1;
}
cargo run --locked -q -p silli-gui --example mac_icon -- packaging/icons/silli-tray.svg target/Silli.iconset

app=dist/Silli.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/vm"
cp target/release/silli-gui "$app/Contents/MacOS/silli-gui"
cp LICENSE "$app/Contents/Resources/LICENSE"
iconutil -c icns -o "$app/Contents/Resources/Silli.icns" target/Silli.iconset
cp -c "$image/system.raw.gz" "$app/Contents/Resources/vm/system.raw.gz"
cp "$image/system.raw.gz.size" "$app/Contents/Resources/vm/system.raw.gz.size"
cp "$image/commit" "$app/Contents/Resources/vm/commit"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Silli</string>
    <key>CFBundleDisplayName</key><string>Silli</string>
    <key>CFBundleIdentifier</key><string>com.github.mattirk.silli</string>
    <key>CFBundleExecutable</key><string>silli-gui</string>
    <key>CFBundleIconFile</key><string>Silli</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$build</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSHumanReadableCopyright</key><string>Silli $version ($commit)</string>
</dict>
</plist>
EOF
# the local signing identity keeps macOS's consents across builds; ad hoc loses them each time
identity=${SILLI_SIGNING_IDENTITY:-}
if [ -n "$identity" ]; then
    : # Explicit CI identity, or "-" for free ad-hoc signing.
elif security find-identity -v -p codesigning 2>/dev/null | grep -q '"Silli local development"'; then
    identity="Silli local development"
else
    identity=-
    echo 'Signing ad hoc: macOS will forget its consents for this build (packaging/mac/signing-identity.sh fixes it).'
fi
codesign --force --options runtime --entitlements packaging/mac/silli-vm.entitlements \
    --sign "$identity" "$app" 2>/dev/null
codesign --verify --strict "$app"

# the disk image: the app beside a link to Applications, to drag it there
stage=target/Silli.dmg.stage
rm -rf "$stage" dist/Silli.dmg
mkdir -p "$stage"
cp -Rc "$app" "$stage/"
ln -s /Applications "$stage/Applications"
hdiutil create -quiet -volname Silli -srcfolder "$stage" -format UDZO -ov dist/Silli.dmg
rm -rf "$stage"
echo "$app: $(du -sh "$app" | cut -f1); dist/Silli.dmg: $(du -h dist/Silli.dmg | cut -f1)"
