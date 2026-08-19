// End-to-end: boot the real guest with the JIT on, and prove it behaves
// identically to the interpreter.
//
// This is the test that matters. Everything before it measured the compiler in
// isolation against synthetic blocks; this runs the actual kernel, with real
// page tables, real faults, real device MMIO, and compares console output byte
// for byte against a run with the JIT off. A miscompiled block corrupts guest
// state, and the guest is a Linux kernel -- it will say so.
//
// Node runs the same V8 as the browser, so passing here is strong evidence for
// the deployed page, and far quicker to iterate on than a browser round trip.
//
//   RUSTFLAGS="-C link-arg=--export-table -C link-arg=--growable-table" \
//     cargo build --release --target wasm32-unknown-unknown -p riscv-wasm
//   wasm-bindgen --keep-lld-exports --target nodejs --out-dir /tmp/jitnode \
//     target/wasm32-unknown-unknown/release/riscv_wasm.wasm
//   printf 'module.exports.__wasm = wasm;\n' >> /tmp/jitnode/riscv_wasm.js
//   node jit-vm-test.js
//
// --keep-lld-exports is required: wasm-bindgen strips __indirect_function_table
// as an LLD-synthesised internal, and without it there is no table to install
// blocks into.

const fs = require('fs');
// Which build to measure. Set JITNODE to compare two side by side; see
// jit-ab.py, which alternates them because this box's spread within a single
// build is larger than most of the effects worth measuring.
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';

const snapshot = fs.readFileSync('kernels/shell.snap');

// The shim instantiates on require, so each run needs a fresh registry entry to
// get a fresh linear memory and table.
function fresh() {
    delete require.cache[require.resolve(SHIM)];
    return require(SHIM);
}

/// Compile and link whatever blocks are queued.
///
/// JS is involved only here. Once installed, the host calls blocks through its
/// own function table with no JS on the path -- which is the whole reason the
/// generated module imports the table rather than exporting a dispatcher.
function makeCompiler(vm, exp) {
    const table = exp.__indirect_function_table;
    // jit_pending now reports one deterministic batch at a time; drain them
    // all so a formation burst compiles within one pump.
    return function pump() {
        let linked = 0, n;
        while ((n = pumpOne())) linked += n;
        return linked;
    };
    function pumpOne() {
        const count = vm.jit_pending();
        if (count === 0) return 0;

        // Grow first: the generated module declares a minimum table size of
        // base + count, so linking fails if the table is too small. Read the
        // count before jit_build, which consumes the queue.
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
                table_base: new WebAssembly.Global({ value: 'i32', mutable: false }, base),
            },
        });
        vm.jit_installed(base);
        return count;
    };
}

function boot(useJit, budgetSteps) {
    const mod = fresh();
    const vm = mod.Vm.restore(snapshot);
    if (!vm) throw new Error('snapshot restore failed');

    // Something that actually exercises the guest: directory walks, pipes,
    // process creation, and a checksum whose value would change if any
    // compiled block computed the wrong thing.
    vm.input(Buffer.from('for i in 1 2 3 4; do ls -la /bin | md5sum; done; echo JITPROBE $((7*6))\n'));
    const pump = useJit ? (vm.jit_enable(true), vm.interp_hist_enable?.(true), makeCompiler(vm, mod.__wasm)) : () => 0;

    let out = Buffer.alloc(0);
    let steps = 0;
    let linked = 0;
    const t0 = process.hrtime.bigint();
    while (steps < budgetSteps) {
        steps += vm.run(2_000_000);
        const c = vm.console();
        if (c.length) out = Buffer.concat([out, Buffer.from(c)]);
        linked += pump();
    }
    const ns = Number(process.hrtime.bigint() - t0);
    return {
        out: out.toString('latin1'),
        steps,
        mips: steps / (ns / 1e9) / 1e6,
        linked,
        stats: useJit ? Array.from(vm.jit_stats()) : null,
        hist: useJit && vm.interp_hist ? Array.from(vm.interp_hist()) : null,
        chainMiss: useJit && vm.chain_miss ? Array.from(vm.chain_miss()) : null,
    };
}

const BUDGET = 400_000_000;

console.log('interpreter...');
const a = boot(false, BUDGET);
console.log('jit...');
const b = boot(true, BUDGET);

console.log(`\ninterpreter: ${a.mips.toFixed(1)} MIPS`);
console.log(`jit:         ${b.mips.toFixed(1)} MIPS  (${b.linked} blocks linked)`);
console.log(`speedup:     ${(b.mips / a.mips).toFixed(2)}x`);

// Split the cost between entering blocks and running them. If entry dominates
// -- an MMU translation, a table probe, a tick_n and an indirect call, all to
// run a handful of instructions -- then the next win is chaining, not codegen.
if (b.stats) {
    const [entries, chains, chainInsns, rejected] = b.stats;
    const interpNs = 1000 / a.mips;
    const totalNs = b.steps * (1000 / b.mips);
    const spentInterpreting = (b.steps - chainInsns) * interpNs;
    console.log('\n--- where the time goes ---');
    console.log(`block entries      ${entries.toFixed(0)}`);
    console.log(`chains entered     ${chains.toFixed(0)}`);
    console.log(`insns in compiled  ${chainInsns.toFixed(0)} (${(100 * chainInsns / b.steps).toFixed(1)}% of all)`);
    console.log(`insns per chain    ${(chainInsns / chains).toFixed(1)}`);
    console.log(`rejected blocks    ${rejected.toFixed(0)}`);
    console.log(`compiled code      ~${((totalNs - spentInterpreting) / chainInsns).toFixed(2)} ns/insn`);
    console.log(`interpreted        ~${interpNs.toFixed(0)} ns/insn on ${(100 - 100 * chainInsns / b.steps).toFixed(1)}% of instructions`);
    console.log(`  => interpreting is ${(100 * spentInterpreting / totalNs).toFixed(1)}% of wall time`);

    // Which instructions are in that population. "cold (compilable)" is the
    // one bin codegen cannot touch -- those blocks simply never got hot, so a
    // large share there means the lever is block selection, not new opcodes.
    if (b.hist) {
        const NAMES = ['mul', 'div/rem', 'atomic', 'csr', 'fence', 'system', 'fp',
            'cold (compilable)', 'other', 'fence.i'];
        const total = b.hist.reduce((a, c) => a + c, 0) || 1;
        console.log('\n--- what falls to the interpreter ---');
        b.hist.map((n, i) => [NAMES[i], n])
            .sort((x, y) => y[1] - x[1])
            .filter(([, n]) => n > 0)
            .forEach(([name, n]) => {
                const share = n / total;
                console.log(`  ${name.padEnd(18)} ${n.toFixed(0).padStart(10)}` +
                    `  ${(100 * share).toFixed(1).padStart(5)}% of interpreted` +
                    `  ~${(100 * share * spentInterpreting / totalNs).toFixed(1)}% of wall time`);
            });
    }
}

if (a.out === b.out) {
    console.log(`\nOK: console output identical (${a.out.length} bytes)`);
} else {
    console.log(`\nMISMATCH: interpreter ${a.out.length} bytes, jit ${b.out.length} bytes`);
    const n = Math.min(a.out.length, b.out.length);
    for (let i = 0; i < n; i++) {
        if (a.out[i] !== b.out[i]) {
            console.log(`first difference at byte ${i}`);
            console.log(`  interp: ${JSON.stringify(a.out.slice(Math.max(0, i - 80), i + 80))}`);
            console.log(`  jit:    ${JSON.stringify(b.out.slice(Math.max(0, i - 80), i + 80))}`);
            process.exit(1);
        }
    }
    console.log('(one output is a prefix of the other -- likely just a timing difference)');
    process.exit(1);
}

if (!a.out.includes('JITPROBE 42')) {
    console.log('WARNING: the guest never ran the probe command; budget may be too small');
}

// --- why compiled chains stop ---
//
// A compiled block ends by probing the chain table for its successor and
// tail-calling it. When that probe misses, control returns to the host, which
// costs an MMU-free lookup at best and a full re-entry at worst. These bins say
// which of the probe's three tests failed, and that selects the fix: eviction
// means the table geometry is wrong, a moved `trans_gen` means address-space
// switches are killing entries (ASIDs), a moved privilege field means kernel
// and user blocks are evicting each other.
if (b.chainMiss) {
    const NAMES = ['probe: slot empty', 'probe: evicted', 'probe: gen priv',
        'probe: gen trans', 'probe: gen both', 'probe: valid, wasm budget',
        '  of those: no block', 'end: fault', 'end: cap'];
    const m = b.chainMiss;
    const probes = m.slice(0, 6).reduce((a, c) => a + c, 0) || 1;
    console.log('\n--- why compiled chains stop ---');
    console.log(`tail-call probe misses ${probes.toFixed(0)}`);
    for (let i = 0; i < 6; i++) {
        console.log(`  ${NAMES[i].padEnd(26)} ${m[i].toFixed(0).padStart(10)}` +
            `  ${(100 * m[i] / probes).toFixed(1).padStart(5)}%`);
    }
    console.log(`  ${NAMES[6].padEnd(26)} ${m[6].toFixed(0).padStart(10)}` +
        `  ${(100 * m[6] / probes).toFixed(1).padStart(5)}% of misses`);
    console.log(`chain ends: fault ${m[7].toFixed(0)}, cap ${m[8].toFixed(0)}`);
}
