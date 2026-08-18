// Why does `sleep 5` crawl under the deep-idle host sleep? Count the parks
// the engine reports (each costs the browser one backoff quantum) and the
// guest-time spacing between them, across a guest `sleep 5`.

const fs = require('fs');
const path = require('path');
const ROOT = path.join(__dirname, '..');
const mod = require((process.env.JITNODE || '/tmp/jitnode_idle') + '/riscv_wasm.js');

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

let out = '';
function runSlice() {
    const n = vm.run(2_000_000);
    pump();
    const c = vm.console();
    if (c.length) out += Buffer.from(c).toString('latin1');
    return n;
}

// Settle to the prompt.
for (let i = 0; i < 150; i++) runSlice();

vm.input(Buffer.from('sleep 5; echo PROBE_DONE\n'));

let reports = 0;
const gaps = [];   // guest-ms between consecutive reported parks
let lastMt = vm.mtime();
let slices = 0;
while (!out.includes('PROBE_DONE') && slices < 100000) {
    runSlice();
    slices++;
    // Mirrors the worker: after a slice, idle_ms() > 0 means the engine
    // reported a deep-idle park and the browser would sleep a quantum here.
    const idle = vm.idle_ms();
    if (idle > 0) {
        reports++;
        const mt = vm.mtime();
        gaps.push((mt - lastMt) / 10_000); // ticks -> guest ms
        lastMt = mt;
    }
}
gaps.sort((a, b) => a - b);
const q = (p) => gaps.length ? gaps[Math.floor(gaps.length * p)].toFixed(1) : '-';
console.log(`parks reported during sleep5: ${reports} (slices ${slices})`);
console.log(`guest-ms between reported parks: p10=${q(0.1)} p50=${q(0.5)} p90=${q(0.9)}`);
console.log(`browser wall estimate at 50ms/quantum: ${(reports * 50 / 1000).toFixed(1)}s`);
