//! RV64F/D conformance spot-checks.
//!
//! Encodings are built by hand rather than assembled, so each test doubles as a
//! check that the decoder routes the opcode to the FPU at all.

use riscv_core::decode::decode;
use riscv_core::execute::{Bus, Cpu};
use riscv_core::types::{Instr, Status};

struct NullBus;
impl Bus for NullBus {
    fn read_u8(&self, _: u64) -> u8 { 0 }
    fn read_u16(&self, _: u64) -> u16 { 0 }
    fn read_u32(&self, _: u64) -> u32 { 0 }
    fn read_u64(&self, _: u64) -> u64 { 0 }
    fn write_u8(&mut self, _: u64, _: u8) {}
    fn write_u16(&mut self, _: u64, _: u16) {}
    fn write_u32(&mut self, _: u64, _: u32) {}
    fn write_u64(&mut self, _: u64, _: u64) {}
}

const OP_FP: u32 = 0x53;

fn r_type(funct7: u32, rs2: u32, rs1: u32, funct3: u32, rd: u32, op: u32) -> u32 {
    (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | op
}

fn boxed(v: f32) -> u64 {
    0xFFFF_FFFF_0000_0000 | v.to_bits() as u64
}

/// Run one instruction with f1/f2/f3 preloaded; returns the cpu afterwards.
fn run(raw: u32, f: [u64; 4], x1: u64) -> Cpu {
    let mut cpu = Cpu::new(0x1000);
    cpu.f[1] = f[1];
    cpu.f[2] = f[2];
    cpu.f[3] = f[3];
    cpu.x[1] = x1;
    let instr = decode(raw);
    assert!(matches!(instr, Instr::Fp { .. }), "decoder did not route {raw:#010x} to the FPU");
    let st = cpu.execute(instr, &mut NullBus);
    assert!(matches!(st, Status::Running), "{raw:#010x} trapped: {st:?}");
    assert_eq!(cpu.pc, 0x1004, "pc must advance by 4");
    cpu
}

#[test]
fn fadd_d_basic() {
    // fadd.d f0, f1, f2
    let raw = r_type(0b0000001, 2, 1, 0, 0, OP_FP);
    let cpu = run(raw, [0, 1.5f64.to_bits(), 2.25f64.to_bits(), 0], 0);
    assert_eq!(f64::from_bits(cpu.f[0]), 3.75);
}

#[test]
fn fadd_s_is_nan_boxed() {
    // fadd.s f0, f1, f2
    let raw = r_type(0b0000000, 2, 1, 0, 0, OP_FP);
    let cpu = run(raw, [0, boxed(1.5), boxed(2.25), 0], 0);
    assert_eq!(
        cpu.f[0] >> 32,
        0xFFFF_FFFF,
        "single-precision results must be NaN-boxed"
    );
    assert_eq!(f32::from_bits(cpu.f[0] as u32), 3.75);
}

#[test]
fn fadd_s_rejects_unboxed_input() {
    // An f register holding a bare f32 (upper half zero) is not a valid single
    // operand; the spec says read it as NaN rather than using the low bits.
    let raw = r_type(0b0000000, 2, 1, 0, 0, OP_FP);
    let cpu = run(raw, [0, 1.5f32.to_bits() as u64, boxed(2.25), 0], 0);
    assert!(
        f32::from_bits(cpu.f[0] as u32).is_nan(),
        "improperly boxed operand must read as NaN, got {}",
        f32::from_bits(cpu.f[0] as u32)
    );
}

#[test]
fn fmul_fdiv_fsqrt_d() {
    let cpu = run(r_type(0b0001001, 2, 1, 0, 0, OP_FP), [0, 3.0f64.to_bits(), 4.0f64.to_bits(), 0], 0);
    assert_eq!(f64::from_bits(cpu.f[0]), 12.0, "fmul.d");

    let cpu = run(r_type(0b0001101, 2, 1, 0, 0, OP_FP), [0, 3.0f64.to_bits(), 4.0f64.to_bits(), 0], 0);
    assert_eq!(f64::from_bits(cpu.f[0]), 0.75, "fdiv.d");

    let cpu = run(r_type(0b0101101, 0, 1, 0, 0, OP_FP), [0, 144.0f64.to_bits(), 0, 0], 0);
    assert_eq!(f64::from_bits(cpu.f[0]), 12.0, "fsqrt.d");
}

#[test]
fn fdiv_by_zero_sets_dz_flag() {
    let mut cpu = Cpu::new(0x1000);
    cpu.f[1] = 1.0f64.to_bits();
    cpu.f[2] = 0.0f64.to_bits();
    let instr = decode(r_type(0b0001101, 2, 1, 0, 0, OP_FP));
    cpu.execute(instr, &mut NullBus);
    assert_eq!(cpu.fcsr & 8, 8, "divide by zero must set fflags.DZ");
    assert!(f64::from_bits(cpu.f[0]).is_infinite());
}

#[test]
fn fflags_are_sticky() {
    let mut cpu = Cpu::new(0x1000);
    cpu.f[1] = 1.0f64.to_bits();
    cpu.f[2] = 0.0f64.to_bits();
    cpu.execute(decode(r_type(0b0001101, 2, 1, 0, 0, OP_FP)), &mut NullBus);
    let after_div = cpu.fcsr;
    assert_ne!(after_div & 8, 0);
    // A subsequent clean operation must not clear the accumulated flag.
    cpu.pc = 0x1000;
    cpu.f[2] = 2.0f64.to_bits();
    cpu.execute(decode(r_type(0b0000001, 2, 1, 0, 0, OP_FP)), &mut NullBus);
    assert_eq!(cpu.fcsr & 8, 8, "fflags must accumulate, not reset");
}

#[test]
fn fsgnj_family() {
    let neg = (-1.0f64).to_bits();
    let pos = 3.0f64.to_bits();
    // fsgnj.d f0, f1, f2 — take magnitude of f1, sign of f2
    let cpu = run(r_type(0b0010001, 2, 1, 0, 0, OP_FP), [0, pos, neg, 0], 0);
    assert_eq!(f64::from_bits(cpu.f[0]), -3.0, "fsgnj.d");
    // fsgnjn.d — inverted sign of f2
    let cpu = run(r_type(0b0010001, 2, 1, 1, 0, OP_FP), [0, pos, neg, 0], 0);
    assert_eq!(f64::from_bits(cpu.f[0]), 3.0, "fsgnjn.d");
    // fsgnjx.d — xor of the signs
    let cpu = run(r_type(0b0010001, 2, 1, 2, 0, OP_FP), [0, neg, neg, 0], 0);
    assert_eq!(f64::from_bits(cpu.f[0]), 1.0, "fsgnjx.d of two negatives is positive");
}

#[test]
fn fmin_fmax_d() {
    let a = 1.0f64.to_bits();
    let b = 2.0f64.to_bits();
    let cpu = run(r_type(0b0010101, 2, 1, 0, 0, OP_FP), [0, a, b, 0], 0);
    assert_eq!(f64::from_bits(cpu.f[0]), 1.0, "fmin.d");
    let cpu = run(r_type(0b0010101, 2, 1, 1, 0, OP_FP), [0, a, b, 0], 0);
    assert_eq!(f64::from_bits(cpu.f[0]), 2.0, "fmax.d");
}

#[test]
fn fmin_returns_the_non_nan_operand() {
    // IEEE minNum semantics: a quiet NaN loses to a real number.
    let nan = f64::NAN.to_bits();
    let cpu = run(r_type(0b0010101, 2, 1, 0, 0, OP_FP), [0, nan, 5.0f64.to_bits(), 0], 0);
    assert_eq!(f64::from_bits(cpu.f[0]), 5.0);
}

#[test]
fn comparisons_write_integer_registers() {
    let a = 1.0f64.to_bits();
    let b = 2.0f64.to_bits();
    // feq.d x5, f1, f2 (funct3=2), flt.d (1), fle.d (0)
    let cpu = run(r_type(0b1010001, 2, 1, 2, 5, OP_FP), [0, a, b, 0], 0);
    assert_eq!(cpu.x[5], 0, "feq.d 1.0 == 2.0");
    let cpu = run(r_type(0b1010001, 2, 1, 1, 5, OP_FP), [0, a, b, 0], 0);
    assert_eq!(cpu.x[5], 1, "flt.d 1.0 < 2.0");
    let cpu = run(r_type(0b1010001, 2, 1, 0, 5, OP_FP), [0, a, a, 0], 0);
    assert_eq!(cpu.x[5], 1, "fle.d 1.0 <= 1.0");
}

#[test]
fn flt_with_nan_signals_invalid() {
    let mut cpu = Cpu::new(0x1000);
    cpu.f[1] = f64::NAN.to_bits();
    cpu.f[2] = 1.0f64.to_bits();
    cpu.execute(decode(r_type(0b1010001, 2, 1, 1, 5, OP_FP)), &mut NullBus);
    assert_eq!(cpu.x[5], 0);
    assert_eq!(cpu.fcsr & 16, 16, "flt.d with NaN must set fflags.NV");
}

#[test]
fn int_float_round_trip() {
    // fcvt.d.l f0, x1  (funct7=0b1101001, rs2=2)
    let cpu = run(r_type(0b1101001, 2, 1, 0, 0, OP_FP), [0; 4], (-42i64) as u64);
    assert_eq!(f64::from_bits(cpu.f[0]), -42.0, "fcvt.d.l");

    // fcvt.l.d x5, f1  (funct7=0b1100001, rs2=2)
    let cpu = run(r_type(0b1100001, 2, 1, 0, 5, OP_FP), [0, (-42.9f64).to_bits(), 0, 0], 0);
    assert_eq!(cpu.x[5] as i64, -42, "fcvt.l.d truncates toward zero");
}

#[test]
fn float_to_int_saturates_and_signals() {
    // Far out of i32 range: saturate to INT_MAX and raise NV, never wrap.
    let mut cpu = Cpu::new(0x1000);
    cpu.f[1] = 1e30f64.to_bits();
    cpu.execute(decode(r_type(0b1100001, 0, 1, 0, 5, OP_FP)), &mut NullBus);
    assert_eq!(cpu.x[5] as i64, i32::MAX as i64, "fcvt.w.d saturates");
    assert_eq!(cpu.fcsr & 16, 16, "and raises NV");

    // NaN converts to the maximum positive value, per the spec.
    let mut cpu = Cpu::new(0x1000);
    cpu.f[1] = f64::NAN.to_bits();
    cpu.execute(decode(r_type(0b1100001, 2, 1, 0, 5, OP_FP)), &mut NullBus);
    assert_eq!(cpu.x[5] as i64, i64::MAX, "fcvt.l.d of NaN");
}

#[test]
fn fcvt_between_widths() {
    // fcvt.s.d f0, f1
    let cpu = run(r_type(0b0100000, 1, 1, 0, 0, OP_FP), [0, 1.5f64.to_bits(), 0, 0], 0);
    assert_eq!(f32::from_bits(cpu.f[0] as u32), 1.5);
    assert_eq!(cpu.f[0] >> 32, 0xFFFF_FFFF, "must stay boxed");
    // fcvt.d.s f0, f1
    let cpu = run(r_type(0b0100001, 0, 1, 0, 0, OP_FP), [0, boxed(1.5), 0, 0], 0);
    assert_eq!(f64::from_bits(cpu.f[0]), 1.5);
}

#[test]
fn fmv_moves_bits_without_converting() {
    // fmv.x.d x5, f1
    let cpu = run(r_type(0b1110001, 0, 1, 0, 5, OP_FP), [0, 0x4008_0000_0000_0000, 0, 0], 0);
    assert_eq!(cpu.x[5], 0x4008_0000_0000_0000);
    // fmv.d.x f0, x1
    let cpu = run(r_type(0b1111001, 0, 1, 0, 0, OP_FP), [0; 4], 0x4008_0000_0000_0000);
    assert_eq!(cpu.f[0], 0x4008_0000_0000_0000);
    // fmv.x.w sign-extends the 32-bit pattern
    let cpu = run(r_type(0b1110000, 0, 1, 0, 5, OP_FP), [0, 0xFFFF_FFFF_8000_0000, 0, 0], 0);
    assert_eq!(cpu.x[5], 0xFFFF_FFFF_8000_0000, "fmv.x.w sign-extends");
}

#[test]
fn fclass_identifies_each_category() {
    let cases: [(u64, u64); 6] = [
        (f64::NEG_INFINITY.to_bits(), 1 << 0),
        ((-1.0f64).to_bits(), 1 << 1),
        ((-0.0f64).to_bits(), 1 << 3),
        ((0.0f64).to_bits(), 1 << 4),
        ((1.0f64).to_bits(), 1 << 6),
        (f64::INFINITY.to_bits(), 1 << 7),
    ];
    for (bits, expect) in cases {
        let cpu = run(r_type(0b1110001, 0, 1, 1, 5, OP_FP), [0, bits, 0, 0], 0);
        assert_eq!(cpu.x[5], expect, "fclass.d of {:#x}", bits);
    }
}

#[test]
fn fused_multiply_add_forms() {
    let f = [0, 2.0f64.to_bits(), 3.0f64.to_bits(), 4.0f64.to_bits()];
    // rs3 in bits 31:27, fmt=01 in bits 26:25
    let enc = |op: u32| (3u32 << 27) | (1 << 25) | (2 << 20) | (1 << 15) | (0 << 7) | op;
    assert_eq!(f64::from_bits(run(enc(0x43), f, 0).f[0]), 10.0, "fmadd.d 2*3+4");
    assert_eq!(f64::from_bits(run(enc(0x47), f, 0).f[0]), 2.0, "fmsub.d 2*3-4");
    assert_eq!(f64::from_bits(run(enc(0x4b), f, 0).f[0]), -2.0, "fnmsub.d -(2*3)+4");
    assert_eq!(f64::from_bits(run(enc(0x4f), f, 0).f[0]), -10.0, "fnmadd.d -(2*3)-4");
}

#[test]
fn fp_marks_state_dirty() {
    let mut cpu = Cpu::new(0x1000);
    cpu.f[1] = 1.0f64.to_bits();
    cpu.f[2] = 1.0f64.to_bits();
    assert!(!cpu.fs_dirty);
    cpu.execute(decode(r_type(0b0000001, 2, 1, 0, 0, OP_FP)), &mut NullBus);
    assert!(cpu.fs_dirty, "writing an f register must flag FP state dirty");
}

#[test]
fn comparison_does_not_dirty_fp_state() {
    // FEQ/FLT/FLE write an integer register only, so FS must stay put —
    // otherwise every comparison would force a pointless FP save on switch.
    let mut cpu = Cpu::new(0x1000);
    cpu.f[1] = 1.0f64.to_bits();
    cpu.f[2] = 2.0f64.to_bits();
    cpu.execute(decode(r_type(0b1010001, 2, 1, 1, 5, OP_FP)), &mut NullBus);
    assert!(!cpu.fs_dirty);
}

#[test]
fn invalid_fp_encoding_traps() {
    let mut cpu = Cpu::new(0x1000);
    // funct7 >> 2 == 0b01111 is not an allocated OP-FP operation.
    let st = cpu.execute(decode(r_type(0b0111101, 0, 1, 0, 0, OP_FP)), &mut NullBus);
    assert!(
        matches!(st, Status::Trap(_)),
        "unallocated OP-FP encodings must raise IllegalInstruction, not silently pass"
    );
}
