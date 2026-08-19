# Guest OS images (bundled)

This directory carries the prebuilt Alpine riscv64 images the browser front-end
needs, so the VM boots out of the box and the snapshot resumes without a kernel:

| file | purpose | size |
|------|---------|------|
| `vmlinuz-lts.raw` | Alpine LTS linux kernel (riscv64), raw image | ~22 MB |
| `boot/initramfs-lts` | initramfs with the rescue-shell userspace | ~6 MB |
| `boot.dtb` | device tree (UART, virtio-mmio, CLINT, PLIC) | ~7 KB |
| `disk-ext4.img.gz` | seed ext4 disk (9p kernel modules under /mod) | ~390 KB |
| `shell.snap.gz` | a RAM snapshot at the shell — resume loads only this | ~15 MB |

The web front-end fetches these by relative path (`../kernels/...`), so they must
be served alongside `web/` — which any static host does if the whole repo is
served. The snapshot is cache-busted by a version string in `vm-worker.js`
(`SNAP_VERSION`, `DISK_SEED_VERSION`); bump it when a bundled image changes.

## Licensing

These are **NOT part of this project's own source** and retain their own
licenses: the Linux kernel is GPL-2.0 (stock Alpine LTS build — corresponding
source is available from the Alpine Linux project and kernel.org); the
initramfs/disk carry Alpine packages (busybox, musl, apk-tools, CPython) under
their respective licenses. See [`NOTICE`](../NOTICE) for the full breakdown.
