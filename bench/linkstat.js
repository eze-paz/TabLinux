// Reach probe for direct fall-through tail-call linking: run the shell workload
// and report how many clean fall-through blocks got a direct in-batch link.
const fs = require("fs");
const mod = require(process.env.HOME + "/" + (process.env.DIR || "ab_link") + "/riscv_wasm.js");
const exp = mod.__wasm;
const snap = fs.readFileSync("kernels/shell.snap");
const vm = mod.Vm.restore(snap);
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
let steps = 0;
for (let i = 0; i < 400; i++) { steps += vm.run(2_000_000); pump(); }
const s = vm.jit_link_stats();
console.log("ran", (steps / 1e6).toFixed(0) + "M guest insns");
console.log("clean fall-through blocks:", s[0]);
console.log("directly linked:          ", s[1]);
console.log("link reach:", s[0] ? (100 * s[1] / s[0]).toFixed(2) + "% of fall-through blocks" : "n/a");
