#!/bin/bash
# Build web/pkg for the browser.
#
# Three non-default flags, all required by the JIT and all silent when missing:
#
#   --export-table     the generated block module imports the host's
#                      __indirect_function_table; without this there is none
#   --growable-table   otherwise the exported table has a fixed maximum and
#                      cannot be grown to hold blocks (RangeError on grow)
#   --keep-lld-exports wasm-bindgen strips __indirect_function_table as an
#                      LLD-synthesised internal, so the table vanishes between
#                      the raw wasm and the shim
#
# And one post-build edit: wasm-bindgen keeps the raw exports module-private,
# but the JIT needs the memory, the table, and the load*/store* that generated
# blocks import. Appending the export is simpler than fighting the generator.
set -e
cd "$(dirname "$0")"

RUSTFLAGS="-C link-arg=--export-table -C link-arg=--growable-table" \
    cargo build --release --target wasm32-unknown-unknown -p riscv-wasm

wasm-bindgen --keep-lld-exports --target web --out-dir web/pkg \
    target/wasm32-unknown-unknown/release/riscv_wasm.wasm

cat >> web/pkg/riscv_wasm.js <<'EOF'

// Appended by build-wasm.sh. The JIT links generated modules against the
// host's own memory and function table, and those are not otherwise reachable
// from outside this module.
export { wasm as __wasm };
EOF

echo "web/pkg rebuilt:"
ls -la web/pkg/riscv_wasm_bg.wasm
grep -c "__wasm" web/pkg/riscv_wasm.js >/dev/null && echo "raw exports exposed"
