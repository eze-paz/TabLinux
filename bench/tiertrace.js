// Run the shell workload on the baseline JIT build and let V8 trace wasm
// compilation, so we can see whether hot JIT block functions tier up from
// Liftoff to TurboFan during steady state.
const fs = require("fs");
const mod = require(process.env.HOME + "/" + (process.env.DIR || "ab_off") + "/riscv_wasm.js");
const exp = mod.__wasm;
const vm = mod.Vm.restore(fs.readFileSync("kernels/shell.snap"));
vm.jit_enable(true);
vm.input(Buffer.from("while :; do ls -la /bin | md5sum; done\n"));
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
const ITERS = Number(process.env.ITERS || 60);
let steps = 0;
for (let i = 0; i < ITERS; i++) { steps += vm.run(2_000_000); pump(); }
console.error("HARNESS_DONE ran " + (steps / 1e6).toFixed(0) + "M insns over " + ITERS + " pumps");
