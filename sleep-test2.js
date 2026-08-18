// Does the guest's clock keep real time while it is idle?
//
// The obvious way to ask is to type `sleep 3; echo MARKER` and wait for MARKER.
// That is wrong, and it produced a confident false result once already: the
// shell ECHOES the typed line straight back, so the marker appears in the
// console before the sleep has even started, and the test reports whatever the
// echo latency happened to be.
//
// So the marker must be something the shell computes, and therefore cannot
// appear in the echo of the command that produced it: type `echo SLP$((20+2))`
// and wait for `SLP22`. The echo shows the arithmetic, the output shows 22.
//
//   node sleep-test2.js              # current shipped behaviour
//   REALTIME=1 node sleep-test2.js   # with a host clock supplied

const fs = require('fs');
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';
const snapshot = fs.readFileSync('kernels/shell.snap');
const mod = require(SHIM);
const vm = mod.Vm.restore(snapshot);
vm.jit_enable(true);

const exp = mod.__wasm;
const table = exp.__indirect_function_table;
function pump() {
    const count = vm.jit_pending();
    if (count === 0) return 0;
    const base = table.length;
    table.grow(count);
    const bytes = vm.jit_build(base);
    if (!bytes || bytes.length === 0) return 0;
    new WebAssembly.Instance(new WebAssembly.Module(bytes), {
        env: {
            mem: exp.memory, __indirect_function_table: table,
            load8u: exp.load8u, load16u: exp.load16u, load32u: exp.load32u,
            load64: exp.load64, store8: exp.store8, store16: exp.store16,
            store32: exp.store32, store64: exp.store64, csr: exp.csr, fp: exp.fp,
        },
    });
    vm.jit_installed(base);
    return count;
}

const REALTIME = process.env.REALTIME === '1';
const SLEEP = Number(process.env.SLEEP || 3);
const MARKER = 'SLP22';

let out = '';
let idleReturns = 0;
let workReturns = 0;

function drain(maxNs, stopOnMarker) {
    const t0 = process.hrtime.bigint();
    let steps = 0;
    while (Number(process.hrtime.bigint() - t0) < maxNs) {
        if (REALTIME) vm.set_host_ns(Number(process.hrtime.bigint()));
        steps += vm.run(2_000_000);
        const c = vm.console();
        if (c.length) out += Buffer.from(c).toString('latin1');
        pump();
        if (stopOnMarker && out.includes(MARKER)) break;
        if (REALTIME && vm.idle_ms && vm.idle_ms() > 0) {
            idleReturns++;
            // Stand in for the browser's setTimeout: give real time back
            // without asking the emulator to do anything.
            const until = process.hrtime.bigint() + BigInt(Math.round(Math.min(20, vm.idle_ms()) * 1e6));
            while (process.hrtime.bigint() < until) { /* idle */ }
        } else {
            workReturns++;
        }
    }
    return steps;
}

drain(2e9, false);          // settle at the prompt, warm the JIT
out = '';
idleReturns = 0; workReturns = 0;

vm.input(Buffer.from(`sleep ${SLEEP}; echo SLP$((20+2))\n`));
const t0 = process.hrtime.bigint();
const m0 = Number(vm.mtime());
const steps = drain(30e9, true);
const wall = Number(process.hrtime.bigint() - t0) / 1e9;
const guest = (Number(vm.mtime()) - m0) / 10_000_000;

console.log(`mode                   ${REALTIME ? 'host clock supplied' : 'no host clock (shipped)'}`);
console.log(`guest asked to sleep   ${SLEEP}.00s`);
console.log(`host time actually     ${wall.toFixed(2)}s`);
console.log(`guest time advanced    ${guest.toFixed(2)}s`);
console.log(`instructions retired   ${(steps / 1e6).toFixed(1)}M   <- work done to wait`);
if (REALTIME) console.log(`run() returned idle    ${idleReturns} times, with work ${workReturns}`);
console.log(`marker seen            ${out.includes(MARKER)}`);
console.log('');

if (!out.includes(MARKER)) {
    console.log('FAIL: the sleep never completed within 30s.');
} else {
    const err = 100 * (wall - SLEEP) / SLEEP;
    const verdict = Math.abs(err) < 15 ? 'keeps real time'
        : wall < SLEEP ? `RUNS FAST by ${(-err).toFixed(0)}%` : `runs slow by ${err.toFixed(0)}%`;
    console.log(`verdict: ${verdict}  (${wall.toFixed(2)}s of real time for a ${SLEEP}s sleep)`);
    console.log(`         ${(steps / 1e6).toFixed(1)}M instructions retired while doing nothing`);
}
