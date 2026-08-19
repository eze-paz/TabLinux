//! Differential test: does a compiled block compute what the interpreter does?
//!
//! Generates pseudo-random straight-line programs from the compilable subset,
//! runs each through the interpreter, emits the wasm, and writes the modules
//! plus the expected end state to /tmp. `bench/jit-difftest.js` executes them under
//! Node — V8, the same engine the browser uses — and compares.
//!
//! This exists because the JIT's failure mode is silent. A miscompiled shift, a
//! missing sign-extension, or a store of the wrong width does not crash; it
//! corrupts guest state and surfaces as a kernel panic thousands of
//! instructions later, in code with nothing to do with the bug. Randomised
//! differential testing against the interpreter is the only cheap way to find
//! that class of error.
//!
//! Memory is a 4 KiB scratch region. Guest addresses are masked into it and
//! aligned, identically on both sides, so random register values produce valid
//! comparable accesses rather than faults. Expected memory is compared by
//! checksum rather than shipping 4 KiB per case.
//!
//!   cargo run --release -p riscv-jit --example difftest
//!   node bench/jit-difftest.js

use riscv_core::execute::{Bus, Cpu};
use riscv_core::types::Instr;

pub const MEM_BYTES: usize = 4096;

/// Deterministic initial memory, reproduced byte-for-byte on the JS side.
fn fill(case: usize) -> Vec<u8> {
    (0..MEM_BYTES)
        .map(|i| ((i * 31 + case * 17) & 0xFF) as u8)
        .collect()
}

/// FNV-1a, so both sides can agree that memory ended up the same without
/// shipping it.
fn checksum(m: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in m {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3); // FNV-1a prime: 11 hex digits, not 12
    }
    h
}

/// Masks and aligns every access into the scratch region. The JS import does
/// exactly the same arithmetic; if the two ever disagree the test fails, which
/// is the point.
fn map(addr: u64, size_log2: u32) -> usize {
    let a = (addr as usize) & (MEM_BYTES - 1);
    a & !((1usize << size_log2) - 1)
}

struct MemBus {
    mem: Vec<u8>,
}

impl Bus for MemBus {
    fn read_u8(&self, a: u64) -> u8 {
        self.mem[map(a, 0)]
    }
    fn read_u16(&self, a: u64) -> u16 {
        let o = map(a, 1);
        u16::from_le_bytes([self.mem[o], self.mem[o + 1]])
    }
    fn read_u32(&self, a: u64) -> u32 {
        let o = map(a, 2);
        u32::from_le_bytes(self.mem[o..o + 4].try_into().unwrap())
    }
    fn read_u64(&self, a: u64) -> u64 {
        let o = map(a, 3);
        u64::from_le_bytes(self.mem[o..o + 8].try_into().unwrap())
    }
    fn write_u8(&mut self, a: u64, v: u8) {
        self.mem[map(a, 0)] = v;
    }
    fn write_u16(&mut self, a: u64, v: u16) {
        let o = map(a, 1);
        self.mem[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn write_u32(&mut self, a: u64, v: u32) {
        let o = map(a, 2);
        self.mem[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn write_u64(&mut self, a: u64, v: u64) {
        let o = map(a, 3);
        self.mem[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }
}

/// xorshift64*, so cases are reproducible without a dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_instr(r: &mut Rng) -> Instr {
    use Instr::*;
    // A small register set so instructions feed each other's results; x0 is
    // included deliberately to exercise the hardwired-zero path both ways.
    let rd = r.below(6) as u8;
    let rs1 = r.below(6) as u8;
    let rs2 = r.below(6) as u8;
    // Immediates span the sign boundary: a sign-extension bug that only shows
    // for negative values is exactly what this is looking for.
    let imm = (r.next() as i64 % 4096) - 2048;
    let shamt = r.below(64) as u8;
    let shamtw = r.below(32) as u8;
    // Atomics reach memory through rs1 with no immediate. The ordering bits are
    // randomised even though a single hart ignores them, so a codegen path that
    // accidentally keyed on aq/rl would show up here.
    let aq = r.next() & 1 == 0;
    let rl = r.next() & 1 == 0;
    match r.below(74) {
        0 => Lui { rd, imm: (r.next() & 0xFFFF_F000) as u64 },
        1 => Addi { rd, rs1, imm },
        2 => Slti { rd, rs1, imm },
        3 => Sltiu { rd, rs1, imm },
        4 => Xori { rd, rs1, imm },
        5 => Ori { rd, rs1, imm },
        6 => Andi { rd, rs1, imm },
        7 => Slli { rd, rs1, shamt },
        8 => Srli { rd, rs1, shamt },
        9 => Srai { rd, rs1, shamt },
        10 => Add { rd, rs1, rs2 },
        11 => Sub { rd, rs1, rs2 },
        12 => Sll { rd, rs1, rs2 },
        13 => Slt { rd, rs1, rs2 },
        14 => Sltu { rd, rs1, rs2 },
        15 => Xor { rd, rs1, rs2 },
        16 => Srl { rd, rs1, rs2 },
        17 => Sra { rd, rs1, rs2 },
        18 => Or { rd, rs1, rs2 },
        19 => And { rd, rs1, rs2 },
        20 => Addiw { rd, rs1, imm },
        21 => Slliw { rd, rs1, shamt: shamtw },
        22 => Srliw { rd, rs1, shamt: shamtw },
        23 => Sraiw { rd, rs1, shamt: shamtw },
        24 => Addw { rd, rs1, rs2 },
        25 => Subw { rd, rs1, rs2 },
        26 => Sllw { rd, rs1, rs2 },
        27 => Srlw { rd, rs1, rs2 },
        28 => Sraw { rd, rs1, rs2 },
        29 => Auipc { rd, imm: (r.next() & 0xFFFF_F000) as u64 },
        // Memory: every width and both signednesses, because the extension
        // behaviour is emitted inline and is the easiest thing to get wrong.
        30 => Lb { rd, rs1, imm },
        31 => Lh { rd, rs1, imm },
        32 => Lw { rd, rs1, imm },
        33 => Ld { rd, rs1, imm },
        34 => Lbu { rd, rs1, imm },
        35 => Lhu { rd, rs1, imm },
        36 => Lwu { rd, rs1, imm },
        37 => Sb { rs1, rs2, imm },
        38 => Sh { rs1, rs2, imm },
        39 => Sw { rs1, rs2, imm },
        40 => Sd { rs1, rs2, imm },

        // Fence compiles to nothing; included so "nothing" is verified to be
        // the right amount of nothing rather than assumed.
        41 => Fence { pred: (r.below(16)) as u8, succ: (r.below(16)) as u8 },
        42 => Mul { rd, rs1, rs2 },
        43 => Mulw { rd, rs1, rs2 },

        // Atomics. Both widths of every operation: the .w forms narrow the
        // store and sign-extend the result, and min/max compare with a
        // signedness the opcode picks — three chances to be subtly wrong that
        // the aggregate boot test would only catch by luck.
        44 => Lrw { rd, rs1, aq, rl },
        45 => Lrd { rd, rs1, aq, rl },
        46 => Scw { rd, rs1, rs2, aq, rl },
        47 => Scd { rd, rs1, rs2, aq, rl },
        48 => Amoswapw { rd, rs1, rs2, aq, rl },
        49 => Amoaddw { rd, rs1, rs2, aq, rl },
        50 => Amoxorw { rd, rs1, rs2, aq, rl },
        51 => Amoandw { rd, rs1, rs2, aq, rl },
        52 => Amoorw { rd, rs1, rs2, aq, rl },
        53 => Amominw { rd, rs1, rs2, aq, rl },
        54 => Amomaxw { rd, rs1, rs2, aq, rl },
        55 => Amominuw { rd, rs1, rs2, aq, rl },
        56 => Amomaxuw { rd, rs1, rs2, aq, rl },
        57 => Amoswapd { rd, rs1, rs2, aq, rl },
        58 => Amoaddd { rd, rs1, rs2, aq, rl },
        59 => Amoandd { rd, rs1, rs2, aq, rl },
        60 => Amoord { rd, rs1, rs2, aq, rl },
        61 => Amomind { rd, rs1, rs2, aq, rl },
        // The F/D extension's flag-free subset, which stage 1.5 inlines.
        // rd/rs1/rs2 pick from the same small set; for loads/stores rs1 is an
        // integer base and rd/rs2 an f-register, both masked into scratch by
        // the shared map().
        63 => Fld { rd, rs1, imm },
        64 => Fsd { rs1, rs2, imm },
        65 => Flw { rd, rs1, imm },
        66 => Fsw { rs1, rs2, imm },
        67 => Fp { raw: fp_raw(0x11, rs2, rs1, 0, rd) }, // fsgnj.d
        68 => Fp { raw: fp_raw(0x11, rs2, rs1, 1, rd) }, // fsgnjn.d
        69 => Fp { raw: fp_raw(0x11, rs2, rs1, 2, rd) }, // fsgnjx.d
        70 => {
            // Alternate the two raw moves on the same draw.
            if r.next() & 1 == 0 {
                Fp { raw: fp_raw(0x71, 0, rs1, 0, rd) } // fmv.x.d
            } else {
                Fp { raw: fp_raw(0x79, 0, rs1, 0, rd) } // fmv.d.x
            }
        }
        // The high-multiply family: a 32-bit split with signed corrections —
        // real arithmetic that can be subtly wrong, so it earns coverage.
        71 => Mulh { rd, rs1, rs2 },
        72 => Mulhu { rd, rs1, rs2 },
        73 => Mulhsu { rd, rs1, rs2 },
        _ => Amomaxud { rd, rs1, rs2, aq, rl },
    }
}

/// The guest PC each block starts at. Non-zero and not page-aligned, so a bug
/// that ignores the pc parameter shows up in auipc instead of passing.
const BLOCK_PC: u64 = 0x8000_1234;

/// Where the compiled blocks find the f-register file and the FS word. The
/// JS side (bench/jitenv.js) uses the same constants; blocks bake them in, so the
/// two must agree or every FP case reads garbage.
const FREGS_BASE: u32 = 8192;
const FS_WORD: u32 = 8448;
const FP_CFG: riscv_jit::FpCfg = riscv_jit::FpCfg {
    fregs_base: FREGS_BASE,
    fs_word: FS_WORD,
};

/// Encode an OP-FP instruction.
fn fp_raw(funct7: u32, rs2: u8, rs1: u8, funct3: u32, rd: u8) -> u32 {
    (funct7 << 25) | ((rs2 as u32) << 20) | ((rs1 as u32) << 15) | (funct3 << 12)
        | ((rd as u32) << 7) | 0x53
}

/// Hand-built programs that exercise macro-op fusion. Random draws over 74
/// opcodes and 6 registers essentially never produce an adjacent lui/auipc +
/// addi pair writing the same register, so the fused path needs explicit
/// coverage: the const and PC-relative idioms, sign boundaries in both the
/// upper and lower halves, the pair sitting at the end of a block (fall-through
/// PC), two pairs back to back, a three-instruction chain, and the near-misses
/// that must NOT fuse (different rd, base reused elsewhere, x0 destination).
fn fusion_programs() -> Vec<Vec<Instr>> {
    use Instr::*;
    // A representative upper immediate with bit 31 set, to catch a fused form
    // that drops the interpreter's sign handling.
    let hi_neg = 0xFFFF_F000u64;
    let hi_pos = 0x0002_1000u64;
    vec![
        // lui + addi: plain 32-bit constant, positive and negative lo.
        vec![Lui { rd: 5, imm: hi_pos }, Addi { rd: 5, rs1: 5, imm: 0x123 }],
        vec![Lui { rd: 5, imm: hi_pos }, Addi { rd: 5, rs1: 5, imm: -1 }],
        // lui with the sign bit set + a negative lo (the awkward carry case).
        vec![Lui { rd: 6, imm: hi_neg }, Addi { rd: 6, rs1: 6, imm: -2048 }],
        // auipc + addi: PC-relative address, both signs of lo.
        vec![Auipc { rd: 7, imm: hi_pos }, Addi { rd: 7, rs1: 7, imm: 0x7ff }],
        vec![Auipc { rd: 8, imm: hi_neg }, Addi { rd: 8, rs1: 8, imm: -2048 }],
        // Two fusible pairs back to back.
        vec![
            Lui { rd: 9, imm: hi_pos }, Addi { rd: 9, rs1: 9, imm: 1 },
            Auipc { rd: 10, imm: hi_pos }, Addi { rd: 10, rs1: 10, imm: -3 },
        ],
        // Three in a row: fuse the first pair, the trailing addi reads the
        // committed result.
        vec![
            Lui { rd: 11, imm: hi_pos }, Addi { rd: 11, rs1: 11, imm: 5 },
            Addi { rd: 12, rs1: 11, imm: 7 },
        ],
        // Pair at the very end of the block: exercises the fall-through PC path.
        vec![Add { rd: 3, rs1: 1, rs2: 2 }, Lui { rd: 4, imm: hi_pos }, Addi { rd: 4, rs1: 4, imm: 9 }],
        // NEAR-MISSES that must stay correct without fusing:
        // addi writes a different register than lui wrote.
        vec![Lui { rd: 5, imm: hi_pos }, Addi { rd: 6, rs1: 5, imm: 4 }],
        // the lui result is consumed by a later instruction too, not just addi.
        vec![Lui { rd: 5, imm: hi_pos }, Addi { rd: 5, rs1: 5, imm: 4 }, Add { rd: 13, rs1: 5, rs2: 1 }],
        // addi's source is a different register than lui wrote.
        vec![Lui { rd: 5, imm: hi_pos }, Addi { rd: 5, rs1: 6, imm: 4 }],
        // x0 destination: writes discarded, must remain a no-op.
        vec![Lui { rd: 0, imm: hi_pos }, Addi { rd: 0, rs1: 0, imm: 4 }],
    ]
}

fn main() {
    const LEN: usize = 12;

    let targeted = fusion_programs();
    let cases: usize = 400 + targeted.len();

    let mut r = Rng(0x1234_5678_9ABC_DEF0);
    let mut out = String::from("[\n");
    let mut all_runs: Vec<Vec<(Instr, u8, i32)>> = Vec::new();

    for case in 0..cases {
        let insns: Vec<(Instr, u8, i32)> = if case < targeted.len() {
            targeted[case]
                .iter()
                .enumerate()
                .map(|(k, i)| (*i, 4u8, (k * 4) as i32))
                .collect()
        } else {
            (0..LEN)
                .map(|k| (random_instr(&mut r), 4u8, (k * 4) as i32))
                .collect()
        };

        let mut init = [0u64; 32];
        for (k, slot) in init.iter_mut().enumerate().skip(1) {
            *slot = match k % 4 {
                0 => r.next(),
                1 => r.next() | 0x8000_0000_0000_0000,
                2 => (r.next() as u32) as u64,
                _ => r.below(64),
            };
        }

        // f-register patterns: raw randoms, NaN-boxed singles, canonical NaNs
        // and small doubles -- the mix the boxing rules care about.
        let mut initf = [0u64; 32];
        for (k, slot) in initf.iter_mut().enumerate() {
            *slot = match k % 4 {
                0 => r.next(),
                1 => 0xFFFF_FFFF_0000_0000 | (r.next() as u32 as u64),
                2 => 0x7FF8_0000_0000_0000,
                _ => (r.below(1000) as f64).to_bits(),
            };
        }

        let mut cpu = Cpu::new(BLOCK_PC);
        cpu.x = init;
        cpu.x[0] = 0;
        cpu.f = initf;
        let mut bus = MemBus { mem: fill(case) };
        for (i, w, _) in &insns {
            cpu.execute_width(*i, *w, &mut bus);
        }

        all_runs.push(insns.clone());
        let (wasm, n) = riscv_jit::compile(&insns, Some(FP_CFG)).expect("subset is all compilable");
        assert_eq!(n, insns.len(), "case {case}: compiler stopped early");
        std::fs::write(format!("/tmp/jit_case_{case}.wasm"), &wasm).unwrap();

        out.push_str("  {\"case\":");
        out.push_str(&case.to_string());
        out.push_str(",\"init\":[");
        for (k, v) in init.iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            out.push_str(&format!("\"{v}\""));
        }
        out.push_str("],\"initf\":[");
        for (k, v) in initf.iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            out.push_str(&format!("\"{v}\""));
        }
        out.push_str("],\"expectf\":[");
        for (k, v) in cpu.f.iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            out.push_str(&format!("\"{v}\""));
        }
        out.push_str("],\"expect\":[");
        for (k, v) in cpu.x.iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            out.push_str(&format!("\"{v}\""));
        }
        out.push_str(&format!("],\"memsum\":\"{}\"}}", checksum(&bus.mem)));
        if case + 1 < cases {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("]\n");
    std::fs::write("/tmp/jit_cases.json", out).unwrap();

    // One module holding every block, dispatched internally: the shape
    // production uses, since a module per block makes the call site
    // megamorphic and costs 293 ns per entry instead of 38.
    let refs: Vec<&[(Instr, u8, i32)]> = all_runs.iter().map(|v| &v[..]).collect();
    let (multi, _) = riscv_jit::compile_many(&refs, Some(FP_CFG)).expect("multi-block module");
    std::fs::write("/tmp/jit_multi.wasm", &multi).unwrap();
    eprintln!("multi-block module: {} bytes for {} blocks", multi.len(), refs.len());

    // The same blocks again, but installing themselves into a host
    // module's function table instead of carrying their own. This is the
    // form the emulator uses; base 16 leaves the host's low slots alone.
    let (tbl, _) = riscv_jit::compile_many_into_table(&refs, &[], 16, None, Some(FP_CFG))
        .expect("table module");
    std::fs::write("/tmp/jit_table.wasm", &tbl).unwrap();

    // A short block too: real basic blocks are short, so block entry cost
    // matters as much as the code inside. bench/jit-bench.js backs it out of the two.
    let short: Vec<(Instr, u8, i32)> = (0..3)
        .map(|k| (random_instr(&mut r), 4u8, (k * 4) as i32))
        .collect();
    let (w, _) = riscv_jit::compile(&short, Some(FP_CFG)).expect("short block compiles");
    std::fs::write("/tmp/jit_short.wasm", &w).unwrap();

    eprintln!("wrote {cases} cases to /tmp/jit_cases.json + /tmp/jit_case_N.wasm");
    eprintln!("now run: node bench/jit-difftest.js");
}
