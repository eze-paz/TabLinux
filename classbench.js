// Per-instruction-class cost of COMPILED code, measured inside the real VM.
//
// Runs the pure-class loops tools/classbench/gen.py builds (180M instructions
// of one class each) in the restored guest, and times each between markers the
// shell computes -- so the echo of the typed command cannot false-trigger. The
// emulator's own step counter gives exact retired-instruction counts, so
// ns/instruction per class is wall/steps with no model in between.
//
//   python3 tools/classbench/gen.py && node classbench.js
//
// Two passes per class; the second is the measurement (the first warms the
// block cache, though a 10M-iteration loop is hot after its first sixteen
// iterations anyway). Slices of 500k instructions bound the marker quantisation
// error at ~0.3% of the smallest benchmark.

const fs = require('fs');
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';
const mod = require(SHIM);
const exp = mod.__wasm;

const snapshot = new Uint8Array(fs.readFileSync('kernels/shell.snap'));
const diskBytes = new Uint8Array(fs.readFileSync('kernels/disk-ext4.img'));

const vm = mod.Vm.restore(snapshot);
if (!vm) throw new Error('restore failed');
const SECT = 512;
vm.attach_disk(diskBytes.length / SECT,
    (s, c) => diskBytes.subarray(s * SECT, (s + c) * SECT),
    (s, b) => { diskBytes.set(b, s * SECT); });

const CLASSES = (process.env.ONLY || 'alu,load,store,br,jmp,ind,fp,fpl,fps').split(',');
for (const c of CLASSES) {
    vm.p9_put(`cb_${c}`, new Uint8Array(fs.readFileSync(`/tmp/classbench/cb_${c}.elf`)));
}

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

vm.jit_enable(true);
// DIAG=1: count inline-TLB misses and the interpreted-instruction bins during
// the benchmark, to attribute a class that measures slower than its target.
if (process.env.DIAG === '1') vm.interp_hist_enable(true);

let out = '';
let steps = 0;
function drain(capNs, marker) {
    const t0 = process.hrtime.bigint();
    while (Number(process.hrtime.bigint() - t0) < capNs) {
        steps += vm.run(500_000);
        const c = vm.console();
        if (c.length) out += Buffer.from(c).toString('latin1');
        pump();
        if (marker && out.includes(marker)) return true;
    }
    return !marker;
}

// The snapshot printed its prompt before it was taken, so it will not appear
// again until the guest is poked. Probe with a computed echo instead.
drain(1e9, null);
vm.input(Buffer.from('echo RDY$((20+2))\n'));
if (!drain(20e9, 'RDY22')) throw new Error('guest not responding:\n' + out.slice(-300));
vm.input(Buffer.from(
    'mkdir -p /mnt/disk; mount -t ext4 /dev/vda /mnt/disk 2>/dev/null' +
    '; insmod /mnt/disk/mod/netfs.ko 2>/dev/null; insmod /mnt/disk/mod/9pnet.ko 2>/dev/null' +
    '; insmod /mnt/disk/mod/9pnet_virtio.ko 2>/dev/null; insmod /mnt/disk/mod/9p.ko 2>/dev/null' +
    '; mkdir -p /files; mount -t 9p -o trans=virtio,version=9p2000.L,msize=131072 shared /files' +
    ' && echo MNT$((20+2))\n'));
if (!drain(30e9, 'MNT22')) throw new Error('9p mount failed:\n' + out.slice(-500));

const rows = [];
for (const c of CLASSES) {
    const up = c.toUpperCase();
    for (let pass = 0; pass < 2; pass++) {
        out = '';
        vm.input(Buffer.from(
            `cp /files/cb_${c} /x && chmod +x /x && echo ${up}S$((20+2)) && /x && echo ${up}F$((20+2))\n`));
        if (!drain(30e9, `${up}S22`)) throw new Error(`${c}: no start marker:\n` + out.slice(-400));
        const s0 = steps;
        const t0 = process.hrtime.bigint();
        if (!drain(600e9, `${up}F22`)) throw new Error(`${c}: no finish marker:\n` + out.slice(-400));
        const wall = Number(process.hrtime.bigint() - t0);
        const ran = steps - s0;
        if (pass === 1) rows.push({ c, ran, wall, ns: wall / ran });
    }
    const r = rows[rows.length - 1];
    console.log(`${c.padEnd(6)} ${(r.ran / 1e6).toFixed(0).padStart(6)}M instrs` +
        `  ${(r.wall / 1e9).toFixed(2).padStart(6)}s  ${r.ns.toFixed(2).padStart(6)} ns/instr`);
    if (process.env.DIAG === '1') {
        const TM = ['slot empty', 'evicted', 'gen: priv', 'gen: trans', 'gen: both',
            'valid, crosses page', 'valid, fits (unexpected)', 'MMIO', 'faulted'];
        const IH = ['mul', 'div/rem', 'atomic', 'csr', 'fence', 'system', 'fp',
            'cold', 'other', 'fence.i'];
        const tm = Array.from(vm.tlb_miss());
        const ih = Array.from(vm.interp_hist());
        console.log('  tlb misses: ' + tm.map((n, i) => n > 1000 ? `${TM[i]}=${(n / 1e6).toFixed(2)}M` : '')
            .filter(Boolean).join('  '));
        console.log('  interp:     ' + ih.map((n, i) => n > 1000 ? `${IH[i]}=${(n / 1e6).toFixed(2)}M` : '')
            .filter(Boolean).join('  '));
        const [entries, chains, chainInsns, rejected] = Array.from(vm.jit_stats());
        const cm = Array.from(vm.chain_miss());
        console.log(`  jit: entries=${(entries/1e6).toFixed(2)}M chains=${(chains/1e6).toFixed(2)}M rejected=${rejected} endFault=${(cm[7]/1e6).toFixed(2)}M endCap=${(cm[8]/1e6).toFixed(2)}M noBlock=${(cm[6]/1e6).toFixed(2)}M`);
    }
}

const alu = rows.find(r => r.c === 'alu').ns;
console.log('\nclass   ns/instr   vs alu   note');
for (const r of rows) {
    console.log(`${r.c.padEnd(6)} ${r.ns.toFixed(2).padStart(8)}   ${(r.ns / alu).toFixed(2).padStart(5)}x` +
        (r.c === 'ind' ? '   (3-instr units: auipc+addi+jalr, jalr ends the block)' :
         r.c === 'fp' ? '   (each op is a wasm->wasm host call)' :
         r.c === 'fpl' || r.c === 'fps' ? '   (stage-1.5 target: FP load/store)' : ''));
}
console.log('\nLoop overhead (addi+bnez back-edge) is 2 of 18 instructions in every row,');
console.log('so ratios between classes are honest; absolute ns/instr runs ~11% hot.');
