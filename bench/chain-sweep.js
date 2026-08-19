// Sweep the compiled-chain cap, scoring on DETERMINISTIC counters, not MIPS.
//
// This box cannot resolve a timing change under ~15%, so a wall-clock sweep
// would be four expensive coin flips. Boundary crossings are exact and repeat
// run to run: if raising the cap does not cut them, there is nothing for an A/B
// to find and it is not worth its 45 minutes. If it does, this says by how
// much, which is the expectation the A/B then has to beat.
//
//   node chain-sweep.js              # 256,512,1024,4096
//   CAPS=256,2048 node chain-sweep.js
//
// Console output is compared against the interpreter on every cap: raising the
// cap raises worst-case interrupt latency, and the way that would show up is a
// guest that behaves differently, not a guest that runs slower.

const fs = require('fs');
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';
const snapshot = fs.readFileSync('kernels/shell.snap');

function fresh() {
    delete require.cache[require.resolve(SHIM)];
    return require(SHIM);
}

function makeCompiler(vm, exp) {
    const table = exp.__indirect_function_table;
    return function pump() {
        const count = vm.jit_pending();
        if (count === 0) return 0;
        const base = table.length;
        table.grow(count);
        const bytes = vm.jit_build(base);
        if (!bytes || bytes.length === 0) return 0;
        const blocks = new WebAssembly.Module(bytes);
        new WebAssembly.Instance(blocks, {
            env: {
                mem: exp.memory,
                __indirect_function_table: table,
                load8u: exp.load8u, load16u: exp.load16u,
                load32u: exp.load32u, load64: exp.load64,
                store8: exp.store8, store16: exp.store16,
                store32: exp.store32, store64: exp.store64,
                csr: exp.csr, fp: exp.fp,
            },
        });
        vm.jit_installed(base);
        return count;
    };
}

const WORK = 'for i in 1 2 3 4; do ls -la /bin | md5sum; done; echo JITPROBE $((7*6))\n';

function boot(useJit, budgetSteps, chainMax) {
    const mod = fresh();
    const vm = mod.Vm.restore(snapshot);
    if (!vm) throw new Error('snapshot restore failed');
    vm.input(Buffer.from(WORK));
    let pump = () => 0;
    if (useJit) {
        vm.jit_enable(true);
        vm.interp_hist_enable(true);
        if (chainMax) vm.set_chain_max(chainMax);
        pump = makeCompiler(vm, mod.__wasm);
    }
    let out = Buffer.alloc(0);
    let steps = 0;
    const t0 = process.hrtime.bigint();
    while (steps < budgetSteps) {
        steps += vm.run(2_000_000);
        const c = vm.console();
        if (c.length) out = Buffer.concat([out, Buffer.from(c)]);
        pump();
    }
    const ns = Number(process.hrtime.bigint() - t0);
    return {
        out: out.toString('latin1'),
        steps,
        mips: steps / (ns / 1e9) / 1e6,
        stats: useJit ? Array.from(vm.jit_stats()) : null,
        miss: useJit ? Array.from(vm.chain_miss()) : null,
    };
}

const BUDGET = Number(process.env.BUDGET || 400_000_000);
const CAPS = (process.env.CAPS || '256,512,1024,4096').split(',').map(Number);

console.log('reference (interpreter)...');
const ref = boot(false, BUDGET, 0);

const rows = [];
for (const cap of CAPS) {
    const r = boot(true, BUDGET, cap);
    const [entries, chains, chainInsns] = r.stats;
    const row = {
        cap, chains, entries,
        perChain: chainInsns / chains,
        compiled: 100 * chainInsns / r.steps,
        mips: r.mips,
        capStops: r.miss[5],
        noBlock: r.miss[6],
        ok: r.out === ref.out,
    };
    rows.push(row);
    console.log(`cap ${String(cap).padStart(5)}  chains ${chains.toExponential(3)}` +
        `  insns/chain ${row.perChain.toFixed(1).padStart(7)}` +
        `  compiled ${row.compiled.toFixed(1)}%` +
        `  MIPS ${r.mips.toFixed(1).padStart(5)}` +
        `  console ${row.ok ? 'OK' : '*** MISMATCH ***'}`);
}

const base = rows[0];
console.log('\n  cap     chains   vs base   insns/chain   cap-stops   no-block   MIPS');
for (const r of rows) {
    console.log(
        `${String(r.cap).padStart(5)}  ${r.chains.toExponential(3)}   ` +
        `${(r.chains / base.chains).toFixed(3).padStart(6)}   ` +
        `${r.perChain.toFixed(1).padStart(9)}   ` +
        `${r.capStops.toExponential(2).padStart(9)}  ` +
        `${r.noBlock.toExponential(2).padStart(9)}  ` +
        `${r.mips.toFixed(1).padStart(5)}`);
}
console.log('\nMIPS here is a single unpaired sample on a box with 1.75x spread.' +
    '\nIt is printed for smell only. The chain columns are the evidence.');
if (rows.some(r => !r.ok)) {
    console.log('\nA CONSOLE MISMATCH IS A CORRECTNESS FAILURE, not a tuning result.');
    process.exit(1);
}
