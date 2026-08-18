// Restore the snapshot under the JIT (the path jit-vm-test.js proves
// responsive), then type the boot setup one step at a time and, after each,
// probe whether the guest still answers. The step after which the probe stops
// echoing is the one that wedges it.
//
//   node jit-restore-repro.js

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
// The snapshot already carries net + 9p (make_snapshot attaches them); restore
// recreated them. Seed the share so a later cat has something.
vm.p9_put('host.txt', new TextEncoder().encode('HELLO_FROM_OPFS\n'));

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
const JIT = process.env.NOJIT ? false : true;
vm.jit_enable(JIT);
console.log('JIT:', JIT);

let out = '';
function drain() { const c = vm.console(); if (c.length) out += Buffer.from(c).toString('latin1'); }
function runFor(steps) { const t = Number(vm.steps()); while (Number(vm.steps()) - t < steps) { vm.run(2_000_000); pump(); drain(); } }

// Answer ESC[6n like the browser terminal does, so a line editor that queries
// the cursor is not left waiting — the guest is otherwise headless here.
function answer6n() {
    let i;
    while ((i = out.indexOf('\x1b[6n')) !== -1) {
        out = out.slice(0, i) + out.slice(i + 4);
        vm.input(new TextEncoder().encode('\x1b[1;1R'));
    }
}

// Is the guest still processing input? Type a unique echo and see if it comes
// back within a step budget.
let probe = 0;
function responsive() {
    probe++;
    const tok = `PRB${probe}X`;
    vm.input(new TextEncoder().encode(`echo ${tok}\n`));
    const t = Number(vm.steps());
    let last = t;
    while (Number(vm.steps()) - t < 3_000_000_000) {
        vm.run(2_000_000); pump(); drain(); answer6n();
        // The echo of the command contains the token too; require it TWICE
        // (once echoed, once as output) so a mere echo does not count.
        if ((out.split(tok).length - 1) >= 2) return true;
        const s = Number(vm.steps());
        if (s - last >= 1_000_000_000) {
            last = s;
            const d = vm.diag();
            console.log(`  ...${((s - t) / 1e9).toFixed(1)}e9 pc=0x${d[0].toString(16)} priv=${d[1]} mip=0x${d[2].toString(16)} mie=0x${d[3].toString(16)} sie=${d[4]} ext=${d[5]} vpend=${d[6]} mtime=${d[7]} stimecmp=${d[8]}`);
        }
    }
    return false;
}

// Settle the restored shell, then confirm it answers before we change anything.
runFor(40_000_000);
console.log('baseline responsive:', responsive());

const steps = [
    ["disk",    "mkdir -p /mnt/disk; mount -t ext4 /dev/vda /mnt/disk 2>&1"],
    ["9p",      "mkdir -p /files; insmod /mnt/disk/mod/netfs.ko; insmod /mnt/disk/mod/9pnet.ko; insmod /mnt/disk/mod/9pnet_virtio.ko; insmod /mnt/disk/mod/9p.ko; mount -t 9p -o trans=virtio,version=9p2000.L,msize=131072 shared /files 2>&1"],
    ["overlay", "modprobe overlay 2>/dev/null; for d in etc usr lib bin sbin var; do mkdir -p /mnt/disk/ovl/$d/u /mnt/disk/ovl/$d/w; mount -t overlay ovl-$d -o lowerdir=/$d,upperdir=/mnt/disk/ovl/$d/u,workdir=/mnt/disk/ovl/$d/w /$d 2>/dev/null; done; echo OVL_DONE"],
    ["stty",    "stty -F /dev/ttyS0 rows 38 cols 129 2>/dev/null; echo STTY_DONE"],
    ["setsid",  "setsid sh -c 'exec sh </dev/ttyS0 >/dev/ttyS0 2>&1'"],
];

let allOk = true;
for (const [name, cmd] of steps) {
    vm.input(new TextEncoder().encode(cmd + "\n"));
    runFor(200_000_000);
    answer6n();
    const ok = responsive();
    if (!ok) allOk = false;
    console.log(`after ${name.padEnd(8)} responsive: ${ok}`);
    if (!ok) {
        console.log(`\n>>> WEDGED after step: ${name}`);
        console.log('tail:', JSON.stringify(out.slice(-300)));
        const d = vm.diag();
        const names = ['pc','priv','mip','mie','sie','ext_pend','vpend','mtime','stimecmp'];
        console.log('diag:', names.map((n, i) => `${n}=${n==='pc'?'0x'+d[i].toString(16):(n==='mip'||n==='mie')?'0x'+d[i].toString(16):d[i]}`).join(' '));
        // Dense PC sampling: run in tiny slices and record the chain-boundary PC
        // each time. Tells us how tight the spin loop is.
        const hist = new Map();
        for (let i = 0; i < 4000; i++) {
            vm.run(20_000); pump(); drain(); answer6n();
            const pc = vm.diag()[0];
            hist.set(pc, (hist.get(pc) || 0) + 1);
        }
        const top = [...hist.entries()].sort((a, b) => b[1] - a[1]).slice(0, 20);
        console.log(`sampled ${hist.size} distinct PCs; top:`);
        for (const [pc, c] of top) console.log(`   0x${pc.toString(16)}  ${c}`);
        // Also dump the interpreter fetch ring in case any bail happened.
        const tr = vm.pc_trace();
        const ih = new Map();
        for (let i = 0; i < tr.length; i += 2) { const pc = tr[i]; if (pc) ih.set(pc, (ih.get(pc) || 0) + 1); }
        const itop = [...ih.entries()].sort((a, b) => b[1] - a[1]).slice(0, 8);
        console.log('interp ring top:', itop.map(([pc, c]) => `0x${pc.toString(16)}:${c}`).join(' ') || '(empty)');
        break;
    }
}

if (allOk) {
    // Confirm the share is not just mounted but populated at /files, and that a
    // guest write round-trips back to the host tree via p9_list. One command
    // per line, like the step probes — a compound line trips the line editor.
    const type = (line, waitFor) => {
        out = '';
        vm.input(new TextEncoder().encode(line + "\n"));
        const t = Number(vm.steps());
        while (Number(vm.steps()) - t < 800_000_000) {
            vm.run(2_000_000); pump(); drain(); answer6n();
            if (out.includes(waitFor)) break;
        }
        return out.replace(/\r/g, '');
    };
    console.log('--- ls /files ---');
    console.log(type("ls -la /files", "\n"));
    console.log('--- cat /files/host.txt ---');
    console.log(type("cat /files/host.txt", "OPFS"));
    type("echo from-the-guest > /files/reply.txt", "reply");
    // Host-side readback of the guest's write.
    const flat = vm.p9_list();
    const names = [];
    let o = 4, count = new DataView(flat.buffer, flat.byteOffset, 4).getUint32(0, true);
    for (let i = 0; i < count; i++) {
        const dv = new DataView(flat.buffer, flat.byteOffset);
        const plen = dv.getUint32(o, true); o += 4;
        const path = new TextDecoder().decode(flat.subarray(o, o + plen)); o += plen;
        const dlen = dv.getUint32(o, true); o += 4; o += dlen;
        names.push(path);
    }
    console.log('host p9_list sees:', names.join(', '));
    console.log(names.includes('reply.txt') ? 'ROUND-TRIP OK: guest write visible to host' : 'ROUND-TRIP MISSING reply.txt');
}
