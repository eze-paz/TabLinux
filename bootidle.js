// Where does a cold boot's idle time actually go?
//
// Holding the guest clock to real time takes a cold boot from 68s to 118s, so
// the boot spends tens of seconds of GUEST time waiting. Before shortening any
// device latency, find out what shape the waiting is: thousands of short waits
// point at device completion latency, a handful of long ones point at a driver
// timeout, and those want opposite fixes.
//
// Counters only, so no A/B discipline is needed. Run with the clock OFF, which
// is faster: bounding changes how long a wait takes in REAL time, not how long
// the guest ASKED to wait, and it is the asking that is being measured.

const fs = require('fs');
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';
const mod = require(SHIM);

const kernel = fs.readFileSync('kernels/vmlinuz-lts.raw');
const initrd = fs.readFileSync('kernels/boot/initramfs-lts');
const dtb = fs.readFileSync('kernels/boot.dtb');
const RAM_MB = Number(process.env.MB || 1024);

const vm = new mod.Vm(kernel, initrd, dtb, RAM_MB);

// ATTACH THE DISK. Without one the guest finds no root device, times out
// waiting for it, and drops into the emergency shell -- a boot with no virtio
// traffic at all, on which device latency is unmeasurable by construction.
// Measuring that and concluding device latency does not matter would be
// measuring the absence of a device. virtio-mmio has no hotplug, so this must
// happen before the guest probes.
const SECT = 512;
const diskBytes = new Uint8Array(fs.readFileSync('kernels/disk-ext4.img'));
vm.attach_disk(diskBytes.length / SECT,
    (s, c) => diskBytes.subarray(s * SECT, (s + c) * SECT),
    (s, b) => { diskBytes.set(b, s * SECT); });
vm.jit_enable(true);

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

const SHELL = process.env.MARKER || 'Launching initramfs emergency recovery shell';
let out = '';
let steps = 0;
const t0 = process.hrtime.bigint();
while (steps < 3_000_000_000) {
    steps += vm.run(2_000_000);
    const c = vm.console();
    if (c.length) out += Buffer.from(c).toString('latin1');
    pump();
    if (out.includes(SHELL)) break;
}
const wall = Number(process.hrtime.bigint() - t0) / 1e9;

const NAMES = ['< 10us', '10-100us', '100us-1ms', '1-5ms', '5-15ms', '15-100ms', '> 100ms'];
const v = Array.from(vm.idle_waits());
const idleSec = v.pop();
const total = v.reduce((a, c) => a + c, 0) || 1;
const guestSec = Number(vm.mtime()) / 1e7;

console.log(`reached shell: ${out.includes(SHELL)}`);
console.log(`boot: ${(steps / 1e6).toFixed(0)}M instructions, ${wall.toFixed(1)}s host`);
console.log(`guest clock advanced ${guestSec.toFixed(2)}s, of which ${idleSec.toFixed(2)}s was idle` +
    ` (${(100 * idleSec / guestSec).toFixed(0)}%)`);
console.log(`${(total / 1e3).toFixed(1)}k idle waits, mean ${(1e6 * idleSec / total).toFixed(0)}us\n`);
console.log('how long the guest asked to wait, each time it idled:');
v.forEach((n, i) => {
    if (!n) return;
    console.log(`  ${NAMES[i].padEnd(12)} ${(n / 1e3).toFixed(1).padStart(8)}k  ${(100 * n / total).toFixed(1).padStart(5)}%`);
});
// Where the waiting actually happens, from the kernel's own printk timestamps.
// They are guest seconds, so a gap between consecutive lines IS the guest
// sitting there, and the line before the gap names what it was waiting for.
// No instrumentation needed -- the guest has been reporting this all along.
const lines = out.split('\n');
const stamped = [];
for (const l of lines) {
    const m = l.match(/^\[\s*(\d+\.\d+)\]/);
    if (m) stamped.push({ t: Number(m[1]), text: l.trim() });
}
const gaps = [];
for (let i = 1; i < stamped.length; i++) {
    const d = stamped[i].t - stamped[i - 1].t;
    if (d > 0.05) gaps.push({ d, before: stamped[i - 1].text, after: stamped[i].text });
}
gaps.sort((a, b) => b.d - a.d);
const gapTotal = gaps.reduce((a, c) => a + c.d, 0);
console.log(`\nprintk gaps over 50ms: ${gaps.length}, totalling ${gapTotal.toFixed(2)}s of guest time`);
for (const g of gaps.slice(0, 12)) {
    console.log(`\n  ${g.d.toFixed(2)}s after:`);
    console.log(`    ${g.before.slice(0, 110)}`);
    console.log(`    -> ${g.after.slice(0, 110)}`);
}
