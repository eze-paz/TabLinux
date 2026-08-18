// The integration prototype: Rust host calls generated blocks with no JS in
// the hot path.
//
// This is the mechanism the emulator will use, and the one unknown the whole
// integration rests on. The run loop is Rust compiled to wasm; the blocks are a
// separate module. Going Rust -> JS -> generated wasm would cost a wasm-to-JS
// boundary each way (~32 ns measured) and give back most of the JIT's gain on
// the short blocks real code produces.
//
// Instead the generated module declares an active element segment on the host's
// *imported* `__indirect_function_table`, writing its blocks straight into the
// host's own table. A Rust function pointer on wasm is a table index, so the
// host calls block `i` by transmuting `base + i`.
//
// JS appears only at setup: it grows the table and instantiates. The timed loop
// runs entirely inside wasm, via call_block_n / call_blocks_rotating, so the
// measurement is not a JS-loop artefact.

const fs = require('fs');

const TABLE_BASE = 16; // must match the base difftest.rs compiled with
const N = 400;

const hostMod = new WebAssembly.Module(
    fs.readFileSync('crates/riscv-jit/prototype/target/wasm32-unknown-unknown/release/jit_prototype.wasm'));
const host = new WebAssembly.Instance(hostMod, {}).exports;

const table = host.__indirect_function_table;
const memory = host.memory;
console.log(`host table starts at ${table.length} entries, memory ${memory.buffer.byteLength} bytes`);

// Make room for the blocks. The host's own table is tiny; the generated module
// declares a minimum of TABLE_BASE + N, so it must be grown before linking.
if (table.length < TABLE_BASE + N) {
    table.grow(TABLE_BASE + N - table.length);
}

const blocksMod = new WebAssembly.Module(fs.readFileSync('/tmp/jit_table.wasm'));
new WebAssembly.Instance(blocksMod, {
    env: {
        mem: memory,
        __indirect_function_table: table,
        load8u: host.load8u, load16u: host.load16u,
        load32u: host.load32u, load64: host.load64,
        store8: host.store8, store16: host.store16,
        store32: host.store32, store64: host.store64,
    },
});
console.log('blocks installed into the host table');

// --- correctness ----------------------------------------------------------
const REGS = host.regs_ptr();
const MEM = host.mem_ptr();
const MEM_BYTES = 4096;
const BLOCK_PC = 0x80001234n;
const view = new DataView(memory.buffer);
const bytes = new Uint8Array(memory.buffer);

const cases = JSON.parse(fs.readFileSync('/tmp/jit_cases.json', 'utf8'));

const FNV_PRIME = 0x100000001b3n;
const MASK64 = (1n << 64n) - 1n;
function checksum() {
    let h = 0xcbf29ce484222325n;
    for (let i = 0; i < MEM_BYTES; i++) {
        h ^= BigInt(bytes[MEM + i]);
        h = (h * FNV_PRIME) & MASK64;
    }
    return h;
}

let bad = 0;
for (const c of cases) {
    for (let r = 0; r < 32; r++) view.setBigUint64(REGS + r * 8, BigInt(c.init[r]), true);
    for (let i = 0; i < MEM_BYTES; i++) bytes[MEM + i] = (i * 31 + c.case * 17) & 0xFF;

    host.call_block(TABLE_BASE, c.case, REGS, BLOCK_PC);

    let ok = true;
    for (let r = 0; r < 32; r++) {
        if (view.getBigUint64(REGS + r * 8, true) !== BigInt(c.expect[r])) { ok = false; break; }
    }
    if (ok && checksum() !== BigInt(c.memsum)) ok = false;
    if (!ok) { if (bad < 5) console.log(`case ${c.case} mismatch via host table call`); bad++; }
}
console.log(bad ? `FAIL: ${bad}/${cases.length}` : `OK: ${cases.length} blocks correct, called from Rust`);
if (bad) process.exit(1);

// --- speed ----------------------------------------------------------------
function bench(label, fn, iters) {
    fn(200000);
    let best = Infinity;
    for (let pass = 0; pass < 5; pass++) {
        const t0 = process.hrtime.bigint();
        fn(iters);
        const ns = Number(process.hrtime.bigint() - t0);
        best = Math.min(best, ns / iters);
    }
    console.log(`${label}: ${best.toFixed(2)} ns/call (${(best / 12).toFixed(2)} ns/guest insn)`);
    return best;
}

console.log();
const one = bench('rust->block, one block ',
    n => host.call_block_n(TABLE_BASE, 0, REGS, BLOCK_PC, n), 1_000_000);
const many = bench('rust->block, rotating  ',
    n => host.call_blocks_rotating(TABLE_BASE, N, REGS, BLOCK_PC, n), 1_000_000);

const INTERP_NS = 42;
console.log(`\nspeedup on a 12-instruction block: ${((12 * INTERP_NS) / many).toFixed(1)}x rotating, ` +
            `${((12 * INTERP_NS) / one).toFixed(1)}x hot`);
console.log('no JS on the call path: the timed loop runs inside the host wasm module');
