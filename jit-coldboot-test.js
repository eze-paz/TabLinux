// Cold boot under the JIT, diffed against the interpreter.
//
// jit-vm-test.js restores a snapshot, and the snapshot resumes at a shell —
// so every instruction the kernel executes on its way there has never once
// been run compiled. That is the entire gap this closes, and there is a hang
// living in it: early init stops just after
//
//     Mountpoint-cache hash table entries: ...
//
// with the guest clock frozen while the host burns instructions at full speed.
//
// Console output is the oracle, as in jit-vm-test.js. The interpreter's boot is
// the reference; the JIT's must match it prefix-for-prefix. Where it stops
// matching is where to look.
//
//   node jit-coldboot-test.js            # both, diff
//   JITNODE=/tmp/jitnode_old node ...    # against another build

const fs = require('fs');
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';

const RAM_MB = Number(process.env.RAM_MB) || 1024;
// Enough to reach a shell interpreted, with room to spare. The hang shows up
// long before this; the budget only has to be big enough that a healthy boot
// is unambiguous.
const BUDGET = Number(process.env.BUDGET) || 3_000_000_000;

const kernel = fs.readFileSync('kernels/vmlinuz-lts.raw');
const initrd = fs.readFileSync('kernels/boot/initramfs-lts');
const dtb = fs.readFileSync('kernels/boot.dtb');

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

function coldBoot(useJit, budget, label) {
    const mod = fresh();
    const vm = new mod.Vm(kernel, initrd, dtb, RAM_MB);
    const pump = useJit ? (vm.jit_enable(true), makeCompiler(vm, mod.__wasm)) : () => 0;

    let out = Buffer.alloc(0);
    let steps = 0;
    // A hang is silent, not a crash: the loop keeps retiring instructions while
    // the guest makes no progress. Watch for the console going quiet while
    // steps keep climbing, and stop rather than running the whole budget.
    let lastLen = 0;
    let quietSteps = 0;
    // Generous, because the interpreter is genuinely slow rather than stuck and a
    // boot has long quiet stretches. Too small a value reports the interpreter
    // as hung, which it is not.
    const QUIET_LIMIT = Number(process.env.QUIET) || 1_500_000_000;

    const t0 = process.hrtime.bigint();
    while (steps < budget) {
        if (process.env.REALTIME === "1") vm.set_host_ns(Number(process.hrtime.bigint()));
        steps += vm.run(2_000_000);
        if (process.env.REALTIME === "1" && vm.idle_ms && vm.idle_ms() > 0) {
            const until = process.hrtime.bigint() + BigInt(Math.round(Math.min(20, vm.idle_ms()) * 1e6));
            while (process.hrtime.bigint() < until) { /* stand in for setTimeout */ }
        }
        const c = vm.console();
        if (c.length) out = Buffer.concat([out, Buffer.from(c)]);
        pump();

        // A finished boot needs no further budget, and waiting one out costs
        // minutes of interpreter time.
        if (out.includes(SHELL)) break;

        if (out.length === lastLen) {
            quietSteps += 2_000_000;
            if (quietSteps >= QUIET_LIMIT) {
                process.stdout.write(`  ${label}: no console output for ${QUIET_LIMIT / 1e6}M instructions — stopping\n`);
                break;
            }
        } else {
            lastLen = out.length;
            quietSteps = 0;
        }
    }
    const secs = Number(process.hrtime.bigint() - t0) / 1e9;
    const text = out.toString('latin1');
    process.stdout.write(`  ${label}: ${steps / 1e6}M instructions, ${out.length} B console, ${secs.toFixed(1)}s\n`);
    return { text, steps, hung: quietSteps >= QUIET_LIMIT };
}

/// Did the boot get far enough to be a boot? The rescue shell is where the
/// snapshot is taken from, so it is the honest finish line.
const SHELL = 'Launching initramfs emergency recovery shell';
const reachedShell = t => t.includes(SHELL);

/// printk stamps every line with the guest clock, and the JIT credits emulated
/// time in batches where the interpreter credits it per instruction — so the
/// low digits differ by a tick or two on a boot that is otherwise identical.
/// That is a real difference in timekeeping granularity and not a
/// miscompilation, so it must not be allowed to mask one: normalise the stamps
/// and compare everything else exactly.
const strip = t => t.replace(/\[\s*\d+\.\d+\]/g, '[TS]');

console.log(`cold boot, ${RAM_MB} MiB, budget ${BUDGET / 1e6}M`);
console.log('interpreter...');
const a = coldBoot(false, BUDGET, 'interp');
console.log('jit...');
const b = coldBoot(true, BUDGET, 'jit');

console.log();
console.log(`interpreter reached the shell: ${reachedShell(a.text)}`);
console.log(`jit         reached the shell: ${reachedShell(b.text)}`);

/// Compare up to the shell marker and no further. Both runs stop as soon as it
/// appears, and whichever got there in a cheaper slice has a little more
/// console already drained — trailing output past the finish line is a
/// difference in when the loop broke, not in what the guest computed.
const upToShell = t => {
    const i = t.indexOf(SHELL);
    return i === -1 ? t : t.slice(0, i + SHELL.length);
};

const A = upToShell(strip(a.text)), B = upToShell(strip(b.text));
if (B === A.slice(0, B.length) && B.length < A.length) {
    // A clean prefix: the JIT did not compute anything wrong that reached the
    // console, it simply stopped. The last line it managed is the neighbourhood
    // of the bug.
    const shown = A.slice(Math.max(0, B.length - 260), B.length + 260);
    console.log(`\nJIT output is a clean PREFIX of the interpreter's, ` +
        `${B.length} of ${A.length} B (timestamps normalised).`);
    console.log('---');
    console.log(shown.slice(0, 260) + '\n>>> JIT STOPPED HERE <<<\n' + shown.slice(260));
    console.log('---');
    process.exit(1);
}

let i = 0;
while (i < Math.min(A.length, B.length) && A[i] === B[i]) i++;
if (i === A.length && i === B.length) {
    console.log('\nOK: identical console output');
    process.exit(reachedShell(a.text) ? 0 : 1);
}
console.log(`\nDIVERGED at byte ${i} (timestamps normalised, so this is real):`);
console.log(`  interp: ${JSON.stringify(A.slice(Math.max(0, i - 120), i + 120))}`);
console.log(`  jit:    ${JSON.stringify(B.slice(Math.max(0, i - 120), i + 120))}`);
process.exit(1);
