#!/bin/bash
# Build the working tree into a shim dir jit-vm-test.js can load.
#
# `set -e` alone is not enough here: wasm-bindgen will happily run on the
# PREVIOUS artifact if cargo failed, and the script then prints success while
# the harness silently measures stale code. That already cost one confusing
# round -- a histogram that never appeared because the build it lived in was
# never loaded. So check cargo explicitly and stamp the output.
set -euo pipefail
cd ~/riscv-vm

OUT="${1:-/tmp/jitnode}"
WASM=target/wasm32-unknown-unknown/release/riscv_wasm.wasm

RUSTFLAGS="-C link-arg=--export-table -C link-arg=--growable-table" \
    cargo build --release -q --target wasm32-unknown-unknown -p riscv-wasm

# Refuse to ship anything cargo did not just produce.
if [ ! -f "$WASM" ] || [ "$WASM" -ot Cargo.toml ]; then
    echo "no fresh wasm artifact" >&2
    exit 1
fi

wasm-bindgen --keep-lld-exports --target nodejs --out-dir "$OUT" "$WASM"
printf '\nmodule.exports.__wasm = wasm;\n' >> "$OUT/riscv_wasm.js"
echo "built $OUT  ($(stat -c%s "$OUT/riscv_wasm_bg.wasm") bytes)"
