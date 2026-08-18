const fs = require("fs");
const mod = require(process.env.HOME + "/" + (process.env.DIR || "ab_hoist2") + "/riscv_wasm.js");
const exp = mod.__wasm;
const vm = mod.Vm.restore(fs.readFileSync("kernels/shell.snap"));
vm.jit_enable(true);
vm.input(Buffer.from(process.env.WORK || "yes | sha256sum > /dev/null\n"));
const table = exp.__indirect_function_table;
function pump() {
  const c = vm.jit_pending(); if (!c) return;
  const base = table.length; table.grow(c);
  const b = vm.jit_build(base); if (!b || !b.length) return;
  new WebAssembly.Instance(new WebAssembly.Module(b), { env: {
    mem: exp.memory, __indirect_function_table: table,
    load8u: exp.load8u, load16u: exp.load16u, load32u: exp.load32u, load64: exp.load64,
    store8: exp.store8, store16: exp.store16, store32: exp.store32, store64: exp.store64,
    csr: exp.csr, fp: exp.fp,
    table_base: new WebAssembly.Global({ value: "i32", mutable: false }, base) } });
  vm.jit_installed(base);
}
let s = 0;
for (let i = 0; i < 50; i++) { s += vm.run(2_000_000); pump(); }
const h = vm.jit_hoist_stats();
console.log((process.env.LABEL || "WORK").padEnd(10),
  "ran", (s / 1e6).toFixed(0) + "M | groups formed:", h[0], "strided(hoisted):", h[1],
  "->", h[0] ? (100 * h[1] / h[0]).toFixed(1) + "% of groups hoisted" : "n/a");
