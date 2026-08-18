// Reproduce the browser's JIT + 9p cold-boot hang in node, where it can be
// iterated on in seconds and instrumented. Mirrors the worker: net + disk (with
// the 9p modules under /mod) + a 9p device, boot to the shell, then run the
// same insmod + mount the setup script does.
//
//   node jit-9p-repro.js            # JIT on (reproduces the browser)
//   NOJIT=1 node jit-9p-repro.js    # interpreter (should work, like native)

const fs = require('fs');
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';
const USE_JIT = !process.env.NOJIT;

const kernel = fs.readFileSync('kernels/vmlinuz-lts.raw');
const initrd = fs.readFileSync('kernels/boot/initramfs-lts');
const dtb = fs.readFileSync('kernels/boot.dtb');
const diskBytes = new Uint8Array(fs.readFileSync('/tmp/disk9p.img'));

const mod = require(SHIM);
const exp = mod.__wasm;

const vm = new mod.Vm(kernel, initrd, dtb, 512);

// Disk backend over diskBytes.
const SECT = 512;
const readFn = (sector, count) => diskBytes.subarray(sector * SECT, (sector + count) * SECT);
const writeFn = (sector, bytes) => { diskBytes.set(bytes, sector * SECT); };
if (!vm.attach_disk(diskBytes.length / SECT, readFn, writeFn)) throw new Error('attach_disk');
if (!vm.attach_net(new Uint8Array([0x52, 0x54, 0, 0x12, 0x34, 0x56]))) throw new Error('attach_net');
if (!vm.attach_9p('shared')) throw new Error('attach_9p');
vm.p9_put('hello.txt', new TextEncoder().encode('hi from the host\n'));

const table = exp.__indirect_function_table;
function pump() {
    const count = vm.jit_pending();
    if (count === 0) return;
    const base = table.length;
    table.grow(count);
    const bytes = vm.jit_build(base);
    if (!bytes || bytes.length === 0) return;
    new WebAssembly.Instance(new WebAssembly.Module(bytes), {
        env: {
            mem: exp.memory, __indirect_function_table: table,
            load8u: exp.load8u, load16u: exp.load16u, load32u: exp.load32u, load64: exp.load64,
            store8: exp.store8, store16: exp.store16, store32: exp.store32, store64: exp.store64,
            csr: exp.csr, fp: exp.fp,
        },
    });
    vm.jit_installed(base);
}
if (USE_JIT) vm.jit_enable(true);

let out = '';
function drain() {
    const c = vm.console();
    if (c.length) out += Buffer.from(c).toString('latin1');
}
function runUntil(marker, budgetSteps, label) {
    const start = Number(vm.steps());
    let lastLen = out.length, quiet = 0;
    while (!out.includes(marker)) {
        vm.run(2_000_000);
        if (USE_JIT) pump();
        drain();
        const now = Number(vm.steps());
        if (out.length === lastLen) {
            quiet += 2_000_000;
            if (quiet > 1_500_000_000) {
                console.log(`[${label}] STUCK after ${((now - start) / 1e6) | 0}M steps, no output. tail:`);
                console.log(out.slice(-600));
                return false;
            }
        } else { lastLen = out.length; quiet = 0; }
        if (now - start > budgetSteps) {
            console.log(`[${label}] budget exhausted. tail:`);
            console.log(out.slice(-600));
            return false;
        }
    }
    return true;
}

console.log(`JIT=${USE_JIT}`);
if (!runUntil('recovery shell', 5_000_000_000, 'boot')) process.exit(1);
console.log(`[boot] reached shell at ${(Number(vm.steps()) / 1e6) | 0}M steps`);
// Let the initramfs coldplug finish creating /dev/vda before typing, exactly
// as the native harness and the browser (prompt-wait) do.
for (let i = 0; i < 60; i++) { vm.run(2_000_000); if (USE_JIT) pump(); drain(); }

// Drive one command at a time, waiting for the shell's NEXT prompt to know the
// command finished — robust against the echo containing marker text. A command
// that never returns a prompt within the budget is the hang, and it is named.
const PROMPT = "~ # ";
function promptCount() {
    let n = 0, i = 0;
    while ((i = out.indexOf(PROMPT, i)) !== -1) { n++; i += PROMPT.length; }
    return n;
}
function runCmd(cmd, hangBudget) {
    const want = promptCount() + 1;
    vm.input(new TextEncoder().encode(cmd + "\n"));
    const t0 = Number(vm.steps());
    while (Number(vm.steps()) - t0 < hangBudget) {
        vm.run(2_000_000);
        if (USE_JIT) pump();
        drain();
        if (promptCount() >= want) return true;
    }
    return false; // never came back to a prompt -> hung
}

const steps = [
    "mount -t ext4 /dev/vda /mnt/disk 2>&1",
    "ls -la /mnt/disk/mod 2>&1",
    "insmod /mnt/disk/mod/netfs.ko 2>&1",
    "insmod /mnt/disk/mod/9pnet.ko 2>&1",
    "insmod /mnt/disk/mod/9pnet_virtio.ko 2>&1",
    "insmod /mnt/disk/mod/9p.ko 2>&1",
    "mkdir -p /mnt/shared; mount -t 9p -o trans=virtio,version=9p2000.L,msize=131072 shared /mnt/shared 2>&1",
    "cat /mnt/shared/hello.txt 2>&1",
];
let hung = null;
for (const cmd of steps) {
    const mark = out.length;
    if (!runCmd(cmd, 2_500_000_000)) { hung = cmd; break; }
    process.stdout.write(`[ok] ${cmd.slice(0, 40)} -> ${out.slice(mark).replace(/\s+/g, " ").trim().slice(0, 90)}\n`);
}
if (hung) {
    console.log(`\n>>> HUNG (no prompt within budget) on: ${hung}`);
    console.log(out.slice(-500));
} else {
    console.log('\ncat ok:', out.includes('hi from the host'));
}
