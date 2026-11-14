#!/bin/sh
# Build the Mac app's Linux system image: Debian 13 arm64 with sill's daemon, shim and jail
# runtime, booted with Apple's Virtualization framework (crates/sill-vm). Run on an Apple Silicon
# Mac from a clone; it builds the committed HEAD, inside the VM itself, with the server deployment
# (packaging/deploy-server.sh). Output: dist/mac-image/disk.raw, a sparse raw disk.
#
# The image leaves no key or identity behind: its first boot in the app runs cloud-init afresh
# with the app's own seed (account key, host keys, machine id).
set -eu
cd "$(dirname "$0")/.."
[ "$(uname -s)-$(uname -m)" = Darwin-arm64 ] || { echo 'Run on an Apple Silicon Mac.' >&2; exit 1; }
command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo 'Missing cargo (rustup).' >&2; exit 1; }

DEBIAN_URL=https://cloud.debian.org/images/cloud/trixie/latest
DEBIAN_IMAGE=debian-13-generic-arm64.tar.xz
DISK_SIZE=32g
cache="$HOME/Library/Caches/sill/image"
work="target/mac-image"
out="dist/mac-image"
mkdir -p "$cache" "$work" "$out"
rm -f "$work"/disk.raw "$work"/seed.iso "$work"/go "$work"/console.log
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
# sparse: room for the build and sill's state image, using only what is written
truncate -s "$DISK_SIZE" "$work/disk.raw"

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
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - $(cat "$work/key.pub")
package_update: true
packages: [git, ca-certificates, build-essential, pkg-config, bubblewrap, e2fsprogs, util-linux,
           libpam-systemd, dbus-user-session, openssh-server, sudo, curl, nodejs, npm]
EOF
printf 'instance-id: sill-image-build-%s\nlocal-hostname: sill-box\n' "$(date +%s)" > "$work/seed/meta-data"
hdiutil makehybrid -quiet -iso -joliet -default-volume-name cidata -o "$work/seed.iso" "$work/seed"

cargo build -q -p sill-vm
codesign --force --sign - --entitlements packaging/mac/sill-vm.entitlements target/debug/sill-vm 2>/dev/null
SILL_VM_HOLD="$PWD/$work/go" target/debug/sill-vm "$work" > "$work/address" 2> "$work/vm.log" &
vm=$!
trap 'touch "$work/go"; wait $vm 2>/dev/null || true' EXIT
while [ ! -s "$work/address" ]; do
    kill -0 $vm 2>/dev/null || { cat "$work/vm.log" >&2; exit 1; }
    sleep 1
done
address=$(cat "$work/address")
echo "Guest at $address"
guest() {
    ssh -q -i "$work/key" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o ConnectTimeout=10 "sill@$address" "$@"
}

echo 'Waiting for the first boot (packages)'
guest 'cloud-init status --wait >/dev/null; cloud-init status --long | grep -E "^(status|errors)"'
echo 'Building and installing sill in the guest'
git archive --format=tar HEAD | guest 'rm -rf src && mkdir src && tar -x -C src'
guest 'curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal >/dev/null'
# a fresh SSH login gives the deployment its user manager (docs/server.md)
guest '. ~/.cargo/env && cd src && ./packaging/deploy-server.sh'

echo 'Cleaning up'
guest 'rm -rf src .cargo .rustup .cache'
guest 'sudo apt-get -qq clean && sudo rm -rf /var/lib/apt/lists/*'
# the next boot is a new instance: the app's seed gives the account its key, and the host keys
# and machine id are made anew
guest 'sudo cloud-init clean --logs --machine-id && rm -f ~/.ssh/authorized_keys ~/.bash_history'
guest 'sudo fstrim -a >/dev/null 2>&1 || true; sudo poweroff' || true
touch "$work/go"
wait $vm || true
trap - EXIT

mv "$work/disk.raw" "$out/disk.raw"
printf '%s\n' "$(git rev-parse HEAD)" > "$out/commit"
echo "$out/disk.raw: $(du -h "$out/disk.raw" | cut -f1) used of $DISK_SIZE"
