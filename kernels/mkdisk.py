#!/usr/bin/env python3
"""Build kernels/disk-ext4.img.gz — a blank ext4 for the browser VM's disk.

Formatting host-side rather than in the guest is a deliberate choice. mkfs
inside the VM would mean apk-installing e2fsprogs over the network first, then
running it on an emulated CPU, every time someone starts from a clean OPFS —
minutes of work to produce a filesystem that is byte-identical every time.
Doing it here makes it deterministic and instant, and the guest needs no
filesystem tools at all.

The image is nearly all zeros, so it gzips from 256 MiB to well under a
megabyte; the worker inflates it into OPFS once, the first time it sees an
unformatted disk.

Size must match DISK_MB in make_snapshot.rs and vm-worker.js — virtio-blk
refuses a capacity mismatch on restore.
"""

import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DISK_MB = 256
IMG = os.path.join(HERE, "disk-ext4.img")


def main() -> None:
    seed = os.path.join(HERE, "_diskseed")
    os.makedirs(seed, exist_ok=True)
    # A marker so a mounted-and-empty disk is distinguishable from a disk that
    # was never formatted — the two look identical from the guest otherwise.
    with open(os.path.join(seed, "README"), "w") as f:
        f.write("persistent disk for the browser VM (kernels/mkdisk.py)\n")

    with open(IMG, "wb") as f:
        f.truncate(DISK_MB * 1024 * 1024)

    # -F: it is a plain file, not a block device.
    # -d: populate from the seed directory.
    # -O flags:
    #   ^has_journal — the disk is small and a journal only adds writes through a
    #     synchronous OPFS handle.
    #   ^metadata_csum — this was ~72% of the post-restore setup time. Every
    #     overlay copy-up and apk write touches ext4 metadata (inode tables,
    #     bitmaps, group descriptors), and with metadata_csum the guest recomputes
    #     and verifies a crc32c on each block IN SOFTWARE (this RISC-V core has no
    #     CRC instruction) — measured 27M of 36M guest instructions. The feature
    #     guards metadata integrity against unreliable storage, which does not
    #     apply here: the "disk" is an OPFS-backed emulated block device (the
    #     medium is already integrity-checked), the only data on it is
    #     reconstructible (kernel modules + the apk overlay cache; real files live
    #     in Dropbox over 9p), and a corrupt image is trivially re-seeded. So the
    #     safety it buys is moot and the cost is most of the boot — dropped.
    subprocess.run(
        ["mke2fs", "-q", "-F", "-t", "ext4", "-O", "^has_journal,^metadata_csum",
         "-L", "riscv-vm", "-d", seed, IMG],
        check=True,
    )
    # A freshly written ext4 has never been checked, so the guest greets every
    # mount with "mounting unchecked fs, running e2fsck is recommended".
    # Harmless, but it is the first thing anyone notices, and a warning people
    # learn to ignore is worse than no warning. One pass here clears it for
    # good, since every browser starts from this same image.
    #
    # e2fsck exits 0 for "clean" and 1 for "errors corrected"; both are fine on
    # an image we just created. Anything above that is a real failure.
    check = subprocess.run(["e2fsck", "-fp", IMG], capture_output=True, text=True)
    if check.returncode > 1:
        sys.exit(f"e2fsck failed ({check.returncode}): {check.stdout}{check.stderr}")

    subprocess.run(["gzip", "-kf9", IMG], check=True)

    raw = os.path.getsize(IMG)
    gz = os.path.getsize(IMG + ".gz")
    print(f"{IMG}: {raw // (1024 * 1024)} MiB -> {gz // 1024} KiB gzipped", file=sys.stderr)


if __name__ == "__main__":
    main()
