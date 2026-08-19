// Steady-state MIPS on the baseline JIT build, best-of-N slices to shed noise.
// Run under different V8 tiering flags to see whether TurboFan tier-up of the
// JIT block functions actually moves the hot path.
const fs = require("fs");
const mod = require(process.env.HOME + "/" + (process.env.DIR || "ab_off") + "/riscv_wasm.js");
const exp = mod.__wasm;
const vm = mod.Vm.restore(fs.readFileSync("kernels/shell.snap"));
vm.jit_enable(true);
vm.input(Buffer.from(process.env.WORK || "while :; do ls -la /bin | md5sum; done\n"));
const table = exp.__indirect_function_table;
function pump() {
  const c = vm.jit_pending(); if (!c) return;
  const base = table.length; table.grow(c);
  const bytes = vm.jit_build(base); if (!bytes || !bytes.length) return;
  new WebAssembly.Instance(new WebAssembly.Module(bytes), { env: {
    mem: exp.memory, __indirect_function_table: table,
    load8u: exp.load8u, load16u: exp.load16u, load32u: exp.load32u, load64: exp.load64,
    store8: exp.store8, store16: exp.store16, store32: exp.store32, store64: exp.store64,
    csr: exp.csr, fp: exp.fp,
    table_base: new WebAssembly.Global({ value: "i32", mutable: false }, base) } });
  vm.jit_installed(base);
}
// Warm up: reach steady state and give tier-up its budget to fire.
let warm = 0;
while (warm < 120_000_000) { warm += vm.run(2_000_000); pump(); }
// Measure: best-of blocks of slices (fastest = least contaminated).
const SLICES = Number(process.env.SLICES || 120);
const BLOCK = 8;
const mips = [];
for (let i = 0; i < SLICES; i++) {
  const t0 = process.hrtime.bigint();
  const ran = vm.run(2_000_000);
  const ns = Number(process.hrtime.bigint() - t0);
  pump();
  mips.push(ran / (ns / 1e9) / 1e6);
}
const best = [];
for (let i = 0; i < mips.length; i += BLOCK) best.push(Math.max(...mips.slice(i, i + BLOCK)));
best.sort((a, b) => a - b);
const med = best[best.length >> 1];
console.log((process.env.LABEL || "run") + ": median-of-best " + med.toFixed(1) + " MIPS  (" + best.length + " blocks)");
