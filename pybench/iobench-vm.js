// Storage-path microbenchmarks inside the emulated guest, host-timed.
//
// Two very different paths hide behind "disk": virtio-blk backed by the
// attached image (the guest's ext4 /dev/vda), and virtio-9p (the /files
// share, served host-side; in the browser a fetch-hydration layer sits on
// top, which this harness deliberately excludes — it measures the transport
// and protocol, the part the engine owns).
//
// Wall time comes from host hrtime between console markers, and each phase
// also reports retired instructions, so a slow phase is attributable: high
// MB/s + low insns/KB = healthy; low MB/s + high insns/KB = the guest is
// burning CPU in the protocol; low MB/s + low insns/KB = stalls.
//
//   node pybench/iobench-vm.js [JITNODE=/tmp/jitnode]

const fs = require('fs');
const path = require('path');

const ROOT = path.join(__dirname, '..');
const SHIM = (process.env.JITNODE || '/tmp/jitnode') + '/riscv_wasm.js';

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
                mem: exp.memory, __indirect_function_table: table,
                table_base: new WebAssembly.Global({ value: 'i32', mutable: false }, base),
                load8u: exp.load8u, load16u: exp.load16u,
                load32u: exp.load32u, load64: exp.load64,
                store8: exp.store8, store16: exp.store16,
                store32: exp.store32, store64: exp.store64,
                csr: exp.csr, fp: exp.fp,
            },
        });
        vm.jit_installed(base);
        return count;
    };
}

const mod = fresh();
const vm = mod.Vm.restore(snapshot);
if (!vm) throw new Error('restore failed');

vm.attach_disk(
    disk.length / 512,
    (sector, count) => new Uint8Array(
        disk.buffer, disk.byteOffset + Number(sector) * 512, Number(count) * 512),
    (sector, bytes) => Buffer.from(bytes).copy(disk, Number(sector) * 512),
);

// Seed the 9p share host-side: one 32 MiB file and 400 small ones. The device
// restores in seeded mode in this harness (the browser's lazy flip is never
// called), so guest reads are served straight from this tree.
vm.p9_put('big.bin', new Uint8Array(32 * 1024 * 1024).fill(7));
vm.p9_mkdir('many');
const small = new Uint8Array(2048).fill(3);
for (let i = 0; i < 400; i++) vm.p9_put(`many/f${i}`, small);

vm.jit_enable(true);
const pump = makeCompiler(vm, mod.__wasm);

// Each phase: marker, command, marker. umount/mount between write and read
// drops the guest page cache, so the read phases measure the device path
// rather than guest memcpy.
const script = [
    'mkdir -p /mnt/disk /files',
    'mount -t ext4 /dev/vda /mnt/disk',
    'mount -t 9p -o trans=virtio,version=9p2000.L,msize=131072 shared /files',
    'echo IOB:blkwrite:48',
    'dd if=/dev/zero of=/mnt/disk/w.bin bs=1M count=48 conv=fsync 2>/dev/null',
    'umount /mnt/disk && mount -t ext4 /dev/vda /mnt/disk',
    'echo IOB:blkread:48',
    'dd if=/mnt/disk/w.bin of=/dev/null bs=1M 2>/dev/null',
    'i=0; while [ $i -lt 400 ]; do echo sfx > /mnt/disk/s$i; i=$((i+1)); done; sync',
    'umount /mnt/disk && mount -t ext4 /dev/vda /mnt/disk',
    // Per-file OPEN cost, forklessly: `read x <file` is a shell builtin, so
    // each iteration is open+read+close with no process spawn. (The first
    // version of this harness used `cat` per file and measured fork+exec:
    // ~1.2M instructions per spawn, identical on both mounts.)
    'echo IOB:blkopen:400',
    'i=0; while [ $i -lt 400 ]; do read x < /mnt/disk/s$i; i=$((i+1)); done',
    'echo IOB:p9read:32',
    'dd if=/files/big.bin of=/dev/null bs=1M 2>/dev/null',
    'echo IOB:p9open:400',
    'i=0; while [ $i -lt 400 ]; do read x < /files/many/f$i; i=$((i+1)); done',
    // Same files again: does the default (uncached) 9p mount pay the full
    // round-trip on every re-open?
    'echo IOB:p9open2:400',
    'i=0; while [ $i -lt 400 ]; do read x < /files/many/f$i; i=$((i+1)); done',
    // And with client-side caching, the mode the real page could adopt.
    'umount /files; mount -t 9p -o trans=virtio,version=9p2000.L,msize=131072,cache=loose shared /files',
    'echo IOB:p9loose:400',
    'i=0; while [ $i -lt 400 ]; do read x < /files/many/f$i; i=$((i+1)); done',
    'echo IOB:p9loose2:400',
    'i=0; while [ $i -lt 400 ]; do read x < /files/many/f$i; i=$((i+1)); done',
    'echo IOB:p9stat:400',
    'ls -la /files/many > /dev/null',
    'echo IOB:done:0',
].join('\n') + '\n';

vm.input(Buffer.from(script));

let out = '', seen = 0;
let cur = null; // { name, unit, t, s }
const deadline = Date.now() + 20 * 60 * 1000;
while (true) {
    vm.run(2_000_000);
    pump();
    const c = vm.console();
    if (c.length) out += Buffer.from(c).toString('latin1');
    let idx;
    while ((idx = out.indexOf('IOB:', seen)) !== -1) {
        const eol = out.indexOf('\n', idx);
        if (eol === -1) break;
        const [, name, unit] = out.slice(idx, eol).trim().split(':');
        seen = eol + 1;
        const now = process.hrtime.bigint();
        const steps = BigInt(vm.steps());
        if (cur) {
            const ms = Number(now - cur.t) / 1e6;
            const insns = Number(steps - cur.s);
            const mb = cur.unit;
            let rate = '';
            if (cur.name.includes('small') || cur.name.includes('stat')) {
                rate = `${(ms * 1000 / mb).toFixed(0)} us/file`;
            } else {
                rate = `${(mb / (ms / 1000)).toFixed(1)} MB/s`;
            }
            console.log(`${cur.name.padEnd(10)} ${ms.toFixed(0).padStart(7)} ms  ${rate.padStart(12)}  ` +
                        `${(insns / 1e6).toFixed(0).padStart(6)}M insns  ` +
                        `${(insns / (mb * (cur.name.includes('small') || cur.name.includes('stat') ? 1 : 1024 * 1024))).toFixed(0)} insns/${cur.name.includes('small') || cur.name.includes('stat') ? 'file' : 'byte'}`);
        }
        if (name === 'done') process.exit(0);
        cur = { name, unit: Number(unit), t: now, s: steps };
    }
    if (Date.now() > deadline) {
        console.error('TIMEOUT\n' + out.slice(-2000));
        process.exit(1);
    }
}
