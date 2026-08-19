// Cost of a guest memory access in generated code: imported host call vs a
// direct load from linear memory.
//
// This decides how much work the JIT needs. Loads and stores are mandatory --
// coverage says 1.48x without them and 3.43x with -- and the cheap way to emit
// one is to call back into the host. The expensive way is to inline a software
// TLB probe and only call out on a miss, which is what QEMU does and needs the
// TLB laid out in linear memory, kept in sync, and a precise guest PC to unwind
// to on a fault.
//
// If an import call is a couple of nanoseconds, the cheap way is fine and the
// TLB work is unnecessary. If it is tens, it is not.
//
// The imported function here does nothing. That is deliberate: it isolates the
// call overhead from the translation work, which both designs pay.

const fs = require('fs');

const memory = new WebAssembly.Memory({ initial: 2 });
let sink = 0n;

function build(path, imports) {
    const mod = new WebAssembly.Module(fs.readFileSync(path));
    return new WebAssembly.Instance(mod, imports).exports.run;
}

// wasm -> JS -> back. Measured first, and it is the wrong model for the real
// system; kept so the two can be compared.
const viaJs = build('/tmp/imp_call.wasm', {
    env: { mem: memory, load: (addr) => addr },
});

// wasm -> wasm, which is what production does: the slow path is a function
// exported by the emulator's own module.
const hostMod = new WebAssembly.Module(fs.readFileSync('/tmp/imp_host.wasm'));
const hostLoad = new WebAssembly.Instance(hostMod, { env: { mem: memory } }).exports.load;
const viaWasm = build('/tmp/imp_call.wasm', { env: { mem: memory, load: hostLoad } });
const inlineLoads = build('/tmp/imp_inline.wasm', { env: { mem: memory } });

function bench(label, run, accesses, iters) {
    for (let i = 0; i < 200000; i++) run(0);
    let best = Infinity;
    for (let pass = 0; pass < 5; pass++) {
        const t0 = process.hrtime.bigint();
        for (let i = 0; i < iters; i++) run(0);
        const ns = Number(process.hrtime.bigint() - t0);
        best = Math.min(best, ns / (iters * accesses));
    }
    console.log(`${label}: ${best.toFixed(2)} ns per access`);
    return best;
}

const js = bench('import -> JS      ', viaJs, 12, 1_000_000);
const ws = bench('import -> wasm    ', viaWasm, 12, 1_000_000);
const inl = bench('inline linear load', inlineLoads, 12, 1_000_000);

console.log(`\nwasm->wasm call overhead: ${(ws - inl).toFixed(2)} ns per access`);
console.log(`wasm->JS   call overhead: ${(js - inl).toFixed(2)} ns per access`);
console.log(`interpreter reference: ~42 ns per instruction`);
console.log(`\nverdict: a wasm->wasm slow-path call at ${ws.toFixed(1)} ns is ` +
            (ws < 8 ? 'cheap enough -- emit every access as a call and skip the inlined TLB'
                    : 'too slow -- inline the TLB probe and call out only on a miss'));
