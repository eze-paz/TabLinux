// Run the pure-Python benchmark inside the emulated guest and report guest
// MIPS over the marked window, with the JIT's own instrumentation.
//
// The window is bounded by the PYBENCH START/END markers the guest prints, so
// mount + interpreter startup are excluded from the score. Wall time is host
// hrtime; instructions are vm.steps() deltas — the guest clock is never read.
//
//   node pybench/pybench-vm.js            # jit on (default)
//   JIT=0 node pybench/pybench-vm.js      # interpreter only
//   SCALE=2 node pybench/pybench-vm.js    # heavier workload
//   JITNODE=/tmp/jitnode_b node ...       # measure a different build
//
// Exits nonzero if the benchmark never finishes or python errors.

const fs = require('fs');
const path = require('path');

const ROOT = path.join(__dirname, '..');
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';
const JIT = process.env.JIT !== '0';
const SCALE = process.env.SCALE || '1';
// Run a single named phase (see pybench.py PHASES) — for microbenchmarks
// where one cost should dominate the measured window.
const ONLY = process.env.ONLY || '';
const QUIET = process.env.QUIET === '1';

const snapshot = fs.readFileSync(path.join(ROOT, 'kernels/shell.snap'));
const disk = fs.readFileSync(path.join(ROOT, 'kernels/disk-python.img'));

function fresh() {
    delete require.cache[require.resolve(SHIM)];
    return require(SHIM);
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
                table_base: new WebAssembly.Global({ value: 'i32', mutable: false }, base),
            },
        });
        vm.jit_installed(base);
        return count;
    };
}

function attach(vm) {
    const ok = vm.attach_disk(
        disk.length / 512,
        (sector, count) => new Uint8Array(
            disk.buffer, disk.byteOffset + Number(sector) * 512, Number(count) * 512),
        (sector, bytes) => Buffer.from(bytes).copy(disk, Number(sector) * 512),
    );
    if (!ok) throw new Error('attach_disk refused (size mismatch?)');
}

function run() {
    const mod = fresh();
    const vm = mod.Vm.restore(snapshot);
    if (!vm) throw new Error('snapshot restore failed');
    attach(vm);

    let pump = () => 0;
    if (JIT) {
        vm.jit_enable(true);
        vm.interp_hist_enable?.(true);
        pump = makeCompiler(vm, mod.__wasm);
    }

    vm.input(Buffer.from(
        'mkdir -p /mnt/disk && mount -t ext4 /dev/vda /mnt/disk && ' +
        'LD_LIBRARY_PATH=/mnt/disk/usr/lib PYTHONHOME=/mnt/disk/usr ' +
        `/mnt/disk/usr/bin/python3 /mnt/disk/bench/pybench.py ${SCALE} ${ONLY}\n`.replace(/ \n$/, '\n')));

    const grab = () => ({
        stats: JIT ? Array.from(vm.jit_stats()) : [],
        miss: JIT && vm.chain_miss ? Array.from(vm.chain_miss()) : [],
        hist: JIT && vm.interp_hist ? Array.from(vm.interp_hist()) : [],
        tlb: JIT && vm.tlb_miss ? Array.from(vm.tlb_miss()) : [],
    });
    let atStart = null, lastGrab = null;

    if (process.env.CHAINMAX) vm.set_chain_max(Number(process.env.CHAINMAX));

    let out = '';
    let t0 = 0n, s0 = 0n, started = false;
    const phases = [];   // [name, ns, steps] per marker-to-marker segment
    let lastT = 0n, lastS = 0n, seenLen = 0;
    const deadline = Date.now() + 30 * 60 * 1000;

    while (true) {
        vm.run(2_000_000);
        pump();
        const c = vm.console();
        if (c.length) out += Buffer.from(c).toString('latin1');

        // Scan only the unscanned tail for markers, in order of appearance.
        let idx;
        while ((idx = out.indexOf('PYBENCH', seenLen)) !== -1) {
            const eol = out.indexOf('\n', idx);
            if (eol === -1) break;                  // marker line incomplete
            const line = out.slice(idx, eol);
            seenLen = eol + 1;
            const now = process.hrtime.bigint();
            const steps = BigInt(vm.steps());
            if (line.startsWith('PYBENCH START')) {
                started = true; t0 = now; s0 = steps; atStart = grab(); lastGrab = atStart;
            } else if (started) {
                const g = grab();
                phases.push([line.replace('PYBENCH ', '').split(' ')[0],
                             Number(now - lastT), Number(steps - lastS),
                             g.stats.map((v, i) => v - lastGrab.stats[i]),
                             g.miss.map((v, i) => v - lastGrab.miss[i]),
                             g.hist.map((v, i) => v - lastGrab.hist[i])]);
                lastGrab = g;
            }
            lastT = now; lastS = steps;
            if (line.startsWith('PYBENCH END')) {
                const atEnd = grab();
                const d = (k) => atEnd[k].map((v, i) => v - (atStart[k][i] || 0));
                return report(vm, out, Number(now - t0), Number(steps - s0), phases,
                              { stats: d('stats'), miss: d('miss'), hist: d('hist'), tlb: d('tlb') });
            }
        }
        if (Date.now() > deadline) {
            console.error('TIMEOUT. Console tail:\n' + out.slice(-3000));
            process.exit(1);
        }
    }
}

function report(vm, out, ns, steps, phases, w) {
    if (!QUIET) {
        console.log('--- console tail ---');
        console.log(out.split('PYBENCH')[0].slice(-400));
    }
    const mips = steps / (ns / 1e9) / 1e6;
    console.log(`RESULT jit=${JIT ? 1 : 0} scale=${SCALE}`);
    console.log(`RESULT wall_ms=${(ns / 1e6).toFixed(0)} steps=${steps} mips=${mips.toFixed(2)}`);
    for (const [name, pns, psteps, pst, pmiss, phist] of phases) {
        let extra = '';
        if (JIT && pst && pst.length) {
            const interp = phist.reduce((a, c) => a + c, 0);
            extra = ` entries=${pst[0]} chainInsns=${pst[2]}` +
                    ` cov=${(100 * pst[2] / psteps).toFixed(1)}%` +
                    ` evicted=${pmiss[1]} budget=${pmiss[5]} cap=${pmiss[8]}` +
                    ` interp=${(100 * interp / psteps).toFixed(1)}%`;
        }
        console.log(`PHASE ${name.padEnd(8)} wall_ms=${(pns / 1e6).toFixed(0).padStart(7)} ` +
                    `mips=${(psteps / (pns / 1e9) / 1e6).toFixed(1)}${extra}`);
    }
    if (JIT && w) {
        const [entries, chains, chainInsns, rejected] = w.stats;
        console.log(`JITSTATS entries=${entries} chains=${chains} ` +
                    `chainInsns=${chainInsns} rejected=${rejected} ` +
                    `coverage=${(100 * chainInsns / steps).toFixed(1)}% ` +
                    `insnsPerChain=${(chainInsns / chains).toFixed(1)} ` +
                    `insnsPerEntry=${(chainInsns / entries).toFixed(1)}`);
        if (w.miss.length) {
            const NAMES = ['empty', 'evicted', 'genpriv', 'gentrans', 'genboth',
                           'budget', 'noblock', 'fault', 'cap'];
            console.log('CHAINMISS ' + w.miss.map((v, i) => `${NAMES[i]}=${v}`).join(' '));
        }
        if (w.tlb.length) {
            console.log('TLBMISS ' + w.tlb.join(' '));
        }
        if (vm.sfence_filter) {
            const [skips, hits] = Array.from(vm.sfence_filter());
            console.log(`SFENCE skips=${skips} hits=${hits}`);
        }
        if (vm.gen_bump) {
            const NAMES = ['satp', 'sfence-all', 'sfence-page', 'other'];
            console.log('GENBUMP ' + Array.from(vm.gen_bump())
                .map((v, i) => `${NAMES[i]}=${v}`).join(' '));
        }
        if (w.hist.length) {
            const NAMES = ['mul', 'divrem', 'atomic', 'csr', 'fence', 'system', 'fp',
                           'cold', 'other', 'fencei'];
            const interp = w.hist.reduce((a, c) => a + c, 0);
            console.log(`INTERP total=${interp} (${(100 * interp / steps).toFixed(1)}% of window) ` +
                        w.hist.map((v, i) => `${NAMES[i]}=${v}`).join(' '));
        }
    }
    const m = out.match(/PYBENCH END (.*)/);
    console.log('CHECK ' + (m ? m[1].trim() : 'missing'));
}

run();
