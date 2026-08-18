#!/usr/bin/env python3
"""Build an ext4 root filesystem image for the emulator to boot from virtio-blk.

The contents come from the Alpine initramfs we already ship (busybox, musl, apk,
libapk and the full 6.18.35 module tree) so this needs no network access. The
result is a real, writable disk: changes made in the guest survive a reboot,
which is the whole point of having a block device.

    python3 mkrootfs.py [--size-mb 512] [--out rootfs.ext4]

Requires mke2fs (e2fsprogs >= 1.43, for `-d`). No root privileges needed.

Note on ownership: mke2fs -d takes uid/gid from the staging tree, so files end
up owned by the building user rather than root. The guest runs as root and root
bypasses permission checks, so this is cosmetic — but it is why `ls -l` in the
guest shows uid 1000. Install fakeroot and the wrapper below fixes it.
"""
import argparse
import gzip
import os
import shutil
import stat
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
INITRAMFS = os.path.join(HERE, "boot", "initramfs-lts")

# busybox applets to symlink up front. `busybox --install -s` would be more
# accurate but it is a riscv64 binary we cannot execute on the build host, so
# the first-boot init script re-runs it in the guest to catch anything missing.
APPLETS = """
ash sh cat chmod chown cp date dd df dmesg echo egrep false fgrep grep gunzip
gzip hostname kill ln login ls mkdir mknod mktemp more mount mv netstat nice
pidof ping ps pwd rm rmdir sed sleep stty su sync tar touch true umount uname
usleep vi zcat awk basename clear cut dirname du env expr find free head
hexdump id killall less md5sum mkfifo nl od printf realpath seq sort tail tee
test top tr uniq wc wget which whoami xargs yes init halt poweroff reboot
switch_root modprobe lsmod insmod rmmod mdev blkid findfs fdisk mkswap swapon
swapoff dmesg watch time tty setsid nproc uptime
""".split()


def read_cpio(path):
    """Yield (name, mode, data) from a gzipped newc cpio archive."""
    d = gzip.open(path, "rb").read()
    off = 0
    while off + 110 <= len(d):
        if d[off : off + 6] != b"070701":
            break
        fields = [int(d[off + 6 + i * 8 : off + 6 + (i + 1) * 8], 16) for i in range(13)]
        mode, filesize, namesize = fields[1], fields[6], fields[11]
        name = d[off + 110 : off + 110 + namesize - 1].decode()
        if name == "TRAILER!!!":
            break
        dstart = (off + 110 + namesize + 3) & ~3
        yield name, mode, d[dstart : dstart + filesize]
        off = dstart + ((filesize + 3) & ~3)


def extract(staging):
    for name, mode, data in read_cpio(INITRAMFS):
        if name in (".", "init"):
            continue  # the initramfs's own init is not the rootfs's init
        dest = os.path.join(staging, name)
        kind = mode & 0o170000
        if kind == 0o040000:
            os.makedirs(dest, exist_ok=True)
        elif kind == 0o120000:
            target = data.rstrip(b"\x00").decode()
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            if os.path.lexists(dest):
                os.remove(dest)
            os.symlink(target, dest)
        else:
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            with open(dest, "wb") as f:
                f.write(data)
            os.chmod(dest, mode & 0o7777)


def write(staging, path, content, mode=0o644):
    full = os.path.join(staging, path.lstrip("/"))
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "w", newline="\n") as f:
        f.write(content)
    os.chmod(full, mode)


def populate(staging):
    for d in ("proc", "sys", "dev", "tmp", "run", "root", "home", "mnt",
              "media", "var/log", "var/tmp", "etc/init.d", "etc/apk"):
        os.makedirs(os.path.join(staging, d), exist_ok=True)
    os.chmod(os.path.join(staging, "tmp"), 0o1777)

    # busybox applet symlinks. usr-merged: /bin and /sbin are symlinks into /usr.
    bb_bin = os.path.join(staging, "usr/bin")
    for applet in sorted(set(APPLETS)):
        link = os.path.join(bb_bin, applet)
        if not os.path.lexists(link):
            os.symlink("busybox", link)
    # A few live in sbin by convention; busybox does not care which path it is
    # invoked through, only argv[0].
    for applet in ("init", "halt", "poweroff", "reboot", "switch_root", "mdev",
                   "blkid", "findfs", "swapon", "swapoff"):
        link = os.path.join(staging, "usr/sbin", applet)
        if not os.path.lexists(link):
            os.symlink("../bin/busybox", link)

    # /sbin/init is what switch_root execs. It runs before anything else, so it
    # does the one thing the build host could not: regenerate the applet
    # symlinks by actually running the riscv64 busybox.
    write(staging, "sbin/rcS", """#!/bin/sh
/bin/busybox --install -s 2>/dev/null
mount -t proc     proc     /proc 2>/dev/null
mount -t sysfs    sysfs    /sys  2>/dev/null
mount -t devtmpfs devtmpfs /dev  2>/dev/null
mount -o remount,rw / 2>/dev/null
mkdir -p /dev/pts && mount -t devpts devpts /dev/pts 2>/dev/null
hostname -F /etc/hostname 2>/dev/null
echo
echo "Alpine on riscv-vm — root filesystem is /dev/vda (persistent)."
echo
""", 0o755)

    write(staging, "etc/inittab", """::sysinit:/sbin/rcS
::respawn:-/bin/sh
::ctrlaltdel:/sbin/reboot
::shutdown:/bin/umount -a -r
""")

    write(staging, "etc/fstab", """/dev/vda   /        ext4     rw,relatime  0 1
proc       /proc    proc     defaults     0 0
sysfs      /sys     sysfs    defaults     0 0
devtmpfs   /dev     devtmpfs defaults     0 0
tmpfs      /tmp     tmpfs    defaults     0 0
""")

    write(staging, "etc/hostname", "riscv-vm\n")
    write(staging, "etc/hosts", "127.0.0.1 localhost riscv-vm\n::1 localhost\n")
    write(staging, "etc/profile", """export PATH=/bin:/sbin:/usr/bin:/usr/sbin
export PS1='riscv-vm:\\w\\$ '
export HOME=/root
""")
    # Points at the real Alpine CDN. Nothing can reach it until virtio-net and
    # the WISP relay land, but having it correct now means `apk update` is the
    # single command that proves networking end to end.
    write(staging, "etc/apk/repositories",
          "https://dl-cdn.alpinelinux.org/alpine/v3.24/main\n"
          "https://dl-cdn.alpinelinux.org/alpine/v3.24/community\n")
    write(staging, "etc/apk/arch", "riscv64\n")
    write(staging, "etc/resolv.conf", "nameserver 1.1.1.1\nnameserver 8.8.8.8\n")
    write(staging, "etc/alpine-release", "3.24.1\n")
    write(staging, "etc/os-release",
          'NAME="Alpine Linux"\nID=alpine\nVERSION_ID=3.24.1\n'
          'PRETTY_NAME="Alpine Linux v3.24 (riscv-vm)"\n')
    os.makedirs(os.path.join(staging, "lib/apk/db"), exist_ok=True)
    write(staging, "lib/apk/db/installed", "")
    write(staging, "lib/apk/db/lock", "")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--size-mb", type=int, default=512)
    ap.add_argument("--out", default=os.path.join(HERE, "rootfs.ext4"))
    ap.add_argument("--staging", default="/tmp/riscv-vm-rootfs")
    args = ap.parse_args()

    if not os.path.exists(INITRAMFS):
        sys.exit(f"missing {INITRAMFS}")

    staging = args.staging
    if os.path.exists(staging):
        shutil.rmtree(staging)
    os.makedirs(staging)

    print(f"staging  {staging}")
    extract(staging)
    populate(staging)

    nfiles = sum(len(f) + len(d) for _, d, f in os.walk(staging))
    print(f"contents {nfiles} entries")

    out = args.out
    if os.path.exists(out):
        os.remove(out)
    with open(out, "wb") as f:
        f.truncate(args.size_mb * 1024 * 1024)

    cmd = ["mke2fs", "-q", "-F", "-t", "ext4", "-L", "riscv-vm-root",
           "-d", staging,
           # Keep the on-disk format conservative: the guest kernel is stock
           # Alpine lts, but metadata_csum_seed and orphan_file are recent and
           # there is no upside to depending on them here.
           "-O", "^metadata_csum_seed,^orphan_file",
           out]
    if shutil.which("fakeroot"):
        cmd = ["fakeroot"] + cmd
        print("using fakeroot so files are owned by root")
    subprocess.run(cmd, check=True)

    size = os.path.getsize(out)
    print(f"wrote    {out} ({size // (1024*1024)} MiB, {size // 512} sectors)")


if __name__ == "__main__":
    main()
