// How often is jalr executed in a realistic workload, and how predictable is
// its target? That decides whether a QEMU-style cached indirect-branch target
// (or a return-address shadow stack) is worth building.
//
// Run with the JIT OFF so every instruction passes through the interpreter and
// is counted; the instruction MIX is identical whether the JIT is on or off,
// since the guest executes the same code either way. The counters are snapshot
// right before the measured workload so boot doesn't pollute the numbers.
//
// Workload: fork/exec + directory listing + hashing + a compiled binary --
// dense in function calls and returns, which is exactly where jalr lives.

const fs = require('fs');
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';
const mod = require(SHIM);

const snapshot = new Uint8Array(fs.readFileSync('kernels/shell.snap'));
const diskBytes = new Uint8Array(fs.readFileSync('kernels/disk-ext4.img'));
const vm = mod.Vm.restore(snapshot);
if (!vm) throw new Error('restore failed');
const SECT = 512;
vm.attach_disk(diskBytes.length / SECT,
    (s, c) => diskBytes.subarray(s * SECT, (s + c) * SECT),
    (s, b) => { diskBytes.set(b, s * SECT); });
// JIT stays OFF: no compiler wiring, every instruction interpreted and counted.

let out = '', steps = 0;
function drain(capNs, marker) {
    const t0 = process.hrtime.bigint();
    while (Number(process.hrtime.bigint() - t0) < capNs) {
        steps += vm.run(500_000);
        const c = vm.console();
        if (c.length) out += Buffer.from(c).toString('latin1');
        if (marker && out.includes(marker)) return true;
    }
    return !marker;
}

drain(1e9, null);
vm.input(Buffer.from('echo RDY$((20+2))\n'));
if (!drain(60e9, 'RDY22')) throw new Error('no prompt:\n' + out.slice(-300));

const before = Array.from(vm.jalr_stats());
const stepsBefore = steps;
const t0 = process.hrtime.bigint();

// The measured workload: three passes of a fork/exec + ls + hash pipeline.
out = '';
vm.input(Buffer.from(
    'for i in 1 2 3; do ls -la /bin /etc /usr 2>/dev/null | md5sum; ' +
    'grep -c . /etc/* 2>/dev/null | md5sum; done; echo WL$((20+2))\n'));
if (!drain(300e9, 'WL22')) throw new Error('workload did not finish:\n' + out.slice(-300));

const after = Array.from(vm.jalr_stats());
const wall = Number(process.hrtime.bigint() - t0) / 1e9;
const ran = steps - stepsBefore;

const jalr = after[0] - before[0];
const ret = after[1] - before[1];
const mono = after[2] - before[2];
const sp = after[3] - before[3];

console.log(`workload: ${(ran / 1e6).toFixed(0)}M instructions in ${wall.toFixed(1)}s (interpreter)\n`);
console.log(`jalr executed        ${(jalr / 1e6).toFixed(2)}M  = ${(100 * jalr / ran).toFixed(2)}% of all instructions`);
console.log(`  of which returns   ${(ret / 1e6).toFixed(2)}M  = ${(100 * ret / jalr).toFixed(0)}% of jalr`);
console.log(`  indirect (non-ret) ${((jalr - ret) / 1e6).toFixed(2)}M  = ${(100 * (jalr - ret) / jalr).toFixed(0)}% of jalr\n`);
console.log(`predictability of the target:`);
console.log(`  last-target cache  ${(100 * mono / jalr).toFixed(1)}% of ALL jalr hit`);
console.log(`  shadow stack       ${(100 * sp / ret).toFixed(1)}% of RETURNS hit`);

// Ceiling estimate. jalr measured ~3.3x an alu at ~10ns; a predicted target
// would cut a hit from ~3.3x toward ~1.5x. Wall share and recoverable share:
const JALR_NS = 33, ALU_NS = 10;
const jalrWall = jalr * JALR_NS;
const totalWall = jalrWall + (ran - jalr) * ALU_NS; // crude: rest at ~alu
const predictedFrac = (mono + (sp - mono > 0 ? 0 : 0)) / jalr; // last-target coverage
const bestPred = Math.max(mono / jalr, (ret / jalr) * (sp / Math.max(ret, 1)));
console.log(`\ncrude ceiling:`);
console.log(`  jalr is ~${(100 * jalrWall / totalWall).toFixed(1)}% of wall (at 3.3x alu)`);
console.log(`  best predictor covers ~${(100 * bestPred).toFixed(0)}% of jalr`);
console.log(`  cutting those from 3.3x to ~1.5x recovers ~${(100 * (jalrWall / totalWall) * bestPred * (1.8 / 3.3)).toFixed(1)}% of wall`);
console.log('  (rough: single unpaired estimate, ranking only, per the postmortem)');
