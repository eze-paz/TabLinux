// Read the inlined-TLB miss taxonomy on the steady-state shell workload.
//
// Counters only -- deterministic, so this needs no A/B discipline. The question
// is whether the inline TLB is being thrown away on every trap, which is what
// stamping its entries with a privilege-bearing generation word would do.
const fs = require('fs');
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';
const snapshot = fs.readFileSync('kernels/shell.snap');

const mod = require(SHIM);
const vm = mod.Vm.restore(snapshot);
if (!vm) throw new Error('restore failed');
vm.jit_enable(true);
vm.interp_hist_enable(true);
vm.input(Buffer.from('while :; do ls -la /bin | md5sum; done\n'));

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

const BUDGET = Number(process.env.BUDGET || 400_000_000);
let steps = 0;
while (steps < BUDGET) { steps += vm.run(2_000_000); vm.console(); pump(); }

const NAMES = ['slot empty', 'evicted', 'gen: priv', 'gen: trans', 'gen: both',
    'valid, crosses page', 'valid, fits (unexpected)', 'MMIO (never cached)',
    'translation faulted'];
const m = Array.from(vm.tlb_miss());
const total = m.reduce((a, c) => a + c, 0) || 1;
const [entries, chains, chainInsns] = Array.from(vm.jit_stats());

console.log(`instructions      ${(steps / 1e6).toFixed(0)}M  (${(100 * chainInsns / steps).toFixed(1)}% compiled)`);
console.log(`inline TLB misses ${(total / 1e6).toFixed(2)}M` +
    `  = ${(1000 * total / chainInsns).toFixed(1)} per 1000 compiled instructions`);
console.log('');
m.map((n, i) => [NAMES[i], n])
    .sort((x, y) => y[1] - x[1])
    .filter(([, n]) => n > 0)
    .forEach(([name, n]) => console.log(
        `  ${name.padEnd(26)} ${(n / 1e6).toFixed(2).padStart(8)}M  ${(100 * n / total).toFixed(1).padStart(5)}%`));

// Memory ops are roughly a quarter to a third of instructions, so this is the
// rate that matters: what fraction of compiled accesses fall out of line.
console.log(`\nassuming ~28% of compiled instructions are accesses,` +
    ` the probe misses ~${(100 * total / (0.28 * chainInsns)).toFixed(1)}% of the time`);

// What moved trans_gen, which is what killed 93% of those entries.
const CAUSES = ["satp write (changed)", "sfence.vma global", "sfence.vma one page",
    "other (restore, fence.i)"];
const g = Array.from(vm.gen_bump());
const gt = g.reduce((a, c) => a + c, 0) || 1;
console.log(`\ntrans_gen bumps   ${(gt / 1e3).toFixed(1)}k  (each one voids the whole inline TLB)`);
g.map((n, i) => [CAUSES[i], n]).sort((x, y) => y[1] - x[1]).filter(([, n]) => n > 0)
    .forEach(([name, n]) => console.log(
        `  ${name.padEnd(26)} ${(n / 1e3).toFixed(1).padStart(8)}k  ${(100 * n / gt).toFixed(1).padStart(5)}%`));
console.log(`\n=> ${(total / gt).toFixed(0)} inline-TLB misses per bump`);

const SLOT = ["empty (first use)", "evicted Cold(1) (no progress lost)",
    "evicted Cold(2+) (hotness reset)", "evicted Queued/Compiled/Rejected"];
const sl = Array.from(vm.slot_state());
const st = sl.reduce((a, c) => a + c, 0) || 1;
console.log(`\nblock-cache slot mismatches ${(st / 1e3).toFixed(1)}k of ${(entries / 1e3).toFixed(1)}k block entries`);
sl.forEach((n, i) => console.log(`  ${SLOT[i].padEnd(36)} ${(n / 1e3).toFixed(1).padStart(9)}k  ${(100 * n / st).toFixed(1).padStart(5)}%`));
