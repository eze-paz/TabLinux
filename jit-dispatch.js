// What does it cost to enter a compiled block *realistically*?
//
// jit-bench.js calls one block in a tight loop, so V8 sees a monomorphic call
// site and the block stays perfectly warm. Real execution rotates through
// hundreds of blocks, and each one is a separate wasm module instance — so the
// call site is megamorphic and the entry cost is whatever indirect dispatch
// actually costs, not the best case.
//
// That distinction decides the design. If dispatch is cheap, blocks can be
// small and the JIT can cover ordinary code. If it is expensive, only long
// blocks pay, and since real basic blocks are short the whole approach needs
// rethinking — probably towards compiling many blocks into one module with an
// internal dispatch loop, which is what v86 does.

const fs = require('fs');

const REGS_BASE = 4096;
const memory = new WebAssembly.Memory({ initial: 2 });
const view = new DataView(memory.buffer);

const N = 400;
const funcs = [];
for (let i = 0; i < N; i++) {
    const mod = new WebAssembly.Module(fs.readFileSync(`/tmp/jit_case_${i}.wasm`));
    funcs.push(new WebAssembly.Instance(mod, { env: { mem: memory } }).exports.run);
}

for (let r = 0; r < 32; r++) {
    view.setBigUint64(REGS_BASE + r * 8, BigInt(r * 0x9E3779B9), true);
}

function bench(label, pick, iters) {
    for (let i = 0; i < 200000; i++) funcs[pick(i)](REGS_BASE);
    let best = Infinity;
    for (let pass = 0; pass < 5; pass++) {
        const t0 = process.hrtime.bigint();
        for (let i = 0; i < iters; i++) funcs[pick(i)](REGS_BASE);
        const ns = Number(process.hrtime.bigint() - t0);
        best = Math.min(best, ns / iters);
    }
    console.log(`${label}: ${best.toFixed(2)} ns/call  ` +
                `(${(best / 12).toFixed(2)} ns per guest insn over 12)`);
    return best;
}

// Monomorphic: the same block every time. This is what jit-bench.js measured.
const mono = bench('one block, repeated      ', () => 0, 2_000_000);
// Megamorphic: rotating through all 400, which is closer to real execution.
const poly = bench('400 blocks, rotating     ', i => i % N, 2_000_000);

console.log(`\ndispatch penalty: ${(poly - mono).toFixed(2)} ns/call`);

// Break-even against the interpreter. The interpreter costs ~42 ns per guest
// instruction natively; the browser's wasm build is slower still, so this is
// the conservative direction.
const INTERP_NS = 42;
const per = 0.71;
for (const [name, entry] of [['monomorphic', mono - 12 * per], ['megamorphic', poly - 12 * per]]) {
    const n = entry / (INTERP_NS - per);
    console.log(`${name}: entry ${entry.toFixed(1)} ns -> breaks even at ` +
                `${n.toFixed(1)} guest instructions per block`);
}
