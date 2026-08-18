#!/bin/bash
# Build the working tree and the committed baseline into separate shim dirs, so
# jit-ab.py can alternate between them.
set -e
cd ~/riscv-vm

build() {
    RUSTFLAGS="-C link-arg=--export-table -C link-arg=--growable-table" \
        cargo build --release -q --target wasm32-unknown-unknown -p riscv-wasm
    wasm-bindgen --keep-lld-exports --target nodejs --out-dir "$1" \
        target/wasm32-unknown-unknown/release/riscv_wasm.wasm
    printf '\nmodule.exports.__wasm = wasm;\n' >> "$1/riscv_wasm.js"
    echo "built $1"
}

build /tmp/jitnode_new

git stash -q
build /tmp/jitnode_old
git stash pop -q

echo "both built"
