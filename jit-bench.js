// How fast is a compiled block, and what does entering one cost?
//
// Those two numbers decide whether this design can work at all. Real basic
// blocks are short -- a handful of instructions between branches, and roughly a
// quarter of executed instructions are loads or stores, which this first
// version does not compile. If entering a block costs as much as interpreting
// the three or four instructions it would cover, the JIT loses no matter how
// good the generated code is.
//
// So: measure per-instruction cost inside a long block (the ceiling), and
// measure it again for a short block (what real code would actually get). The
// gap between them is the entry overhead.
//
//   cargo run --release -p riscv-jit --example difftest   # generates modules
//   node jit-bench.js

const fs = require('fs');

const REGS_BASE = 4096;
const memory = new WebAssembly.Memory({ initial: 2 });
const view = new DataView(memory.buffer);

function load(path) {
    const mod = new WebAssembly.Module(fs.readFileSync(path));
    return new WebAssembly.Instance(mod, { env: { mem: memory } }).exports.run;
}

function seed() {
    for (let r = 0; r < 32; r++) {
        view.setBigUint64(REGS_BASE + r * 8, BigInt(r * 0x9E3779B9), true);
    }
}

// Time `iters` calls, reporting nanoseconds per guest instruction.
function bench(run, insnsPerCall, iters) {
    seed();
    // Warm up so V8 has tiered the call site up before timing.
    for (let i = 0; i < 200000; i++) run(REGS_BASE);

    let best = Infinity;
    for (let pass = 0; pass < 5; pass++) {
        const t0 = process.hrtime.bigint();
        for (let i = 0; i < iters; i++) run(REGS_BASE);
        const ns = Number(process.hrtime.bigint() - t0);
        best = Math.min(best, ns / (iters * insnsPerCall));
    }
    return best;
}

const long = load('/tmp/jit_case_0.wasm');   // 12 instructions, from difftest
const nsLong = bench(long, 12, 2_000_000);

console.log(`12-instruction block: ${nsLong.toFixed(2)} ns/guest-insn ` +
            `(${(1000 / nsLong).toFixed(0)} MIPS)`);

// Entry overhead, backed out of the two measurements.
if (fs.existsSync('/tmp/jit_short.wasm')) {
    const short = load('/tmp/jit_short.wasm');
    const nsShort = bench(short, 3, 2_000_000);
    console.log(`3-instruction block:  ${nsShort.toFixed(2)} ns/guest-insn ` +
                `(${(1000 / nsShort).toFixed(0)} MIPS)`);
    // t(n) = entry + n*per  =>  entry = (t3*3 - t12*12) / (1 - 12/3) ... solve
    // directly from the two totals instead, which is less error-prone.
    const total12 = nsLong * 12, total3 = nsShort * 3;
    const per = (total12 - total3) / 9;
    const entry = total3 - 3 * per;
    console.log(`\nper-instruction ${per.toFixed(2)} ns, block entry ${entry.toFixed(2)} ns`);
    console.log(`entry pays for itself above ${(entry / per).toFixed(1)} instructions`);
} else {
    console.log('(no /tmp/jit_short.wasm; run difftest with a short case for entry cost)');
}

// The number to beat: the native interpreter currently does ~24 MIPS, and the
// wasm build is slower than native, so anything the JIT produces has to be
// compared against that -- not against zero.
console.log(`\ninterpreter reference: ~24 MIPS native (~42 ns/insn), wasm build slower still`);
