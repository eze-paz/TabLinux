// Measure v86's instruction throughput on a CPU-bound guest workload, to gauge
// ours against the reference implementation of this technique.
//
// v86 JITs x86 to WebAssembly in a browser, which is exactly what this emulator
// now does for RISC-V, and it is mature. Same Node, same machine, same session.
//
// Two things this deliberately controls for:
//
// *Idle.* A first attempt measured the whole run and got a 0.3 MIPS median with
// a 246 MIPS peak. Most of that window was the guest waiting on boot I/O, where
// v86 executes almost nothing. Averaging idle into throughput measures the
// scheduler, not the JIT. So this waits for a shell, starts a busy workload, and
// samples only after that.
//
// *Workload shape.* The guest runs a shell loop over the filesystem plus a
// checksum -- the same shape as jit-vm-test.js runs on our side, rather than a
// tight synthetic loop that flatters whichever engine has the better inner-loop
// codegen.
//
// What it cannot control for is the ISA. An x86 instruction does more work on
// average than a RISC-V one -- variable length, memory operands folded into ALU
// ops, string instructions -- so equal MIPS does not mean equal speed, and v86
// is doing more per instruction. Order-of-magnitude check, not a race.
//
//   bash get-v86.sh && node v86-bench.mjs

import path from 'node:path';
import { V86 } from '/tmp/v86cmp/package/build/libv86.mjs';

const DIR = '/tmp/v86cmp';
const SAMPLE_MS = 60000;

const emulator = new V86({
    bios: { url: path.join(DIR, 'seabios.bin') },
    vga_bios: { url: path.join(DIR, 'vgabios.bin') },
    cdrom: { url: path.join(DIR, 'linux4.iso') },
    wasm_path: path.join(DIR, 'package/build/v86.wasm'),
    autostart: true,
    memory_size: 128 * 1024 * 1024,
    vga_memory_size: 2 * 1024 * 1024,
    disable_speaker: true,
});

let serial = '';
let started = false;

emulator.add_listener('serial0-output-byte', (b) => {
    serial += String.fromCharCode(b);
    if (!started && serial.includes('~%')) {
        started = true;
        setTimeout(begin, 500);
    }
});

function begin() {
    console.log('shell up; starting workload');
    // Same shape as our test: directory walk, pipe, checksum, repeated.
    emulator.serial0_send('while true; do ls -la /bin | md5sum; done\n');

    // Let it get going before sampling, so process startup is not counted.
    setTimeout(sample, 2000);
}

function sample() {
    const out = [];
    let last = emulator.get_instruction_counter();
    let lastT = process.hrtime.bigint();

    const iv = setInterval(() => {
        const c = emulator.get_instruction_counter();
        const now = process.hrtime.bigint();
        const dt = Number(now - lastT) / 1e9;
        // The counter is 32-bit and wraps; a negative delta is a wrap, not the
        // guest running backwards.
        if (dt > 0 && c > last) out.push((c - last) / dt / 1e6);
        last = c;
        lastT = now;
    }, 1000);

    setTimeout(() => {
        clearInterval(iv);
        emulator.stop();
        const clean = out.filter(Number.isFinite).sort((a, b) => a - b);
        const median = clean[Math.floor(clean.length / 2)] ?? 0;
        console.log(`\nv86 (x86 -> wasm JIT), CPU-bound guest workload`);
        console.log(`  per-second: ${clean.map(s => s.toFixed(0)).join(', ')}`);
        console.log(`  median ${median.toFixed(1)} MIPS   best ${(clean.at(-1) ?? 0).toFixed(1)} MIPS`);
        process.exit(0);
    }, SAMPLE_MS);
}

setTimeout(() => {
    if (!started) {
        console.log('never reached a shell; tail:', JSON.stringify(serial.slice(-200)));
        process.exit(1);
    }
}, 90000);
