// Interleaved A/B for the Python-in-guest benchmark.
//
// Sequential comparison is unsound on this box — wall MIPS swings ~30% between
// identical runs as host load drifts. Alternating the two builds inside each
// pair makes both see the same drift; the per-pair ratio is the statistic, and
// the median across pairs is the answer.
//
//   node pybench/pybench-ab.js /tmp/jitnode_old /tmp/jitnode_new [pairs] [-- env=val ...]
//
// Each run is a fresh node process (fresh V8, fresh wasm instance). The guest
// work is deterministic, so runs also cross-check: any CHECK mismatch aborts.

const { spawnSync } = require('child_process');
const path = require('path');

const args = process.argv.slice(2);
const A = args[0], B = args[1];
const pairs = Number(args[2]) || 6;
// KEY=VAL applies to both runs; A.KEY=VAL / B.KEY=VAL to one side only.
const extraEnv = {}, envA = {}, envB = {};
for (const kv of args.slice(3)) {
    const m = kv.match(/^(?:([AB])\.)?(\w+)=(.*)$/);
    if (!m) continue;
    (m[1] === 'A' ? envA : m[1] === 'B' ? envB : extraEnv)[m[2]] = m[3];
}

function run(shim) {
    const side = shim === A ? envA : envB;
    // NODE_FLAGS (global or per-side) become V8 flags for the child, e.g.
    // B.NODE_FLAGS=--no-liftoff to measure the TurboFan ceiling on one side.
    const flags = (side.NODE_FLAGS ?? extraEnv.NODE_FLAGS ?? '').split(' ').filter(Boolean);
    const r = spawnSync('node', [...flags, path.join(__dirname, 'pybench-vm.js')], {
        env: { ...process.env, ...extraEnv, ...side, JITNODE: shim, QUIET: '1' },
        encoding: 'utf8',
        timeout: 30 * 60 * 1000,
    });
    const out = (r.stdout || '') + (r.stderr || '');
    const mips = out.match(/mips=([\d.]+)/);
    const check = out.match(/CHECK (.*)/);
    if (!mips || !check) {
        console.error(`run failed for ${shim}:\n` + out.slice(-2000));
        process.exit(1);
    }
    return { mips: Number(mips[1]), check: check[1].trim(), out };
}

let ref = null;
const ratios = [];
for (let i = 1; i <= pairs; i++) {
    // Alternate which build goes first inside the pair (ABBA) so slow drift
    // within a pair cancels too.
    const first = i % 2 === 1 ? A : B, second = first === A ? B : A;
    const r1 = run(first), r2 = run(second);
    const a = first === A ? r1 : r2, b = first === A ? r2 : r1;

    for (const r of [a, b]) {
        if (ref === null) ref = r.check;
        else if (r.check !== ref) {
            console.error(`CHECK MISMATCH:\n  ref ${ref}\n  got ${r.check}`);
            process.exit(1);
        }
    }
    ratios.push(b.mips / a.mips);
    console.log(`pair ${i}: A=${a.mips.toFixed(1)} B=${b.mips.toFixed(1)} ` +
                `ratio=${(b.mips / a.mips).toFixed(3)}`);
}

ratios.sort((x, y) => x - y);
const mid = ratios.length >> 1;
const median = ratios.length % 2 ? ratios[mid] : (ratios[mid - 1] + ratios[mid]) / 2;
console.log(`\nmedian B/A = ${median.toFixed(3)}  ` +
            `(${ratios.filter(r => r > 1).length}/${ratios.length} pairs favor B)`);
console.log(`ratios: ${ratios.map(r => r.toFixed(3)).join(' ')}`);
console.log('guest work identical across all runs (CHECK matched)');
