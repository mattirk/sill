#!/bin/sh
# Sourced by mac-image.sh on ARM64 Linux. Match Apple's disk serials and virtio console.
# TCG works on hosted runners without /dev/kvm; expensive compilation happens natively first.
qemu_firmware=${SILL_QEMU_FIRMWARE:-/usr/share/AAVMF/AAVMF_CODE.fd}
qemu_vars=${SILL_QEMU_VARS:-/usr/share/AAVMF/AAVMF_VARS.fd}
qemu_port=${SILL_IMAGE_SSH_PORT:-22222}
[ -r "$qemu_firmware" ] && [ -r "$qemu_vars" ] || { echo 'Install qemu-efi-aarch64.' >&2; exit 1; }
cp "$qemu_vars" "$work/efi-vars.raw"
boot() {
    qemu-system-aarch64 -machine virt -cpu max -accel tcg,thread=multi -smp 4 -m 4096 \
        -display none -monitor none -serial "file:$work/serial.log" \
        -drive "if=pflash,format=raw,readonly=on,file=$qemu_firmware" \
        -drive "if=pflash,format=raw,file=$work/efi-vars.raw" \
        -drive "if=none,id=system,format=raw,discard=unmap,file=$work/system.raw" \
        -device virtio-blk-pci,drive=system,serial=sill-system,bootindex=1 \
        -drive "if=none,id=home,format=raw,discard=unmap,file=$work/home.raw" \
        -device virtio-blk-pci,drive=home,serial=sill-home \
        -drive "if=none,id=state,format=raw,discard=unmap,file=$work/state.raw" \
        -device virtio-blk-pci,drive=state,serial=sill-state \
        -drive "if=none,id=seed,format=raw,readonly=on,file=$work/seed.iso" \
        -device virtio-blk-pci,drive=seed \
        -device virtio-serial-pci \
        -chardev "file,id=console,path=$work/console.log,append=on" \
        -device virtconsole,chardev=console \
        -netdev "user,id=net,hostfwd=tcp:127.0.0.1:$qemu_port-:22" \
        -device virtio-net-pci,netdev=net >> "$work/vm.log" 2>&1 &
    vm=$!
}
guest() {
    ssh -q -i "$work/key" -o BatchMode=yes -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 \
        -p "$qemu_port" sill@127.0.0.1 "$@"
}
wait_off() {
    attempts=0
    while kill -0 "$vm" 2>/dev/null; do
        attempts=$((attempts + 1))
        [ "$attempts" -lt 120 ] || { echo 'QEMU did not power off cleanly.' >&2; return 1; }
        sleep 1
    done
    wait "$vm"
    vm=
}
power_off() {
    guest 'sudo systemctl poweroff' || true
    wait_off
}
# On failure/cancellation, stop QEMU instead of waiting indefinitely for an active guest.
trap '[ -z "$vm" ] || { kill "$vm" 2>/dev/null || true; wait "$vm" 2>/dev/null || true; }' EXIT

trap 'exit 130' INT
trap 'exit 143' TERM
