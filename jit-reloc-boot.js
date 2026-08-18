// Boot the guest under Node/V8 with the JIT ON, using the relocatable codegen
// (table_base is now an imported global, not a baked const). Proves the blocks
// still install + execute correctly end-to-end. Same engine the browser uses.
const fs = require('fs');
const mod = require('/home/aezequiel/jitnode/riscv_wasm.js');
const root = '/home/aezequiel/rv9p-wt/kernels';
const kernel = fs.readFileSync(root + '/vmlinuz-lts.raw');
const initrd = fs.readFileSync(root + '/boot/initramfs-lts');
const dtb = fs.readFileSync(root + '/boot.dtb');

const vm = new mod.Vm(kernel, initrd, dtb, 256);
vm.attach_net(new Uint8Array([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]));
vm.jit_enable(true);

const w = mod.__wasm;
const table = w.__indirect_function_table;
function pump() {
    const n = vm.jit_pending();
    if (!n) return;
    const base = table.length;
    table.grow(n);
    const bytes = vm.jit_build(base);
    if (!bytes || !bytes.length) return;
    new WebAssembly.Instance(new WebAssembly.Module(bytes), {
        env: {
            mem: w.memory,
            __indirect_function_table: table,
            table_base: new WebAssembly.Global({ value: 'i32', mutable: false }, base),
            load8u: w.load8u, load16u: w.load16u, load32u: w.load32u, load64: w.load64,
            store8: w.store8, store16: w.store16, store32: w.store32, store64: w.store64,
            csr: w.csr, fp: w.fp,
        },
    });
    vm.jit_installed(base);
}

let out = '';
const drain = () => { const c = vm.console(); if (c.length) out += Buffer.from(c).toString('latin1'); };
function runUntil(marker, capMs) {
    const t = Date.now();
    while (!out.includes(marker) && Date.now() - t < capMs) { vm.run(2_000_000); drain(); pump(); }
    return out.includes(marker);
}

const booted = runUntil('~ # ', 180_000);
console.log(booted ? 'BOOTED TO SHELL (relocatable JIT installs + runs)' : 'NO SHELL');
if (booted) {
    // A computation that exercises freshly-compiled blocks.
    vm.input(new TextEncoder().encode("echo mul=$((7*7)); echo done\n"));
    const ok = runUntil('mul=49', 20_000) && runUntil('done', 5_000);
    console.log(ok ? 'CMD OK: 7*7=49 via JIT-compiled blocks' : 'CMD FAIL');
    console.log('jit blocks installed:', vm.jit_installed_count());
}
