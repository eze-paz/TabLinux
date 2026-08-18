// What does the user's empirical test — awk generating random doubles —
// actually spend its time on?
//
// 50k iterations take ~10 real seconds in the browser, which is far slower
// than the integer workloads suggest. Hypothesis: busybox awk does everything
// in doubles, the JIT compiles NO floating-point instruction (riscv-jit has no
// F/D codegen at all), so every FP op both round-trips the interpreter AND
// truncates the trace around it — the same double penalty fence and CSR had
// before they were compiled (+36% for CSR alone, and CSRs were only 6% of
// wall).
//
// Deterministic counters only. This answers "what share of the work is FP and
// how much wall time does the interpreter eat on this workload" — it makes no
// speed claim.

const fs = require('fs');
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';
const mod = require(SHIM);
const exp = mod.__wasm;

const snapshot = new Uint8Array(fs.readFileSync('kernels/shell.snap'));
const vm = mod.Vm.restore(snapshot);
if (!vm) throw new Error('restore failed');
// NOJIT=1 runs the same test on the interpreter: the printed awk sum is then
// an end-to-end FP oracle -- 50k double adds through rand()'s multiply-and-
// carry arithmetic; a single compiled FP op computing differently changes it.
if (process.env.NOJIT !== '1') { vm.jit_enable(true); vm.interp_hist_enable(true); }

const table = exp.__indirect_function_table;
function pump() {
    const n = vm.jit_pending();
    if (!n) return;
    const base = table.length;
    table.grow(n);
    const bytes = vm.jit_build(base);
    if (!bytes || !bytes.length) return;
    new WebAssembly.Instance(new WebAssembly.Module(bytes), { env: {
        mem: exp.memory, __indirect_function_table: table,
        load8u: exp.load8u, load16u: exp.load16u, load32u: exp.load32u, load64: exp.load64,
        store8: exp.store8, store16: exp.store16, store32: exp.store32, store64: exp.store64,
        csr: exp.csr, fp: exp.fp,
    }});
    vm.jit_installed(base);
}

let out = '';
function drainUntil(marker, capNs) {
    const t0 = process.hrtime.bigint();
    let steps = 0;
    while (Number(process.hrtime.bigint() - t0) < capNs) {
        steps += vm.run(2_000_000);
        const c = vm.console();
        if (c.length) out += Buffer.from(c).toString('latin1');
        pump();
        if (marker && out.includes(marker)) break;
    }
    return steps;
}

// Settle at the prompt, warm the JIT on shell startup code.
drainUntil(null, 2e9);
out = '';

// The user's exact test, with a computed end marker (the echo of the typed
// command cannot contain AWKDONE22, so matching it cannot false-trigger).
const N = Number(process.env.N || 50000);
vm.input(Buffer.from(
    `awk 'BEGIN{srand(7);s=0;for(i=0;i<${N};i++)s+=rand();print s}'; echo AWKDONE$((20+2))\n`));

const t0 = process.hrtime.bigint();
const steps = drainUntil('AWKDONE22', 120e9);
const wall = Number(process.hrtime.bigint() - t0) / 1e9;

const NAMES = ['mul', 'div/rem', 'atomic', 'csr', 'fence', 'system', 'fp',
    'cold (compilable)', 'other', 'fence.i'];
const h = Array.from(vm.interp_hist());
const interpTotal = h.reduce((a, c) => a + c, 0) || 1;
const [entries, chains, chainInsns] = Array.from(vm.jit_stats());

// Interpreted instructions cost ~196ns (measured earlier on this box, order of
// magnitude); compiled ~10ns. Shares of wall derived from that split.
const INTERP_NS = 196, COMPILED_NS = 10;
const interpNs = interpTotal * INTERP_NS;
const compiledNs = chainInsns * COMPILED_NS;
const totalNs = interpNs + compiledNs;

const lines = out.split('\n').map(l => l.trim()).filter(Boolean);
const di = lines.findIndex(l => l.includes('AWKDONE22') && !l.includes('echo'));
const sum = di > 0 ? lines[di - 1] : '(not found)';
console.log(`awk sum = ${sum}   <- must be identical between NOJIT=1 and JIT runs`);
console.log(`awk N=${N}: ${wall.toFixed(1)}s in this harness, ${(steps / 1e6).toFixed(0)}M instructions`);
console.log(`  => ${(steps / N / 1000).toFixed(1)}k guest instructions per loop iteration`);
console.log(`compiled: ${(100 * chainInsns / steps).toFixed(1)}% of instructions`);
console.log(`interpreted: ${(interpTotal / 1e6).toFixed(1)}M instructions` +
    ` ~= ${(100 * interpNs / totalNs).toFixed(0)}% of estimated wall\n`);
console.log('what falls to the interpreter on THIS workload:');
h.map((n, i) => [NAMES[i], n]).sort((a, b) => b[1] - a[1]).filter(([, n]) => n > 0)
    .forEach(([name, n]) => console.log(
        `  ${name.padEnd(20)} ${(n / 1e6).toFixed(2).padStart(8)}M` +
        `  ${(100 * n / interpTotal).toFixed(1).padStart(5)}% of interpreted` +
        `  ~${(100 * n * INTERP_NS / totalNs).toFixed(0).padStart(3)}% of wall`));
