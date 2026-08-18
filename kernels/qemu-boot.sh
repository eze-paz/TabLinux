#!/bin/bash
# Boot the same kernel + initramfs + disk under QEMU as a reference oracle for
# what our emulator should be doing. QEMU lives in a local prefix built without
# root — see ~/qemu-local. Pass --no-disk to boot the initramfs alone.
#
# Note the structural difference from our emulator: QEMU runs OpenSBI in M-mode
# and enters the kernel in S-mode from there, whereas riscv-vm enters S-mode
# directly and answers SBI calls itself.
set -u
Q="$HOME/qemu-local"
export LD_LIBRARY_PATH="$Q/root/usr/lib/x86_64-linux-gnu:$Q/root/lib/x86_64-linux-gnu:$Q/root/usr/lib"
K="$HOME/riscv-vm/kernels"

ARGS="earlycon=uart8250,mmio,0x10000000 console=ttyS0 loglevel=7"
DISK=()
if [ "${1:-}" != "--no-disk" ]; then
    ARGS="$ARGS root=/dev/vda rootfstype=ext4 rootflags=rw modules=virtio_mmio,virtio_blk,ext4"
    DISK=(-drive file="$K/rootfs.ext4",format=raw,if=none,id=hd0
          -device virtio-blk-device,drive=hd0)
fi

exec "$Q/root/usr/bin/qemu-system-riscv64" \
    -M virt -m 1G -smp 1 -nographic \
    -L "$Q/root/usr/share/qemu" \
    -kernel "$K/boot/vmlinuz-lts" \
    -initrd "$K/boot/initramfs-lts" \
    -append "$ARGS" \
    "${DISK[@]}"
