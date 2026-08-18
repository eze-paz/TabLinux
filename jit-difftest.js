// Run each compiled block under V8 and compare its end state against the
// interpreter's. See crates/riscv-jit/examples/difftest.rs, which generates
// the cases.
//
// Node runs the same V8 that Chrome does, so a block correct here is correct in
// the browser -- the only place the JIT will actually run.
//
// Correctness is checked through BOTH import implementations. The wasm host is
// what production uses; the JS one is what a harness reaches for by default.
// Running both means a bug in either is caught here rather than showing up as
// a guest kernel panic thousands of instructions downstream.

const fs = require('fs');
const E = require('./jitenv.js');

const cases = JSON.parse(fs.readFileSync('/tmp/jit_cases.json', 'utf8'));

function runAll(env, label) {
    let failures = 0;
    for (const c of cases) {
        const mod = new WebAssembly.Module(fs.readFileSync(`/tmp/jit_case_${c.case}.wasm`));
        const run = new WebAssembly.Instance(mod, { env }).exports.run;

        E.setRegs(c.init, c.initf);
        E.fillMem(c.case);
        run(E.REGS_BASE, E.BLOCK_PC);

        const err = E.checkAgainst(c);
        if (err) {
            if (failures < 10) console.log(`[${label}] case ${c.case} ${err}`);
            failures++;
        }
    }
    if (failures) {
        console.log(`[${label}] FAIL: ${failures}/${cases.length} disagree with the interpreter`);
    } else {
        console.log(`[${label}] OK: ${cases.length} cases match (registers + memory)`);
    }
    return failures;
}

const bad = runAll(E.wasmEnv(), 'wasm imports') + runAll(E.jsEnv(), 'JS imports');
process.exit(bad ? 1 : 0);
