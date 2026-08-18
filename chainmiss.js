const fs = require("fs");
const dir = process.env.DIR;
const mod = require(process.env.HOME + "/" + dir + "/riscv_wasm.js");
const exp = mod.__wasm;
const vm = mod.Vm.restore(fs.readFileSync("kernels/shell.snap"));
vm.jit_enable(true);
if (vm.interp_hist_enable) vm.interp_hist_enable(true);
vm.input(Buffer.from(process.env.WORK || "yes | gzip -c > /dev/null\n"));
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
for (let i = 0; i < 60; i++) { s += vm.run(2_000_000); pump(); }
const NAMES = ["slot empty", "evicted", "gen priv", "gen trans", "gen both", "wasm budget"];
const m = vm.chain_miss();
const total = m.reduce((a, b) => a + b, 0);
const g = vm.gen_bump();
console.log(dir.padEnd(8), "ran", (s / 1e6).toFixed(0) + "M | satp", g[0], "sfence-glob", g[1], "| chain misses", total.toFixed(0));
NAMES.forEach((n, i) => console.log("   " + n.padEnd(12), m[i].toFixed(0), "(" + (100 * m[i] / total).toFixed(1) + "%)"));
