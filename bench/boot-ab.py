#!/usr/bin/env python3
"""Interleaved A/B on the cold-boot workload.

Why this exists: I once measured the decode cache's size on boot by running
8192, then 32768, then 2048, read 96.6s / 119.1s / 109.6s as a size effect, and
then re-ran 8192 and got 136.8s. The host drifts more over the minutes a boot
takes than the differences being read off it. Sequential comparison of two
builds is not a weak method here, it is a broken one.

Two things fix it.

*Interleaving, in ABBA order*: run old,new then new,old, so drift that is
linear in time cancels instead of landing entirely on whichever binary runs
second.

*A confidence interval, and the honesty to say "unresolved"*: this is the part
that matters. The noise here comes from the Windows host scheduling WSL's vCPUs
onto physical cores, which is invisible and uncontrollable from inside the VM --
all twelve vCPUs benchmark within 17-19 MIPS, so there is no P/E split to pin
around, and taskset only makes throughput worse. Null runs on *identical binaries* have
returned 1.000x, 0.904x, 1.086x, and -- with ABBA, a shortened boot and 25s
cooldowns -- 1.080x with a 95% CI of [1.000, 1.160] whose four blocks were all
above 1.0.

That last one matters and is not just noise: it is a *systematic* bias of about
8% in the null condition, direction consistent across blocks, and I could not
find its cause. Order is not it (ABBA), cold page cache is not it (the effect
survives swapping which binary runs first), and the slices are verified to be
executing identical work (all 220 pair up on step count).

Two later fixes shrank that band a long way, and both are about removing
variance rather than averaging over it:

*Host normalisation* (in bench.rs): each slice is timed next to a fixed lump of
reference work, and scaled by how slow that reference ran. CPU time is immune to
being descheduled but not to running at a lower clock, and on a thermally
limited laptop under a hypervisor the clock moves constantly. Hardware counters
would have settled it, but WSL2 does not virtualise the PMU --
perf_event_open(PERF_TYPE_HARDWARE) returns ENOENT -- so a local yardstick is
the substitute.

*Outlier rejection* (OUTLIER below): a block whose runs took far longer than the
fastest run seen was measuring the rest of the machine, not this change.

NULL_BIAS still encodes a refusal to call anything inside it a win, because the
interval measures spread between blocks and cannot see a bias present in every
block.

Practical consequence: this tool can confirm a large change -- it read the
decode cache at 1.29x, well outside the band -- and cannot adjudicate a small
one. Devirtualising the bus measured ~3%; that is inside the band and was left
unproven rather than banked. Resolving effects that size needs hardware
counters, or a quiet machine that is not a thermally-limited laptop under a
hypervisor.

*Pairing*: a cold boot is deterministic, so slice `i` executes the identical
instruction sequence in both builds. Comparing slice `i` to slice `i` gives a
ratio that means something on its own, and the median over hundreds of those
ratios is unmoved by the handful of slices where the host stole a core. This is
far more sensitive than comparing whole-run totals, which average the
interference back in.

Usage:
    cargo build --release -p riscv-machine --example bench
    cp target/release/examples/bench /tmp/bench_new
    git stash && cargo build --release -p riscv-machine --example bench \\
      && cp target/release/examples/bench /tmp/bench_old && git stash pop
    ./boot-ab.py [blocks]      # each block is 4 boots (ABBA); default 1

A boot is ~1.2G instructions and takes minutes, so this is the confirmation
instrument. Iterate with `bench user`, confirm here.
"""
import subprocess
import statistics
import sys
import csv
import os
import time
import math

# Seconds idle between boots. Back-to-back runs heat the package until it
# throttles mid-measurement, which shows up as the second run of a pair being
# slower regardless of which binary it is -- a bias ABBA cancels only if the
# drift is linear, and thermal throttling is not. Idling between runs keeps
# each one starting from a comparable clock.
COOLDOWN = 25

# Width of the band in which this instrument has produced false positives on
# identical binaries (measured: 1.080x with a 95% CI of [1.000, 1.160]). A
# little wider than that observation, because one null run is a lower bound on
# the bias, not a measurement of it. Re-derive it by running the null test --
# copy the same binary to /tmp/bench_old and /tmp/bench_new -- rather than
# trusting this constant.
NULL_BIAS = 0.06

# A block is thrown away if either of its two runs took more than this multiple
# of the fastest run seen. Interference can only ever make a run slower, so an
# unusually slow one is contaminated rather than informative -- keeping it does
# not average out, it just widens the interval until nothing is resolvable.
#
# Measured: in a 6-block null run, 10 of 12 runs landed within 14.8-15.9s while
# two came in at 25.0s and 30.2s. Those two produced block ratios of 0.602 and
# 2.014 and were the entire reason that run could not resolve anything.
OUTLIER = 1.25

PAIRS = int(sys.argv[1]) if len(sys.argv) > 1 else 2


def run(binary, csv_path):
    """Boot once, returning (per-slice cpu seconds, total cpu seconds)."""
    time.sleep(COOLDOWN)
    out = subprocess.run([binary, "boot", csv_path], capture_output=True, text=True)
    total = None
    for line in out.stdout.splitlines():
        if line.startswith("BOOTCPU"):
            total = float(line.split("secs=")[1].split()[0])
    if total is None:
        sys.exit("%s produced no BOOTCPU line:\n%s\n%s" % (binary, out.stdout, out.stderr[-2000:]))
    with open(csv_path) as fh:
        rows = [(int(r["slice"]), int(r["steps"]), float(r["cpu_secs"]))
                for r in csv.DictReader(fh)]
    return rows, total


def compare(old, new):
    """Median per-slice speedup. >1 means new is faster."""
    if len(old) != len(new):
        # Determinism is the premise of the pairing; if it does not hold, the
        # comparison is meaningless and saying so is better than a number.
        print("  WARNING: %d old slices vs %d new -- boot diverged, so the "
              "pairing is invalid. Treat the ratio as unusable and find out "
              "why the instruction stream changed." % (len(old), len(new)))
    ratios = []
    for (_, s_old, t_old), (_, s_new, t_new) in zip(old, new):
        if s_old != s_new:
            continue  # different work in this slice; not comparable
        if t_old > 0 and t_new > 0:
            ratios.append(t_old / t_new)
    return ratios


def main():
    if not (os.path.exists("/tmp/bench_old") and os.path.exists("/tmp/bench_new")):
        sys.exit("build /tmp/bench_old and /tmp/bench_new first (see the docstring)")

    all_ratios, totals, block_medians, blocks = [], [], [], []
    for i in range(1, PAIRS + 1):
        # ABBA: old,new then new,old. Each block contributes one comparison, and
        # a drift that would inflate the second run in block A deflates it in
        # block B.
        for order in ("AB", "BA"):
            if order == "AB":
                old, t_old = run("/tmp/bench_old", "/tmp/boot_old.csv")
                new, t_new = run("/tmp/bench_new", "/tmp/boot_new.csv")
            else:
                new, t_new = run("/tmp/bench_new", "/tmp/boot_new.csv")
                old, t_old = run("/tmp/bench_old", "/tmp/boot_old.csv")
            ratios = compare(old, new)
            if not ratios:
                sys.exit("no comparable slices")
            med = statistics.median(ratios)
            all_ratios += ratios
            block_medians.append(med)
            totals.append((t_old, t_new))
            blocks.append((med, t_old, t_new, "%d%s" % (i, order)))
            print("pair %d%s: %d paired slices, median speedup %.3fx  "
                  "(totals %.1fs -> %.1fs, %.3fx)"
                  % (i, order, len(ratios), med, t_old, t_new, t_old / t_new))

    # Discard contaminated blocks before doing any statistics on them.
    floor = min(min(t_o, t_n) for _, t_o, t_n, _ in blocks)
    kept = [b for b in blocks if max(b[1], b[2]) <= OUTLIER * floor]
    dropped = [b for b in blocks if b not in kept]
    for med_d, t_o, t_n, tag in dropped:
        print("  dropped block %s: %.1fs/%.1fs vs %.1fs floor -- contaminated"
              % (tag, t_o, t_n, floor))
    if len(kept) < 2:
        print("only %d clean block(s) of %d; the machine was too busy for this "
              "run to mean anything. Re-run when it is idle."
              % (len(kept), len(blocks)))
        return
    block_medians = [b[0] for b in kept]
    totals = [(b[1], b[2]) for b in kept]

    to = sum(t for t, _ in totals) / len(totals)
    tn = sum(t for _, t in totals) / len(totals)
    print()
    print("mean total cpu %.1fs -> %.1fs (%.3fx)  [%d/%d blocks kept]"
          % (to, tn, to / tn, len(kept), len(blocks)))

    # The block medians are the independent observations, not the individual
    # slices: slices within a block share a scheduling environment, so treating
    # 440 of them as 440 samples would claim a precision that is not there.
    n = len(block_medians)
    point = statistics.mean(block_medians)
    if n < 2:
        print("point estimate %.3fx from a single block -- no interval, so no "
              "verdict. Run at least 2 blocks." % point)
        return

    sd = statistics.stdev(block_medians)
    # Two-sided 95% t critical values; beyond the table the normal value is
    # close enough given everything else here is approximate.
    TCRIT = {2: 12.71, 3: 4.30, 4: 3.18, 5: 2.78, 6: 2.57, 7: 2.45, 8: 2.36,
             9: 2.31, 10: 2.26, 11: 2.23, 12: 2.20}
    t = TCRIT.get(n, 2.10)
    half = t * sd / math.sqrt(n)
    lo, hi = point - half, point + half
    print("blocks: " + ", ".join("%.3f" % b for b in block_medians))
    print("speedup %.3fx   95%% CI [%.3f, %.3f]   (%d blocks)" % (point, lo, hi, n))

    if abs(point - 1.0) < NULL_BIAS:
        # Deliberately checked before the confidence interval. A null run has
        # already produced 1.080x with a CI that excluded 1.0, so a tight
        # interval inside this band is evidence of nothing.
        print("verdict: UNRESOLVED -- %.1f%% is inside the +/-%.0f%% band where "
              "this instrument has produced false positives on identical "
              "binaries." % ((point - 1) * 100, NULL_BIAS * 100))
        print("         More blocks will not fix this; the bias is in every "
              "block, not in the spread between them.")
    elif lo <= 1.0 <= hi:
        print("verdict: UNRESOLVED -- the interval spans 1.0, so this run cannot "
              "tell a real effect from noise.")
        if point != 1.0 and sd > 0:
            need = (t * sd / abs(point - 1.0)) ** 2
            print("         to resolve an effect this size you would need roughly "
                  "%d blocks; you ran %d." % (max(2, math.ceil(need)), n))
    else:
        print("verdict: new is %.1f%% %s (95%% CI %.1f%%..%.1f%%)"
              % (abs(point - 1) * 100, "faster" if point > 1 else "SLOWER",
                 (lo - 1) * 100, (hi - 1) * 100))


main()
