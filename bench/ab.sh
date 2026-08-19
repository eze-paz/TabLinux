#!/bin/bash
# Interleaved A/B. Sequential comparison is unsound on this box: the host load
# drifts over the minutes a run takes, so "measure old, then measure new" can
# report a regression that is really just the laptop getting busier. Alternating
# the two binaries makes both see the same drift.
#
#   cargo build --release -p riscv-machine --example bench
#   cp target/release/examples/bench /tmp/bench_new    # with your change
#   git stash && cargo build ... && cp ... /tmp/bench_old && git stash pop
#   ./ab.sh [pairs]
N=${1:-4}
for i in $(seq 1 "$N"); do
    o=$(/tmp/bench_old 2>/dev/null | awk -F= '/^BENCH/{print $2}')
    n=$(/tmp/bench_new 2>/dev/null | awk -F= '/^BENCH/{print $2}')
    echo "pair $i: old=$o new=$n"
done
