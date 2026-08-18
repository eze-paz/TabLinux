#!/usr/bin/env python3
"""Build kernels/disk-python.img: a 256 MiB ext4 carrying Alpine's riscv64
CPython, for benchmarking Python inside the emulator.

Downloads python3 and its transitive dependencies from the Alpine mirror,
extracts them into a staging tree, adds /bench scripts, and formats the tree
into an ext4 image with mke2fs -d (same recipe as mkrootfs.py — no root
privileges needed). The guest mounts it and runs:

    mount -t ext4 /dev/vda /mnt/disk
    LD_LIBRARY_PATH=/mnt/disk/usr/lib PYTHONHOME=/mnt/disk/usr \
        /mnt/disk/usr/bin/python3 /mnt/disk/bench/pybench.py

Size must stay 256 MiB: virtio-blk refuses a capacity mismatch on restore.
"""
import io
import os
import shutil
import subprocess
import sys
import tarfile
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
MIRROR = "https://dl-cdn.alpinelinux.org/alpine/v3.24/main/riscv64"
STAGE = os.path.join(HERE, "_pystage")
IMG = os.path.join(HERE, "disk-python.img")
DISK_MB = 256

def fetch(url: str) -> bytes:
    sys.stderr.write(f"fetch {url}\n")
    with urllib.request.urlopen(url, timeout=60) as r:
        return r.read()

def parse_index(raw: bytes):
    """APKINDEX: blank-line-separated stanzas of single-letter fields."""
    pkgs = {}       # name -> (version, deps, provides)
    provides = {}   # provided name (so:..., cmd:..., alias) -> package name
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:gz") as t:
        text = t.extractfile("APKINDEX").read().decode()
    for stanza in text.split("\n\n"):
        f = {}
        for line in stanza.splitlines():
            if len(line) > 2 and line[1] == ":":
                f[line[0]] = line[2:]
        if "P" not in f:
            continue
        name, ver = f["P"], f["V"]
        deps = [d for d in f.get("D", "").split() if d]
        provs = [p for p in f.get("p", "").split() if p]
        pkgs[name] = (ver, deps, provs)
        for p in provs:
            provides.setdefault(p.split("=")[0], name)
    return pkgs, provides

def resolve(pkgs, provides, roots):
    """BFS over D: fields, mapping so:/cmd:/alias deps through p: provides."""
    seen, order, queue = set(), [], list(roots)
    while queue:
        want = queue.pop(0).split("=")[0].split(">")[0].split("<")[0].split("~")[0]
        if want.startswith("!"):
            continue
        name = want if want in pkgs else provides.get(want)
        if name is None:
            sys.stderr.write(f"  (no provider for {want}, skipping)\n")
            continue
        if name in seen:
            continue
        seen.add(name)
        order.append(name)
        queue.extend(pkgs[name][1])
    return order

def extract_apk(data: bytes, dest: str):
    """An .apk is a gzipped tar (concatenated segments; tarfile with
    ignore_zeros reads through all of them). Skip the .PKGINFO/.SIGN metadata."""
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz", ignore_zeros=True) as t:
        for m in t:
            if m.name.startswith(".") or m.name.startswith("sbin/ldconfig"):
                continue
            if m.isdev():
                continue
            m.name = m.name.lstrip("/")
            try:
                t.extract(m, dest, set_attrs=False)
                if m.isfile():
                    os.chmod(os.path.join(dest, m.name), 0o755 if (m.mode & 0o111) else 0o644)
            except Exception as e:
                sys.stderr.write(f"  skip {m.name}: {e}\n")

def main():
    shutil.rmtree(STAGE, ignore_errors=True)
    os.makedirs(STAGE, exist_ok=True)

    pkgs, provides = parse_index(fetch(f"{MIRROR}/APKINDEX.tar.gz"))
    order = resolve(pkgs, provides, ["python3"])
    # musl/busybox layers are already in the initramfs root; keep them anyway
    # (harmless, the guest uses /mnt/disk/usr/lib via LD_LIBRARY_PATH).
    sys.stderr.write(f"packages: {' '.join(order)}\n")
    for name in order:
        ver = pkgs[name][0]
        extract_apk(fetch(f"{MIRROR}/{name}-{ver}.apk"), STAGE)

    # Benchmark scripts ride on the same disk.
    bench_src = os.path.join(HERE, "..", "pybench")
    if os.path.isdir(bench_src):
        shutil.copytree(bench_src, os.path.join(STAGE, "bench"))

    if os.path.exists(IMG):
        os.remove(IMG)
    subprocess.run(
        ["mke2fs", "-q", "-t", "ext4", "-d", STAGE, "-E", "no_copy_xattrs",
         IMG, f"{DISK_MB}m"],
        check=True,
    )
    sys.stderr.write(f"wrote {IMG}\n")

if __name__ == "__main__":
    main()
