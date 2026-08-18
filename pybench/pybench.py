# Deterministic pure-Python workload for the emulator A/B rig.
#
# The host times the wall clock between the PYBENCH markers on the console and
# counts retired guest instructions over the same window, so nothing here reads
# a clock — guest clocks under emulation are not trustworthy and the workload
# must be identical run to run.
#
# Phases cover the shapes CPython spends its time on: function-call dispatch,
# integer arithmetic, dict/list traffic, string building, and attribute access.
# Scale with argv[1] (default 1).

import sys

SCALE = int(sys.argv[1]) if len(sys.argv) > 1 else 1

def fib(n):
    return n if n < 2 else fib(n - 1) + fib(n - 2)

def calls(n):
    s = 0
    for i in range(n):
        s += fib(12)
    return s

def arith(n):
    s = 0
    for i in range(n):
        s += (i * 3 + 7) % 11 - (i >> 2)
    return s

def dicts(n):
    d = {}
    for i in range(n):
        d[i & 1023] = i
        s = d.get((i * 7) & 1023, 0)
    return len(d), s

def lists(n):
    l = list(range(256))
    s = 0
    for i in range(n):
        l[i & 255] = i
        s += l[(i * 3) & 255]
    return s

def strings(n):
    parts = []
    for i in range(n):
        parts.append("x%d" % (i & 63))
        if len(parts) > 64:
            "".join(parts)
            parts.clear()
    return len(parts)

def bigints(n):
    # Multi-limb integer arithmetic: CPython's long multiply is built on the
    # mulhu/mulh instructions, which the JIT interpreted until 2026-08-13.
    m = (1 << 191) - 19
    x = 3 ** 137
    s = 0
    for i in range(n):
        x = (x * x + i) % m
        s ^= x & 0xFFFF
    return s

def memwalk(n):
    # Pseudo-random touches over 8 MiB = 2048 guest pages: more than a
    # 1024-entry inline TLB can hold, comfortably inside 4096. Index math is
    # small-int only, so this isolates data-TLB behavior from bigint cost.
    b = bytearray(8 * 1024 * 1024)
    mask = len(b) - 1
    idx = 12345
    s = 0
    for i in range(n):
        idx = (idx * 1103515245 + 12345) & mask
        b[idx] = i & 255
        s += b[(idx * 7) & mask]
    return s

def mmaps(n):
    # Map, touch, unmap. Every close() is a munmap whose TLB shootdown Linux
    # issues as per-page sfence.vma — the pattern CPython's allocator produces
    # constantly on real workloads (147k single-page sfences per measured
    # session), each of which used to wipe the JIT's entire chain table.
    # 16 KiB maps: a munmap this small stays under Linux's flush-all
    # threshold, so the shootdown is per-page sfence.vma — the shape real
    # sessions show (69.4k single-page vs 189 global). A 256 KiB map went
    # global on every munmap and measured a different problem entirely.
    import mmap
    s = 0
    for i in range(n):
        m = mmap.mmap(-1, 16384)
        m[0] = i & 255
        m[8192] = 1
        s += m[0] + m[8192]
        m.close()
    return s

class P:
    __slots__ = ("x", "y")
    def __init__(self):
        self.x = 1
        self.y = 2
    def step(self):
        self.x, self.y = self.y, self.x + self.y

def attrs(n):
    p = P()
    for i in range(n):
        p.step()
        if p.x > 1 << 60:
            p.x, p.y = 1, 2
    return p.x

# argv[2] (optional) runs a single phase, for microbenchmarks where one cost
# should dominate the window instead of being averaged away.
ONLY = sys.argv[2] if len(sys.argv) > 2 else None
PHASES = [
    ("calls", calls, 60),
    ("arith", arith, 30000),
    ("dicts", dicts, 20000),
    ("lists", lists, 20000),
    ("strings", strings, 10000),
    ("attrs", attrs, 20000),
    ("bigints", bigints, 1500),
    ("memwalk", memwalk, 120000),
    ("mmaps", mmaps, 250),
]

print("PYBENCH START", flush=True)
r = []
for k, (name, fn, base) in enumerate(PHASES):
    if ONLY and name != ONLY:
        continue
    r.append(fn(base * SCALE))
    if k + 1 < len(PHASES) and not ONLY:
        print(f"PYBENCH {name} done", flush=True)
print("PYBENCH END", r, flush=True)
