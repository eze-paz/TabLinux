#!/bin/bash
# Compare generated code size between the working tree and the committed
# baseline, for exactly the same 400 blocks.
#
# Wall-clock on this box cannot resolve a 10-20% change: paired runs of the same
# two builds gave ratios from 0.66 to 1.23. Code size can, because it is
# deterministic -- the difftest generates the same blocks from the same seed
# every time, so any difference is the codegen and nothing else.
#
# It is not a direct proxy for speed: fewer bytes of wasm is not automatically
# faster, and a register held in a local is cheaper than one reloaded from
# memory even at equal instruction count. But a large move in either direction
# says something real about how much work the generated code is doing.
set -e
cd ~/riscv-vm

cargo run --release -q -p riscv-jit --example difftest >/dev/null 2>&1
new=$(stat -c%s /tmp/jit_multi.wasm)

git stash -q
cargo run --release -q -p riscv-jit --example difftest >/dev/null 2>&1
old=$(stat -c%s /tmp/jit_multi.wasm)
git stash pop -q

# Regenerate so the modules on disk match the working tree again.
cargo run --release -q -p riscv-jit --example difftest >/dev/null 2>&1

echo "400 blocks, multi-block module:"
echo "  baseline       $old bytes"
echo "  working tree   $new bytes"
awk -v o="$old" -v n="$new" 'BEGIN { printf "  change         %+.1f%%\n", (n-o)*100.0/o }'
