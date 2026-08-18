// One clean boot under Node/V8, dumping a JSON summary: a hash of the console
// (to prove the boot is deterministic) and the set of content-keys the JIT
// produced (murmur128 of each compiled batch's wasm bytes). Run it in two
// separate processes and compare: identical console + overlapping key-sets means
// a compiled block in session A is byte-identical in session B, so the
// content-addressed cache hits — and, being content-addressed, every hit is
// correct by construction.
//
//   node jit-cache-verify.mjs > /tmp/run1.json ; node jit-cache-verify.mjs > /tmp/run2.json
import { createRequire } from "module";
import fs from "fs";
import { murmur128 } from "./web/jit-cache.js";
const require = createRequire(import.meta.url);
const mod = require("/home/aezequiel/jitnode/riscv_wasm.js");

const root = "/home/aezequiel/rv9p-wt/kernels";
const kernel = fs.readFileSync(root + "/vmlinuz-lts.raw");
const initrd = fs.readFileSync(root + "/boot/initramfs-lts");
const dtb = fs.readFileSync(root + "/boot.dtb");
const w = mod.__wasm;
const table = w.__indirect_function_table;

const vm = new mod.Vm(kernel, initrd, dtb, 256);
vm.attach_net(new Uint8Array([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]));
vm.jit_enable(true);

const keys = [];
const pump = () => {
    const n = vm.jit_pending();
    if (!n) return;
    const base = table.length;
    table.grow(n);
    const bytes = vm.jit_build(base);
    if (!bytes || !bytes.length) return;
    keys.push(murmur128(bytes));
    new WebAssembly.Instance(new WebAssembly.Module(bytes), {
        env: {
            mem: w.memory, __indirect_function_table: table,
            table_base: new WebAssembly.Global({ value: "i32", mutable: false }, base),
            load8u: w.load8u, load16u: w.load16u, load32u: w.load32u, load64: w.load64,
            store8: w.store8, store16: w.store16, store32: w.store32, store64: w.store64,
            csr: w.csr, fp: w.fp,
        },
    });
    vm.jit_installed(base);
};

let out = "";
const drain = () => { const c = vm.console(); if (c.length) out += Buffer.from(c).toString("latin1"); };
const runUntil = (marker, cap) => {
    const t0 = Date.now();
    while (!out.includes(marker) && Date.now() - t0 < cap) { vm.run(2_000_000); drain(); pump(); }
    return out.includes(marker);
};
if (!runUntil("~ # ", 180_000)) { console.error("no shell"); process.exit(2); }
// Deterministic workload so the comparison covers more than the raw boot.
vm.input(new TextEncoder().encode("i=0; s=0; while [ $i -lt 400 ]; do s=$((s+i*3)); i=$((i+1)); done; echo SUM=$s; echo END\n"));
if (!runUntil("END", 60_000)) { console.error("workload hung"); process.exit(2); }

// Console hash (deterministic-boot check). Console up to END only, since timing
// tails can differ; the guest-visible bytes we compare are the meaningful part.
const upto = out.slice(0, out.indexOf("END") + 3);
let h = 0;
for (let i = 0; i < upto.length; i++) h = (Math.imul(h, 31) + upto.charCodeAt(i)) | 0;
process.stdout.write(JSON.stringify({ consoleHash: h >>> 0, consoleLen: upto.length, keyCount: keys.length, keys }) + "\n");
