// A JIT A/B that finishes in a minute and states what it can actually resolve.
//
// Replaces jit-ab.py, which took 40 minutes to return a number that disagreed
// with itself -- its first two pairs on one run were 0.838 and 1.091. Four
// things were wrong, and only the last is statistics.
//
//  1. It spent 93% of its time measuring nothing. Every `jit-vm-test.js` run
//     boots the INTERPRETER for 400M instructions -- 78s at 5.1 MIPS -- to
//     produce a correctness reference, then runs the JIT for 6s. An A/B needs
//     the second number. Correctness is a separate question, asked once by the
//     test battery rather than sixteen times here.
//
//  2. It compared across processes minutes apart, so every pair straddled
//     whatever else the host was doing. Here both builds are two VMs in ONE
//     process, alternating every few tens of milliseconds, so drift and
//     scheduler mood land on both halves of a pair nearly equally.
//
//  3. It benchmarked a boot. A boot is heterogeneous -- decompression, device
//     probe and fault storms run at genuinely different speeds -- so slices are
//     not comparable unless the two runs stay in lockstep, which they do not
//     once interrupt timing nudges them apart. An endless shell loop reaches
//     steady state, which makes slices interchangeable and drift harmless.
//
//  4. Wall clock. See the note on clocks below -- this cost a full rebuild of
//     the instrument, because the obvious fix is also wrong.
//
// ## Which clock, and the trap in between
//
// /proc/self/schedstat looks like the right answer: field 0 is CPU nanoseconds,
// so it excludes time the vCPU spent descheduled by the Windows host, which is
// the largest error term on this box and is invisible from inside it.
//
// It is the wrong answer here. **Measured, it only advances every ~4ms** -- it
// is updated on the scheduler tick, and its nanosecond unit is a unit, not a
// resolution. Against a 33ms slice that is 12% quantisation, and worse, it
// makes many A and B slices land on an identical quantised value, so their
// ratio comes out at exactly 1.0. An earlier version of this file reported a
// null resolution of 0.0% for precisely that reason: the median sat on a spike
// of exact ones and every bootstrap resample found it there. A instrument that
// reports impossible precision is more dangerous than a noisy one.
//
// So: time with hrtime (152ns resolution, measured) and deal with descheduling
// by estimator rather than by clock. Interference can only ever make a slice
// slower, so within a block of consecutive slices the FASTEST is the least
// contaminated estimate of what the build can do. Blocks of BEST slices, one
// ratio per block. schedstat is still read, but only as a diagnostic: it says
// how much descheduling happened, without being trusted to time anything.
//
//   node jit-fast-ab.js                 # /tmp/jitnode_old vs /tmp/jitnode_new
//   node jit-fast-ab.js --null          # THE VALIDATION: same build both sides
//   SLICES=600 BLOCK=8 node jit-fast-ab.js
//
// RUN --null FIRST AND BELIEVE NOTHING SMALLER THAN WHAT IT REPORTS.
//
// ## Do not just raise SLICES to get a tighter interval
//
// SLICES=1200 dies in V8's GC inside WasmTableObject::Grow. Blocks accumulate
// for the whole run and the harness grows the function table for every batch,
// with no equivalent of the engine's own JIT_CACHE_MAX discard, so a long run
// walks into an out-of-memory with two VMs resident. ~480 slices is what fits.
// Want more confidence? Run the whole thing twice and check the two medians
// agree -- two independent 100-second runs are better evidence than one long
// one anyway, because they resample the host's mood as well as the guest's.

const fs = require('fs');

const NULL_MODE = process.argv.includes('--null');
const DIR_A = process.env.JITNODE_A || '/tmp/jitnode_old';
const DIR_B = NULL_MODE ? DIR_A : (process.env.JITNODE_B || '/tmp/jitnode_new');

const SLICES = Number(process.env.SLICES || 480);
const INSNS = Number(process.env.INSNS || 2_000_000);
const BLOCK = Number(process.env.BLOCK || 8);
const WARMUP = Number(process.env.WARMUP || 60_000_000);
const BURN = Number(process.env.BURN || 8);

// Endless, so the guest never falls idle. An idle guest parks in WFI and the
// emulator skips its clock forward, retiring no instructions -- that would
// quietly turn a throughput measurement into a measurement of nothing.
const WORK = process.env.WORK || 'while :; do ls -la /bin | md5sum; done\n';

const snapshot = fs.readFileSync('kernels/shell.snap');

const cpuNs = () => Number(fs.readFileSync('/proc/self/schedstat', 'utf8').split(' ')[0]);

function load(dir) {
    const p = require.resolve(dir + '/riscv_wasm.js');
    delete require.cache[p];
    return require(p);
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
        new WebAssembly.Instance(new WebAssembly.Module(bytes), {
            env: {
                mem: exp.memory,
                __indirect_function_table: table,
                load8u: exp.load8u, load16u: exp.load16u,
                load32u: exp.load32u, load64: exp.load64,
                store8: exp.store8, store16: exp.store16,
                store32: exp.store32, store64: exp.store64,
                csr: exp.csr, fp: exp.fp,
                // Blocks are relocatable: the active element segment installs at
                // an imported base rather than a baked offset. `base` is where we
                // just grew the table to, matching what jit_installed expects.
                table_base: new WebAssembly.Global({ value: 'i32', mutable: false }, base),
            },
        });
        vm.jit_installed(base);
        return count;
    };
}

function spawn(dir, label) {
    const mod = load(dir);
    const vm = mod.Vm.restore(snapshot);
    if (!vm) throw new Error('snapshot restore failed for ' + dir);
    vm.jit_enable(true);
    vm.input(Buffer.from(WORK));
    return { label, dir, vm, pump: makeCompiler(vm, mod.__wasm), console: Buffer.alloc(0), steps: 0 };
}

function slice(s, insns) {
    const c0 = cpuNs();
    const t0 = process.hrtime.bigint();
    const ran = s.vm.run(insns);
    const wall = Number(process.hrtime.bigint() - t0);
    const cpu = cpuNs() - c0;
    s.steps += ran;
    const c = s.vm.console();
    if (c.length && s.console.length < (1 << 20)) {
        s.console = Buffer.concat([s.console, Buffer.from(c)]);
    }
    s.pump();
    return { mips: ran / (wall / 1e9) / 1e6, ran, wall, cpu };
}

const sorted = xs => [...xs].sort((a, b) => a - b);
function median(xs) {
    const v = sorted(xs), m = v.length >> 1;
    return v.length % 2 ? v[m] : (v[m - 1] + v[m]) / 2;
}
const quantile = (xs, q) => sorted(xs)[Math.min(xs.length - 1, Math.max(0, Math.round(q * (xs.length - 1))))];

/// Confidence interval of the MEDIAN, by bootstrap.
///
/// The spread of individual blocks is not the resolution of the instrument: the
/// verdict is a median over many of them, and a median is far steadier than its
/// samples. Resampling says how much steadier, and that is the number a real
/// result has to beat. Quoting the raw spread instead would set the bar ~10x
/// too high and reject true wins.
function bootstrapCI(xs, iters) {
    const n = xs.length;
    // xorshift32, deterministic: an instrument that reports a different
    // interval each time it is asked about identical data invites the "run it
    // again until it agrees" habit this file exists to end. All shifts are
    // unsigned -- a signed >> here silently degrades the generator.
    let seed = 0x2545f491 >>> 0;
    const rnd = () => {
        seed ^= (seed << 13) >>> 0; seed >>>= 0;
        seed ^= seed >>> 17;
        seed ^= (seed << 5) >>> 0; seed >>>= 0;
        return seed / 0x100000000;
    };
    const meds = [];
    const buf = new Array(n);
    for (let it = 0; it < iters; it++) {
        for (let i = 0; i < n; i++) buf[i] = xs[(rnd() * n) | 0];
        meds.push(median(buf));
    }
    const v = sorted(meds);
    return [v[Math.floor(0.025 * iters)], v[Math.floor(0.975 * iters)]];
}

console.log(NULL_MODE ? `NULL TEST: ${DIR_A} against itself` : `A = ${DIR_A}\nB = ${DIR_B}`);
console.log(`${SLICES} slices x ${(INSNS / 1e6).toFixed(1)}M insns, best-of-${BLOCK} blocks\n`);

const A = spawn(DIR_A, 'A');
const B = spawn(DIR_B, 'B');

process.stdout.write('warming up... ');
for (const s of [A, B]) {
    let done = 0;
    while (done < WARMUP) done += slice(s, 2_000_000).ran;
}
console.log(`${(A.steps / 1e6).toFixed(0)}M / ${(B.steps / 1e6).toFixed(0)}M instructions in`);

const rawA = [], rawB = [], wallSum = [], cpuSum = [];
for (let i = 0; i < SLICES + BURN; i++) {
    // ABBA: order flips each slice, so a trend across the run cannot favour
    // whichever build happens to go first.
    let a, b;
    if (i % 2 === 0) { a = slice(A, INSNS); b = slice(B, INSNS); }
    else { b = slice(B, INSNS); a = slice(A, INSNS); }
    if (i < BURN) continue;
    rawA.push(a.mips); rawB.push(b.mips);
    wallSum.push(a.wall + b.wall); cpuSum.push(a.cpu + b.cpu);
}

// One ratio per block, from the fastest slice each build managed inside it.
// Interference only ever subtracts speed, so the fastest slice in a window is
// the cleanest look at the build; comparing the two cleanest is what makes this
// robust to a busy host without needing a clock that can see the host.
const ratios = [], bestA = [], bestB = [];
for (let i = 0; i + BLOCK <= rawA.length; i += BLOCK) {
    const pa = Math.max(...rawA.slice(i, i + BLOCK));
    const pb = Math.max(...rawB.slice(i, i + BLOCK));
    bestA.push(pa); bestB.push(pb); ratios.push(pb / pa);
}

const med = median(ratios);
const [ciLo, ciHi] = bootstrapCI(ratios, 4000);
const halfWidth = Math.max(ciHi - med, med - ciLo);
const dutyCycle = cpuSum.reduce((x, y) => x + y, 0) / wallSum.reduce((x, y) => x + y, 0);

console.log(`\nA  best-of-${BLOCK} median ${median(bestA).toFixed(1)} MIPS   (all slices: median ${median(rawA).toFixed(1)})`);
console.log(`B  best-of-${BLOCK} median ${median(bestB).toFixed(1)} MIPS   (all slices: median ${median(rawB).toFixed(1)})`);
console.log(`\npaired ratio B/A over ${ratios.length} blocks`);
console.log(`  median            ${med.toFixed(4)}`);
console.log(`  IQR               ${quantile(ratios, 0.25).toFixed(4)} .. ${quantile(ratios, 0.75).toFixed(4)}`);
console.log(`  95% CI of median  ${ciLo.toFixed(4)} .. ${ciHi.toFixed(4)}  (+/-${(100 * halfWidth).toFixed(2)}%)`);
console.log(`\nhost duty cycle ${(100 * dutyCycle).toFixed(0)}%  (CPU time / wall time; low means a busy box)`);

const n = Math.min(A.console.length, B.console.length);
const same = A.console.slice(0, n).equals(B.console.slice(0, n));
console.log(`console prefix (${n} B): ${same ? 'identical' : '*** DIVERGED -- CORRECTNESS FAILURE ***'}`);

if (NULL_MODE) {
    const bias = Math.abs(med - 1);
    console.log(`\nNULL RESULT`);
    console.log(`  bias          ${(100 * bias).toFixed(2)}%  (median vs 1.0)`);
    console.log(`  CI half-width ${(100 * halfWidth).toFixed(2)}%`);
    console.log(`  => resolution ${(100 * (bias + halfWidth)).toFixed(1)}%  -- treat anything smaller as no result`);
    // A CI far tighter than the block spread would mean the estimator has
    // collapsed onto repeated values rather than genuinely converged, which is
    // exactly how the schedstat version fooled itself.
    const spread = quantile(ratios, 0.75) - quantile(ratios, 0.25);
    if (halfWidth < spread / (20 * Math.sqrt(ratios.length))) {
        console.log('  SUSPICIOUS: CI implausibly tight for this spread -- check for tied values.');
    }
    console.log(bias > 0.02 ? '  NULL FAILED: biased against itself. Do not use it.' : '  Null is unbiased.');
} else {
    console.log(`\nVERDICT: ` + (
        (med - 1) > halfWidth ? `B is ${((med - 1) * 100).toFixed(1)}% FASTER (+/-${(100 * halfWidth).toFixed(1)}%)`
            : (1 - med) > halfWidth ? `B is ${((1 - med) * 100).toFixed(1)}% SLOWER (+/-${(100 * halfWidth).toFixed(1)}%)`
                : `no resolvable difference (${((med - 1) * 100).toFixed(1)}% +/-${(100 * halfWidth).toFixed(1)}%)`));
    if (!same) process.exit(1);
}
