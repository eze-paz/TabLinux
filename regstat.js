const fs = require("fs");
const dir = process.env.DIR;
const mod = require(process.env.HOME + "/" + dir + "/riscv_wasm.js");
const exp = mod.__wasm;
const vm = mod.Vm.restore(fs.readFileSync("kernels/shell.snap"));
vm.jit_enable(true);
vm.input(Buffer.from("while :; do ls -la /bin | md5sum; done\n"));
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
const r = vm.jit_reg_stats();
console.log(dir.padEnd(9), "reg loads(emitted, miss-path):", r[0], "stores:", r[1], "compiled:", r[2],
  "-> loads/insn:", (r[0] / r[2]).toFixed(2), "stores/insn:", (r[1] / r[2]).toFixed(2));
