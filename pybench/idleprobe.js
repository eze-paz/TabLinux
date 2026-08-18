// Diagnose the idle-instruction classifier: run an idle window, then a work
// window, and compare steps vs prof_idle_insns vs WFI-park counts in each.
// The classifier is right when idle windows attribute ~everything and work
// windows attribute ~nothing.

const fs = require('fs');
const path = require('path');
const ROOT = path.join(__dirname, '..');
const mod = require((process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js');

const vm = mod.Vm.restore(fs.readFileSync(path.join(ROOT, 'kernels/shell.snap')));
vm.jit_enable(true);
const exp = mod.__wasm;
const table = exp.__indirect_function_table;
function pump() {
    const count = vm.jit_pending();
    if (!count) return;
    const base = table.length;
    table.grow(count);
    const bytes = vm.jit_build(base);
    if (!bytes || !bytes.length) return;
    new WebAssembly.Instance(new WebAssembly.Module(bytes), { env: {
        mem: exp.memory, __indirect_function_table: table,
        table_base: new WebAssembly.Global({ value: 'i32', mutable: false }, base),
        load8u: exp.load8u, load16u: exp.load16u, load32u: exp.load32u, load64: exp.load64,
        store8: exp.store8, store16: exp.store16, store32: exp.store32, store64: exp.store64,
        csr: exp.csr, fp: exp.fp } });
    vm.jit_installed(base);
}

let consoleBytes = 0;
function grab() {
    return {
        steps: Number(vm.steps()),
        idle: vm.prof_idle_insns(),
        parks: vm.prof_parks ? Array.from(vm.prof_parks()) : [0, 0],
        out: consoleBytes,
    };
}
function runFor(steps) {
    let n = 0;
    while (n < steps) { n += vm.run(2_000_000); pump(); consoleBytes += vm.console().length; }
}
function report(tag, a, b) {
    const d = b.steps - a.steps;
    const di = b.idle - a.idle;
    console.log(`${tag}: steps=${(d / 1e6).toFixed(0)}M idle=${(100 * di / d).toFixed(1)}% ` +
                `parks tmr=${b.parks[0] - a.parks[0]} dev=${b.parks[1] - a.parks[1]} ` +
                `console+${b.out - a.out}B`);
}

// Settle, then measure an IDLE window.
runFor(200_000_000);
let a = grab();
runFor(300_000_000);
report('IDLE   ', a, grab());

// Kick off the work loop, then report in 50M-step windows so the workload's
// lifetime is visible (console bytes mark real activity).
vm.input(Buffer.from('for i in 1 2 3 4 5 6 7 8 9 10; do ls -laR /usr /lib 2>/dev/null | md5sum; done\n'));
for (let w = 0; w < 12; w++) {
    a = grab();
    runFor(50_000_000);
    report(`WORK w${String(w).padStart(2)}`, a, grab());
}
