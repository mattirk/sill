#!/bin/sh
# Build the Mac app's Linux system image: Debian 13 arm64 with sill's daemon, shim and jail
# runtime. On Apple Silicon, boot with Apple's Virtualization framework (crates/sill-vm).
# On ARM64 Linux, boot with QEMU (no nested virtualization required); CI supplies native-built
# binaries via SILL_IMAGE_BINARIES. Both paths install committed HEAD with the server deployment
# (packaging/deploy-server.sh). Output: dist/mac-image/system.raw, a sparse raw disk.
#
# Only the system is in the image. The account's home and sill's state go on data disks of their
# own (home.raw, state.raw), which the app makes and keeps across new images; the guest formats
# them on first use (packaging/mac/guest). The image leaves no key or identity behind: its first
# boot in the app runs cloud-init afresh with the app's own seed (account key, host keys,
# machine id).
set -eu
cd "$(dirname "$0")/.."
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) image_backend=apple ;;
    Linux-aarch64|Linux-arm64) image_backend=qemu ;;
    *) echo 'Run on an Apple Silicon Mac or an ARM64 Linux host.' >&2; exit 1 ;;
esac
command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo 'Missing cargo (rustup).' >&2; exit 1; }

DEBIAN_URL=https://cloud.debian.org/images/cloud/trixie/latest
DEBIAN_IMAGE=debian-13-generic-arm64.tar.xz
SYSTEM_SIZE=16g
# the build's own data disks, thrown away after it
DATA_SIZE=32g
# downloads and built pieces kept between builds; CI points this at its own cache
cache="${SILL_IMAGE_CACHE:-$HOME/Library/Caches/sill/image}"
work="target/mac-image"
out="dist/mac-image"
mkdir -p "$cache" "$work" "$out"
rm -f "$work"/*.raw "$work"/seed.iso "$work"/ready "$work"/go "$work"/console.log
rm -rf "$work"/seed

# Debian's image, verified against its published checksums
(
    cd "$cache"
    curl -fsSLO "$DEBIAN_URL/SHA512SUMS"
    if ! grep " $DEBIAN_IMAGE\$" SHA512SUMS | shasum -a 512 -c - >/dev/null 2>&1; then
        echo "Downloading $DEBIAN_IMAGE"
        curl -fL# -o "$DEBIAN_IMAGE" "$DEBIAN_URL/$DEBIAN_IMAGE"
        grep " $DEBIAN_IMAGE\$" SHA512SUMS | shasum -a 512 -c -
    fi
)
tar -xf "$cache/$DEBIAN_IMAGE" -C "$work" disk.raw
mv "$work/disk.raw" "$work/system.raw"
# sparse: room for the build, using only what is written
truncate -s "$SYSTEM_SIZE" "$work/system.raw"
truncate -s "$DATA_SIZE" "$work/home.raw" "$work/state.raw"

# a key for this build only, and the packages the box needs (docs/server.md, plus Node for
# registry installs)
rm -f "$work/key" "$work/key.pub"
ssh-keygen -q -t ed25519 -N '' -f "$work/key" -C sill-image-build
mkdir "$work/seed"
cat > "$work/seed/user-data" <<EOF
#cloud-config
hostname: sill-box
users:
  - name: sill
    shell: /bin/bash
    # for this build only: removed at its end
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - $(cat "$work/key.pub")
# lean from the first package on: no recommended extras, no docs or translations
# (packaging/mac/guest/apt-lean.conf, dpkg-lean.cfg)
write_files:
  - path: /etc/apt/apt.conf.d/90sill-lean
    encoding: b64
    content: $(base64 < packaging/mac/guest/apt-lean.conf | tr -d '\n')
  - path: /etc/dpkg/dpkg.cfg.d/90sill-lean
    encoding: b64
    content: $(base64 < packaging/mac/guest/dpkg-lean.cfg | tr -d '\n')
package_update: true
# the box (docs/server.md; Node for registry installs comes from nodejs.org below), then
# Desktop development's test
# desktop (docs/permission-modes.md): Xwayland, the agent's input, software OpenGL and Vulkan
# for the programs under test (the VM has no GPU of its own), the jail's private D-Bus, a font;
# Weston itself is built from source below
packages: [git, ripgrep, ca-certificates, build-essential, pkg-config, bubblewrap, e2fsprogs, util-linux,
           libpam-systemd, dbus-user-session, openssh-server, sudo, curl, xz-utils,
           xwayland, xdotool, libxi6, libxkbcommon-x11-0, libxcb-xkb1,
           libgl1-mesa-dri, libegl1, mesa-vulkan-drivers, dbus,
           fonts-dejavu-core]
# what cloud-init does shows on the virtio console (console.log), and the build learns there
# when the account is ready, so it never tries a login too early
output: {all: '| tee -a /var/log/cloud-init-output.log /dev/hvc0'}
runcmd:
  - [sh, -c, 'echo sill-image-ready > /dev/hvc0']
EOF
printf 'instance-id: sill-image-build-%s\nlocal-hostname: sill-box\n' "$(date +%s)" > "$work/seed/meta-data"
if [ "$image_backend" = apple ]; then
    hdiutil makehybrid -quiet -iso -joliet -default-volume-name cidata -o "$work/seed.iso" "$work/seed"
    cargo build --locked -q -p sill-vm
    codesign --force --sign - --entitlements packaging/mac/sill-vm.entitlements target/debug/sill-vm 2>/dev/null
else
    cloud-localds "$work/seed.iso" "$work/seed/user-data" "$work/seed/meta-data"
fi
# Boot the guest, held until `go` exists; the build never reboots it from inside, it powers it
# off and boots it again from here, as the app does
vm=
boot() {
    rm -f "$work/go" "$work/ready"
    SILL_VM_HOLD="$PWD/$work/go" target/debug/sill-vm "$work" > "$work/ready" 2>> "$work/vm.log" &
    vm=$!
    while [ ! -s "$work/ready" ]; do
        kill -0 $vm 2>/dev/null || { cat "$work/vm.log" >&2; exit 1; }
        sleep 1
    done
    echo 'Guest is up'
}
# a clean power-off, then the VM process ends by itself
power_off() {
    guest 'sudo systemctl poweroff' || true
    touch "$work/go"
    wait $vm || true
    vm=
}
trap 'touch "$work/go"; [ -z "$vm" ] || wait $vm 2>/dev/null || true' EXIT
: > "$work/vm.log"
guest() {
    # never a password prompt: until cloud-init has put the key in place, a login just fails
    # over the VM's host–guest socket (sill-vm's ssh.sock), never the guest's network
    ssh -q -i "$work/key" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 \
        -o ProxyCommand="nc -U $PWD/$work/ssh.sock" sill@sill-box "$@"
}

# Override only transport/lifecycle on Linux; all image provisioning below is shared.
if [ "$image_backend" = qemu ]; then
    . ./packaging/mac/image-qemu.sh
fi
boot

# SSH answers before cloud-init has made the account; OpenSSH holds failed logins against
# the source for a while, so try at a gentle pace
let_in() {
    tries=0
    until guest true 2>/dev/null; do
        tries=$((tries + 1))
        [ $tries -lt 60 ] || { echo 'The guest never let the build in.' >&2; exit 1; }
        sleep 10
    done
    # vsock answers before the guest's network is up; the build downloads
    guest 'timeout 120 sh -c "until getent hosts deb.debian.org >/dev/null; do sleep 1; done"' ||
        { echo 'The guest has no network.' >&2; exit 1; }
}
echo 'Waiting for the first boot (packages; console: target/mac-image/console.log)'
# cloud-init's last step marks the console; logins before it would only fail, and OpenSSH holds
# failed logins against the source
tries=0
until grep -q sill-image-ready "$work/console.log" 2>/dev/null; do
    tries=$((tries + 1))
    kill -0 "$vm" 2>/dev/null || { cat "$work/vm.log" >&2; exit 1; }
    [ $tries -lt 180 ] || { echo 'The first boot did not finish in 30 minutes.' >&2; exit 1; }
    sleep 10
done
let_in
guest 'cloud-init status --long | grep -E "^(status|errors)"'
# again, so the account's user manager starts with the session bus the packages brought
power_off
boot
let_in
echo 'Setting up the data disks'
git archive --format=tar HEAD packaging/mac/guest | guest 'rm -rf guest && mkdir guest && tar -x -C guest'
guest 'sudo install -D -m755 guest/packaging/mac/guest/sill-disks /usr/local/lib/sill/sill-disks &&
    sudo install -D -m755 -o root -g root guest/packaging/mac/guest/sill-mac-folders /usr/local/lib/sill/sill-mac-folders &&
    sudo install -m644 guest/packaging/mac/guest/sill-disks.service /etc/systemd/system/ &&
    sudo sh -c "cat guest/packaging/mac/guest/fstab >> /etc/fstab" &&
    sudo install -m644 guest/packaging/mac/guest/grub.cfg /etc/default/grub.d/sill.cfg &&
    sudo install -m644 guest/packaging/mac/guest/gitconfig /etc/gitconfig &&
    sudo install -m644 guest/packaging/mac/guest/sysctl.conf /etc/sysctl.d/90-sill.conf &&
    sudo install -m644 guest/packaging/mac/guest/sshd.conf /etc/ssh/sshd_config.d/90-sill.conf &&
    sudo sshd -t &&
    sudo install -D -m644 guest/packaging/mac/guest/resolved.conf /etc/systemd/resolved.conf.d/90-sill.conf &&
    sudo install -m600 guest/packaging/mac/guest/network.yaml /etc/netplan/90-sill.yaml &&
    sudo install -m644 guest/packaging/mac/guest/cloud-network.cfg /etc/cloud/cloud.cfg.d/99-sill-network.cfg &&
    sudo rm -f /etc/netplan/50-cloud-init.yaml &&
    sudo update-grub 2>/dev/null &&
    sudo mkdir -p /mnt/sill-state && rm -rf guest &&
    sudo systemctl daemon-reload && sudo systemctl enable sill-disks.service &&
    sudo /usr/local/lib/sill/sill-disks && sudo mount -a && findmnt -n -o SOURCE,TARGET /home && findmnt -n -o SOURCE,TARGET /mnt/sill-state'
# Node for the registry's agents, newer than Debian's 20 (Kimi Code needs 22.19 or later; the
# person chose the LTS line, 2026-11-13): nodejs.org's build, pinned by version and checksum,
# downloaded once into the cache, in /usr/local with its npm and npx
NODE_VERSION=v24.22.3
NODE_SHA256=6e51a2ada00f0011cc266f65d98922b403d7ad88f809ca2aa615a5fe1ce6e86c
node_tar="$cache/node-$NODE_VERSION-linux-arm64.tar.xz"
if ! { [ -f "$node_tar" ] && echo "$NODE_SHA256  $node_tar" | shasum -a 256 -c -s; }; then
    echo "Downloading Node $NODE_VERSION"
    curl -fsSL -o "$node_tar.part" "https://nodejs.org/dist/$NODE_VERSION/node-$NODE_VERSION-linux-arm64.tar.xz"
    echo "$NODE_SHA256  $node_tar.part" | shasum -a 256 -c -s || { echo 'Node download does not match its checksum.' >&2; exit 1; }
    mv "$node_tar.part" "$node_tar"
fi
guest 'sudo tar -xJ -C /usr/local --strip-components=1 --no-same-owner \
    --exclude=CHANGELOG.md --exclude=README.md --exclude=LICENSE --exclude="share/doc" --exclude="include" &&
    node --version && npm --version' < "$node_tar"
# Weston 16, built once per recipe (packaging/mac/guest/weston-build.sh) and kept in the cache
# under its hash: CI caches the same file by the same key
weston_key=$(shasum -a 256 packaging/mac/guest/weston-build.sh | cut -c1-16)
weston_cache="$cache/weston-$weston_key"
if [ -f "$weston_cache/weston.tar.gz" ] && [ -f "$weston_cache/runtime-packages" ]; then
    echo "Weston from the cache ($weston_key)"
else
    echo "Building Weston ($weston_key; cached for later builds)"
    git archive --format=tar HEAD packaging/mac/guest/weston-build.sh | guest 'rm -rf wb && mkdir wb && tar -x -C wb'
    guest 'sudo sh wb/packaging/mac/guest/weston-build.sh /tmp/weston-out && rm -rf wb'
    mkdir -p "$weston_cache.new"
    guest 'cat /tmp/weston-out/weston.tar.gz' > "$weston_cache.new/weston.tar.gz"
    guest 'cat /tmp/weston-out/runtime-packages' > "$weston_cache.new/runtime-packages"
    rm -rf "$weston_cache" && mv "$weston_cache.new" "$weston_cache"
fi
guest 'sudo rm -rf /tmp/weston-out && mkdir -p /tmp/weston-in'
guest 'cat > /tmp/weston-in/weston.tar.gz' < "$weston_cache/weston.tar.gz"
guest 'cat > /tmp/weston-in/runtime-packages' < "$weston_cache/runtime-packages"
guest 'sudo apt-get install -y -qq $(cat /tmp/weston-in/runtime-packages) >/dev/null &&
    sudo tar -xzf /tmp/weston-in/weston.tar.gz -C / && sudo ldconfig && rm -rf /tmp/weston-in &&
    weston --version'
echo 'Building and installing sill in the guest'
git archive --format=tar HEAD | guest 'rm -rf src && mkdir src && tar -x -C src'
# Rust is only used for the image build and removed below. Leave shell profiles untouched;
# deployment sources cargo/env explicitly, so fresh installs need no Rust initialization.
if [ -n "${SILL_IMAGE_BINARIES:-}" ]; then
    [ "$(cat "$SILL_IMAGE_BINARIES/commit")" = "$(git rev-parse HEAD)" ] || {
        echo 'Prebuilt binaries do not match this commit.' >&2; exit 1;
    }
    for binary in sill sill-shim; do
        guest "mkdir -p src/target/release && cat > src/target/release/$binary && chmod 755 src/target/release/$binary" < "$SILL_IMAGE_BINARIES/$binary"
    done
    guest 'cd src && SILL_STATE_DIR=/mnt/sill-state ./packaging/deploy-server.sh --no-build'
else
    guest 'curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --no-modify-path >/dev/null'
    # A fresh SSH login gives deployment its user manager; the state is the data disk's.
    guest '. ~/.cargo/env && cd src && SILL_STATE_DIR=/mnt/sill-state ./packaging/deploy-server.sh'
fi

# a disk that failed under the guest loses writes quietly: refuse such an image
if guest 'sudo dmesg | grep -qiE "I/O error|EXT4-fs error|Oops|Internal error"'; then
    guest 'sudo dmesg | grep -iE "I/O error|EXT4-fs error|Oops|Internal error" | head -5' >&2
    echo 'The guest saw disk or kernel errors; not keeping this image.' >&2
    exit 1
fi
echo 'Cleaning up'
guest 'rm -rf src .cargo .rustup .cache'
guest 'sudo apt-get -qq clean && sudo rm -rf /var/lib/apt/lists/*'
# what the cloud image brought before the lean settings: docs, manuals and translations
guest 'sudo find /usr/share/doc -mindepth 1 ! -name copyright ! -type d -delete;
    sudo find /usr/share/doc -mindepth 1 -type d -empty -delete;
    sudo rm -rf /usr/share/man/* /usr/share/info/*;
    sudo find /usr/share/locale -mindepth 1 -maxdepth 1 ! -name locale.alias -exec rm -rf {} +'
# the test desktop is there (the daemon's probe would say why not)
guest 'weston --version && command -v Xwayland xdotool >/dev/null'
# the next boot is a new instance: the app's seed gives the account its key, and the host keys
# and machine id are made anew
# the home the build left (the user units' links among it) becomes the image's own /home, which
# a new home disk starts from, without the build's key; the build's own home disk keeps the key
# for the logins still to come and is thrown away
guest 'sudo cloud-init clean --logs --machine-id && rm -f ~/.bash_history &&
    sudo mkdir -p /mnt/root && sudo mount --bind / /mnt/root &&
    sudo rm -rf /mnt/root/home/sill && sudo cp -a /home/sill /mnt/root/home/ &&
    sudo rm -f /mnt/root/home/sill/.ssh/authorized_keys &&
    sudo umount /mnt/root && sudo rmdir /mnt/root'
# last, as root in one go: the account's sudo was the build's alone. The app's seed gives none,
# and what Sill.app does as root there goes through root's SSH and the folder mounter
# (docs/sill-mac.md, finding 15). An image whose account can still become root is not kept.
left=$(guest 'sudo sh -c "fstrim -a >/dev/null 2>&1
    rm -f /etc/sudoers.d/90-cloud-init-users
    if runuser -u sill -- sudo -n true 2>/dev/null; then echo sill-still-root; fi
    test -x /usr/local/lib/sill/sill-mac-folders || echo sill-no-mounter
    echo sill-image-clean
    systemctl poweroff"' 2>&1) || true
touch "$work/go"
if [ "$image_backend" = qemu ]; then wait_off; else wait $vm || true; vm=; fi
case "$left" in
    *sill-still-root*) echo 'The account still has sudo; not keeping this image.' >&2; exit 1 ;;
    *sill-no-mounter*) echo 'The folder mounter is not in the image; not keeping it.' >&2; exit 1 ;;
esac
case "$left" in
    *sill-image-clean*) ;;
    *) echo 'Guest did not confirm final cleanup; not keeping this image.' >&2; exit 1 ;;
esac
trap - EXIT

mv "$work/system.raw" "$out/system.raw"
rm -f "$work/home.raw" "$work/state.raw"
printf '%s\n' "$(git rev-parse HEAD)" > "$out/commit"
echo "$out/system.raw: $(du -h "$out/system.raw" | cut -f1) used of $SYSTEM_SIZE"
