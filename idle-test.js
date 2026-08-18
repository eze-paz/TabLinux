// What does an IDLE guest cost the host?
//
// A terminal spends almost all of its life at a prompt with nothing running. If
// the emulator burns a core to sit there, the page is a battery fire and every
// throughput number is beside the point -- and this is the one regime no
// existing harness covers, because they all feed the guest work on purpose.
//
// Machine::run already parks on WFI and jumps mtime to the next deadline
// (idle_skip_mtime, capped at MAX_IDLE_SKIP = 1ms of guest time per jump). What
// was never checked is whether that path is reached with the JIT on. So:
//
//   guest-time / host-time  -- how much emulated time one second of CPU buys.
//     >> 1  the guest is sleeping properly and the host is nearly free
//     ~= 1  the emulator is spinning through an idle loop in real time
//   instructions retired    -- an idle guest should retire very few
//
// The comparison run does the same wall-clock work with the guest busy, so the
// two numbers are directly readable against each other.

const fs = require('fs');
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';
const snapshot = fs.readFileSync('kernels/shell.snap');

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
                mem: exp.memory, __indirect_function_table: table,
                load8u: exp.load8u, load16u: exp.load16u, load32u: exp.load32u,
                load64: exp.load64, store8: exp.store8, store16: exp.store16,
                store32: exp.store32, store64: exp.store64, csr: exp.csr, fp: exp.fp,
            },
        });
        vm.jit_installed(base);
        return count;
    };
}

// mtime ticks per second, from the device tree the guest was built against.
const MTIME_HZ = Number(process.env.MTIME_HZ || 10_000_000);

function measure(label, input, seconds) {
    const mod = fresh();
    const vm = mod.Vm.restore(snapshot);
    vm.jit_enable(true);
    const pump = makeCompiler(vm, mod.__wasm);
    if (input) vm.input(Buffer.from(input));

    // Settle: reach the prompt (or get the workload going) and warm the JIT.
    let warm = 0;
    while (warm < 40_000_000) { warm += vm.run(2_000_000); vm.console(); pump(); }

    const t0 = process.hrtime.bigint();
    const m0 = vm.mtime ? Number(vm.mtime()) : 0;
    let steps = 0;
    // Drive the emulator for a fixed WALL duration, the way a browser rAF loop
    // would, rather than for a fixed instruction count -- an idle guest would
    // take an unbounded time to retire a fixed count.
    while (Number(process.hrtime.bigint() - t0) < seconds * 1e9) {
        steps += vm.run(2_000_000);
        vm.console();
        pump();
    }
    const wall = Number(process.hrtime.bigint() - t0) / 1e9;
    const m1 = vm.mtime ? Number(vm.mtime()) : 0;
    const guest = (m1 - m0) / MTIME_HZ;

    console.log(`${label.padEnd(22)} host ${wall.toFixed(2)}s  guest ${guest.toFixed(2)}s` +
        `  ratio ${(guest / wall).toFixed(2)}x` +
        `  retired ${(steps / 1e6).toFixed(1)}M` +
        `  (${(steps / wall / 1e6).toFixed(1)} MIPS)`);
    return { wall, guest, steps };
}

const SECS = Number(process.env.SECS || 4);
console.log(`driving the emulator for ${SECS}s of host time in each case\n`);
const idle = measure('idle at the prompt', null, SECS);
const busy = measure('busy (md5sum loop)', 'while :; do ls -la /bin | md5sum; done\n', SECS);

console.log('');
if (idle.guest / idle.wall > 5) {
    console.log(`idle fast-forward WORKS: ${(idle.guest / idle.wall).toFixed(0)}x real time,` +
        ` and the idle guest retired ${(busy.steps / idle.steps).toFixed(0)}x fewer instructions than the busy one.`);
} else {
    console.log(`idle fast-forward IS NOT PAYING: an idle guest advances only` +
        ` ${(idle.guest / idle.wall).toFixed(2)}x real time while retiring ${(idle.steps / 1e6).toFixed(1)}M` +
        ` instructions. The host is spinning through an idle loop.`);
}
