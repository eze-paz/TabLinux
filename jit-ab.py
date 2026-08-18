#!/usr/bin/env python3
"""Interleaved A/B for two JIT builds.

This box's spread within a single build is larger than most of the effects worth
measuring: four runs of identical code gave speedups of 4.87, 3.77, 6.26 and
5.99. Running one build then the other and comparing is therefore meaningless,
and has already produced a confident wrong answer once in this project.

So the two builds alternate, in ABBA order so that drift linear in time cancels
rather than landing on whichever went second, and the verdict comes from the
median of the paired ratios rather than from either build's absolute numbers.

    RUSTFLAGS=... cargo build ... && wasm-bindgen ... --out-dir /tmp/jitnode_new
    git stash && (same) --out-dir /tmp/jitnode_old && git stash pop
    ./jit-ab.py [blocks]
"""
import os
import re
import statistics
import subprocess
import sys

BLOCKS = int(sys.argv[1]) if len(sys.argv) > 1 else 3


def run(which):
    """One measured run against a build; returns its JIT MIPS."""
    env = dict(os.environ, JITNODE=which)
    out = subprocess.run(["node", "jit-vm-test.js"], capture_output=True,
                         text=True, env=env).stdout
    m = re.search(r"^jit:\s+([0-9.]+) MIPS", out, re.M)
    if not m:
        sys.exit("no result from %s:\n%s" % (which, out[-800:]))
    if "OK: console output identical" not in out:
        sys.exit("%s produced different guest output -- correctness, not speed" % which)
    return float(m.group(1))


pairs = []
for i in range(BLOCKS):
    # ABBA: whichever build runs second in the first half runs first in the
    # second, so a machine that is steadily speeding up or slowing down does not
    # favour one of them.
    a1, b1 = run("/tmp/jitnode_old"), run("/tmp/jitnode_new")
    b2, a2 = run("/tmp/jitnode_new"), run("/tmp/jitnode_old")
    for old, new, tag in ((a1, b1, "AB"), (a2, b2, "BA")):
        pairs.append(new / old)
        print("block %d%s: old %5.1f  new %5.1f  ratio %.3f" % (i + 1, tag, old, new, new / old))

med = statistics.median(pairs)
print()
print("pairs: " + ", ".join("%.3f" % p for p in pairs))
print("median ratio %.3f" % med)
if abs(med - 1) < 0.05:
    print("verdict: no resolvable difference (under 5%)")
else:
    print("verdict: new is %.0f%% %s" % (abs(med - 1) * 100,
                                         "faster" if med > 1 else "SLOWER"))
