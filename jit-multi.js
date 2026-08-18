// The production shape: every block in one module, entered through a single
// exported dispatch(index, regs, pc), with memory accesses calling into a wasm
// host module.
//
// Both of those are the result of a measurement rather than a preference:
//
//   * One module per block makes the host's call site megamorphic. Rotating
//     through 400 blocks cost 293 ns per entry against 21 ns entering one
//     repeatedly. Real basic blocks are short, so that penalty alone decides
//     whether a JIT can pay at all.
//   * Routing memory accesses through JS closures costs ~32 ns each against
//     ~6 ns wasm-to-wasm. Measuring the JS version makes loads look ruinous and
//     argues for inlining a software TLB that is not needed.
//
// Correctness is checked before speed. A dispatcher that computes the wrong
// index would look wonderfully fast while running the wrong block.

const fs = require('fs');
const E = require('./jitenv.js');

const cases = JSON.parse(fs.readFileSync('/tmp/jit_cases.json', 'utf8'));
const N = cases.length;

function instantiate(env) {
    const mod = new WebAssembly.Module(fs.readFileSync('/tmp/jit_multi.wasm'));
    return new WebAssembly.Instance(mod, { env }).exports.dispatch;
}

const dispatch = instantiate(E.wasmEnv());

let bad = 0;
for (const c of cases) {
    E.setRegs(c.init);
    E.fillMem(c.case);
    dispatch(c.case, E.REGS_BASE, E.BLOCK_PC);
    const err = E.checkAgainst(c);
    if (err) {
        if (bad < 5) console.log(`case ${c.case} ${err}`);
        bad++;
    }
}
console.log(bad ? `FAIL: ${bad}/${N} mismatch` : `OK: ${N} blocks correct through dispatcher`);
if (bad) process.exit(1);

function bench(label, run, pick, iters) {
    for (let i = 0; i < 200000; i++) run(pick(i), E.REGS_BASE, E.BLOCK_PC);
    let best = Infinity;
    for (let pass = 0; pass < 5; pass++) {
        const t0 = process.hrtime.bigint();
        for (let i = 0; i < iters; i++) run(pick(i), E.REGS_BASE, E.BLOCK_PC);
        const ns = Number(process.hrtime.bigint() - t0);
        best = Math.min(best, ns / iters);
    }
    console.log(`${label}: ${best.toFixed(2)} ns/call (${(best / 12).toFixed(2)} ns/guest insn)`);
    return best;
}

console.log();
const one = bench('wasm imports, one block ', dispatch, () => 0, 1_000_000);
const many = bench('wasm imports, rotating  ', dispatch, i => i % N, 1_000_000);

// The same blocks with JS imports, to keep the size of that mistake visible.
const viaJs = instantiate(E.jsEnv());
const jsMany = bench('JS imports,   rotating  ', viaJs, i => i % N, 500_000);

console.log(`\nrotating penalty inside one module: ${(many - one).toFixed(2)} ns/call`);
console.log(`JS imports cost ${(jsMany - many).toFixed(0)} ns/call more than wasm imports`);

// These blocks are ~29% memory instructions, close to the mix a real boot
// showed, so this is representative rather than a pure-ALU best case.
const INTERP_NS = 42;
console.log(`\nspeedup on a 12-instruction block: ${((12 * INTERP_NS) / many).toFixed(1)}x ` +
            `vs the interpreter's ~${INTERP_NS} ns/instruction`);
