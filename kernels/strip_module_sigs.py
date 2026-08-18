#!/usr/bin/env python3
"""Strip module signatures out of the initramfs -> boot/initramfs-nosig.

Why: the kernel is built with CONFIG_MODULE_SIG_ALL, so every module load runs
SHA-256 over the whole .ko plus an RSA verify. At interpreter speed that was a
measured slice of boot (sha256_blocks_generic + mpihelp_submul_1 dominating the
600M-1100M step window). CONFIG_MODULE_SIG_FORCE is NOT set, so a module with
no signature loads fine -- the kernel taints itself and skips the crypto.

A signed .ko ends with:
    [signature bytes][struct module_signature, 12 bytes]["~Module signature appended~\n"]
Stripping = truncating all three. The cpio is newc format: plain ASCII headers,
so rewriting sizes is mechanical.

Output is a separate gitignored file rather than an in-place edit: the pristine
initramfs stays the reference for tests, and this is cheap to regenerate.
"""

import gzip
import os
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = os.path.join(HERE, "boot", "initramfs-lts")
DST = os.path.join(HERE, "boot", "initramfs-nosig")

MAGIC = b"~Module signature appended~\n"


def strip_sig(data: bytes, name: str) -> bytes:
    if not data.endswith(MAGIC):
        return data
    # struct module_signature: u8 algo, hash, id_type, signer_len, key_id_len,
    # pad[3]; __be32 sig_len -- immediately before the magic string.
    info = data[-len(MAGIC) - 12 : -len(MAGIC)]
    signer_len, key_id_len = info[3], info[4]
    (sig_len,) = struct.unpack(">I", info[8:12])
    strip = len(MAGIC) + 12 + sig_len + signer_len + key_id_len
    if strip >= len(data):
        print(f"  !! {name}: malformed trailer, left alone")
        return data
    return data[:-strip]


def align4(n: int) -> int:
    return (n + 3) & ~3


def main() -> None:
    blob = gzip.open(SRC, "rb").read()
    out = bytearray()
    pos = 0
    stripped = saved = 0

    while pos + 110 <= len(blob):
        hdr = blob[pos : pos + 110]
        if hdr[:6] not in (b"070701", b"070702"):
            print(f"unexpected cpio magic at {pos:#x}", file=sys.stderr)
            sys.exit(1)
        f = [int(hdr[6 + i * 8 : 14 + i * 8], 16) for i in range(13)]
        filesize, namesize = f[6], f[11]
        name_at = pos + 110
        data_at = align4(name_at + namesize)
        name = blob[name_at : name_at + namesize - 1].decode()
        data = blob[data_at : data_at + filesize]
        next_at = align4(data_at + filesize)

        if name == "TRAILER!!!":
            out += blob[pos:next_at]
            break

        if name.endswith(".ko"):
            new = strip_sig(data, name)
            if len(new) != len(data):
                stripped += 1
                saved += len(data) - len(new)
                data = new
                f[6] = len(data)

        rebuilt = hdr[:6] + b"".join(b"%08X" % v for v in f)
        entry = rebuilt + blob[name_at : name_at + namesize]
        entry += b"\0" * (align4(len(entry)) - len(entry))
        entry += data
        entry += b"\0" * (align4(len(data)) - len(data))
        out += entry
        pos = next_at

    with gzip.open(DST, "wb", compresslevel=6) as g:
        g.write(bytes(out))
    print(
        f"{stripped} modules stripped, {saved // 1024} KiB of signatures removed\n"
        f"wrote {DST} ({os.path.getsize(DST) // (1024 * 1024)} MiB)"
    )


if __name__ == "__main__":
    main()
