// Shared setup for the JIT harnesses: the linear memory every module shares,
// and the load/store imports generated blocks call.
//
// Two implementations of the same contract, because which one you measure
// against changes the answer by 5x. `wasmEnv()` routes accesses to a wasm host
// module, which is what production does -- the emulator itself is wasm and
// exports these. `jsEnv()` routes them to JS closures, which is convenient and
// costs ~32 ns per call instead of ~6 ns. Benchmarks must use the wasm one;
// measuring against JS once already pointed this design at an inlined software
// TLB it does not need.

const fs = require('fs');

const REGS_BASE = 4096;
// The f-register file and FS word, at the addresses difftest.rs bakes into
// the blocks. FS is set to 1 (Dirty) so the inline FP paths are the ones
// under test; the host-call fallback is exercised end-to-end by the guest
// boot oracles instead.
const FREGS_BASE = 8192;
const FS_WORD = 8448;
const MEM_BASE = 16384;
const MEM_BYTES = 4096;
const BLOCK_PC = 0x80001234n;

const memory = new WebAssembly.Memory({ initial: 4 });
const view = new DataView(memory.buffer);
const bytes = new Uint8Array(memory.buffer);

function map(addr, sizeLog2) {
    const a = Number(BigInt.asUintN(64, addr) & BigInt(MEM_BYTES - 1));
    return MEM_BASE + (a & ~((1 << sizeLog2) - 1));
}

// JS implementations. Correct, and deliberately not what the benchmarks use.
function jsEnv() {
    const ld = (sz) => (addr) => {
        const o = map(addr, sz);
        switch (sz) {
            case 0: return BigInt(view.getUint8(o));
            case 1: return BigInt(view.getUint16(o, true));
            case 2: return BigInt(view.getUint32(o, true));
            default: return view.getBigUint64(o, true);
        }
    };
    const st = (sz) => (addr, val) => {
        const o = map(addr, sz);
        switch (sz) {
            case 0: view.setUint8(o, Number(BigInt.asUintN(8, val))); break;
            case 1: view.setUint16(o, Number(BigInt.asUintN(16, val)), true); break;
            case 2: view.setUint32(o, Number(BigInt.asUintN(32, val)), true); break;
            default: view.setBigUint64(o, BigInt.asUintN(64, val), true); break;
        }
    };
    return {
        mem: memory,
        load8u: ld(0), load16u: ld(1), load32u: ld(2), load64: ld(3),
        store8: st(0), store16: st(1), store32: st(2), store64: st(3),
        // Every generated module now imports csr, but the difftest generator
        // emits no CSR ops — modelling CSR state here would be a lot of harness
        // for no coverage, so this is a stub that never runs. The boot tests,
        // against a kernel that hammers CSRs, are the real oracle for it.
        csr: () => {},
        fp: () => {},
    };
}

// The wasm host module, standing in for the emulator's own exports.
function wasmEnv() {
    const mod = new WebAssembly.Module(fs.readFileSync('/tmp/jit_host.wasm'));
    const h = new WebAssembly.Instance(mod, { env: { mem: memory } }).exports;
    return {
        mem: memory,
        load8u: h.load8u, load16u: h.load16u, load32u: h.load32u, load64: h.load64,
        store8: h.store8, store16: h.store16, store32: h.store32, store64: h.store64,
        // Stub, same reasoning as jsEnv: no generated case exercises it.
        csr: () => {},
        fp: () => {},
    };
}

function fillMem(caseNo) {
    for (let i = 0; i < MEM_BYTES; i++) {
        bytes[MEM_BASE + i] = (i * 31 + caseNo * 17) & 0xFF;
    }
}

// FNV-1a over the scratch region, matching the Rust side. The prime is 11 hex
// digits: writing it as 0x1000_0000_01b3 slips in a twelfth and silently makes
// every case disagree, which cost a debugging round.
const FNV_PRIME = 0x100000001b3n;
const MASK64 = (1n << 64n) - 1n;
function checksum() {
    let h = 0xcbf29ce484222325n;
    for (let i = 0; i < MEM_BYTES; i++) {
        h ^= BigInt(bytes[MEM_BASE + i]);
        h = (h * FNV_PRIME) & MASK64;
    }
    return h;
}

function setRegs(init, initf) {
    for (let r = 0; r < 32; r++) {
        view.setBigUint64(REGS_BASE + r * 8, BigInt(init[r]), true);
        view.setBigUint64(FREGS_BASE + r * 8, BigInt(initf ? initf[r] : 0), true);
    }
    view.setUint32(FS_WORD, 1, true);
}

/// Returns a description of the first mismatch, or null.
function checkAgainst(c) {
    for (let r = 0; r < 32; r++) {
        const got = view.getBigUint64(REGS_BASE + r * 8, true);
        if (got !== BigInt(c.expect[r])) return `x${r}: got ${got}, want ${c.expect[r]}`;
    }
    if (c.expectf) {
        for (let r = 0; r < 32; r++) {
            const got = view.getBigUint64(FREGS_BASE + r * 8, true);
            if (got !== BigInt(c.expectf[r])) return `f${r}: got ${got}, want ${c.expectf[r]}`;
        }
    }
    const got = checksum();
    if (got !== BigInt(c.memsum)) return `memory: got ${got}, want ${c.memsum}`;
    return null;
}

module.exports = {
    REGS_BASE, MEM_BASE, MEM_BYTES, BLOCK_PC,
    memory, view, bytes,
    jsEnv, wasmEnv, fillMem, checksum, setRegs, checkAgainst,
};
