//! RV64F / RV64D floating point.
//!
//! The devicetree advertises `rv64imafdc` and every Alpine userspace binary is
//! built for rv64gc, so F and D are not optional: musl's printf, busybox's
//! `sleep` and `dd`, and anything doing arithmetic on a rate or a timestamp all
//! execute OP-FP. Without this, userspace dies with SIGILL the moment it does
//! anything numeric.
//!
//! Only the FP *loads and stores* used to exist here, which was enough for the
//! kernel to context-switch FP state around but not for anyone to compute with
//! it.
//!
//! Scope: the compute opcodes — OP-FP (0x53) and the four fused multiply-add
//! forms (0x43/0x47/0x4B/0x4F). Loads and stores stay in `execute.rs`.
//!
//! Two conventions matter and are easy to get wrong:
//!
//! * **NaN boxing.** A 32-bit float held in a 64-bit `f` register must have all
//!   upper bits set. A single-precision op reading a register whose upper half
//!   is not all ones must treat the input as NaN, or `fmv.w.x`-then-`fadd.s`
//!   silently computes on garbage.
//! * **fflags are sticky.** Exception flags accumulate until software clears
//!   them; they are never reset by a subsequent clean operation.

use crate::execute::Cpu;

// fflags bits (fcsr[4:0])
pub const FFLAG_NX: u64 = 1; // inexact
pub const FFLAG_UF: u64 = 2; // underflow
pub const FFLAG_OF: u64 = 4; // overflow
pub const FFLAG_DZ: u64 = 8; // divide by zero
pub const FFLAG_NV: u64 = 16; // invalid operation

/// Quiet NaN patterns produced when an operation has no meaningful result.
const CANON_NAN_S: u32 = 0x7fc0_0000;
const CANON_NAN_D: u64 = 0x7ff8_0000_0000_0000;

/// Unbox a single-precision operand. Per the spec an improperly boxed value
/// reads as a canonical NaN rather than as its low 32 bits.
fn unbox_s(bits: u64) -> f32 {
    if bits >> 32 == 0xFFFF_FFFF {
        f32::from_bits(bits as u32)
    } else {
        f32::from_bits(CANON_NAN_S)
    }
}

fn box_s(v: f32) -> u64 {
    0xFFFF_FFFF_0000_0000 | v.to_bits() as u64
}

fn as_d(bits: u64) -> f64 {
    f64::from_bits(bits)
}

/// Is this bit pattern a signalling NaN?
fn is_snan_s(b: u32) -> bool {
    (b & 0x7f80_0000) == 0x7f80_0000 && (b & 0x007f_ffff) != 0 && (b & 0x0040_0000) == 0
}

fn is_snan_d(b: u64) -> bool {
    (b & 0x7ff0_0000_0000_0000) == 0x7ff0_0000_0000_0000
        && (b & 0x000f_ffff_ffff_ffff) != 0
        && (b & 0x0008_0000_0000_0000) == 0
}

/// Result of executing one FP instruction.
pub struct FpResult {
    /// Exception flags to OR into fflags.
    pub flags: u64,
    /// True if an `f` register or fcsr was written, so mstatus.FS must go Dirty.
    pub dirty: bool,
    /// False if the encoding is not a valid FP instruction.
    pub ok: bool,
}

impl FpResult {
    fn new() -> Self {
        Self { flags: 0, dirty: false, ok: true }
    }
    fn bad() -> Self {
        Self { flags: 0, dirty: false, ok: false }
    }
}

/// Classify for FCLASS.{S,D}: a one-hot 10-bit mask.
fn fclass_bits(sign: bool, exp_max: bool, exp_zero: bool, mant_zero: bool, quiet: bool) -> u64 {
    if exp_max && mant_zero {
        if sign { 1 << 0 } else { 1 << 7 } // -inf / +inf
    } else if exp_max {
        if quiet { 1 << 9 } else { 1 << 8 } // qNaN / sNaN
    } else if exp_zero && mant_zero {
        if sign { 1 << 3 } else { 1 << 4 } // -0 / +0
    } else if exp_zero {
        if sign { 1 << 2 } else { 1 << 5 } // -subnormal / +subnormal
    } else if sign {
        1 << 1 // -normal
    } else {
        1 << 6 // +normal
    }
}

fn fclass_s(b: u32) -> u64 {
    fclass_bits(
        b >> 31 == 1,
        (b & 0x7f80_0000) == 0x7f80_0000,
        (b & 0x7f80_0000) == 0,
        (b & 0x007f_ffff) == 0,
        (b & 0x0040_0000) != 0,
    )
}

fn fclass_d(b: u64) -> u64 {
    fclass_bits(
        b >> 63 == 1,
        (b & 0x7ff0_0000_0000_0000) == 0x7ff0_0000_0000_0000,
        (b & 0x7ff0_0000_0000_0000) == 0,
        (b & 0x000f_ffff_ffff_ffff) == 0,
        (b & 0x0008_0000_0000_0000) != 0,
    )
}

/// Exact residual of a double-precision op: `exact_result - rounded_result`.
/// Non-zero means the operation rounded, i.e. it was inexact.
///
/// There is no wider float to compare against for f64, so this uses
/// error-free transforms: 2Sum for add/sub, and fma to recover the exact
/// product or dividend for mul/div.
fn residual_d(op: u8, a: f64, b: f64, v: f64) -> f64 {
    match op {
        0b00000 => {
            let bb = v - a;
            (a - (v - bb)) + (b - bb)
        }
        0b00001 => {
            let nb = -b;
            let bb = v - a;
            (a - (v - bb)) + (nb - bb)
        }
        0b00010 => libm::fma(a, b, -v),
        // a - v*b; non-zero when the quotient did not divide exactly.
        _ => libm::fma(-v, b, a),
    }
}

/// Did a single-precision op round? f64 is wide enough to hold the exact
/// result of any f32 add/sub/mul, so a plain comparison settles it.
fn inexact_s(exact: f64, v: f32) -> bool {
    v.is_finite() && (v as f64) != exact
}

/// Flags for a float→int conversion that cannot be represented.
fn cvt_invalid<T>(_: T) -> u64 {
    FFLAG_NV
}

/// f64 → signed integer with RISC-V's out-of-range behaviour: saturate, and
/// raise NV rather than NX. NaN converts to the maximum positive value.
fn f64_to_i64(v: f64) -> (u64, u64) {
    if v.is_nan() {
        return (i64::MAX as u64, cvt_invalid(v));
    }
    let r = libm::trunc(v);
    if r >= 9223372036854775808.0 {
        (i64::MAX as u64, FFLAG_NV)
    } else if r < -9223372036854775808.0 {
        (i64::MIN as u64, FFLAG_NV)
    } else {
        ((r as i64) as u64, if r != v { FFLAG_NX } else { 0 })
    }
}

fn f64_to_u64(v: f64) -> (u64, u64) {
    if v.is_nan() {
        return (u64::MAX, FFLAG_NV);
    }
    let r = libm::trunc(v);
    if r >= 18446744073709551616.0 {
        (u64::MAX, FFLAG_NV)
    } else if r <= -1.0 {
        (0, FFLAG_NV)
    } else {
        (r as u64, if r != v { FFLAG_NX } else { 0 })
    }
}

fn f64_to_i32(v: f64) -> (u64, u64) {
    if v.is_nan() {
        return (i32::MAX as i64 as u64, FFLAG_NV);
    }
    let r = libm::trunc(v);
    if r >= 2147483648.0 {
        (i32::MAX as i64 as u64, FFLAG_NV)
    } else if r < -2147483648.0 {
        (i32::MIN as i64 as u64, FFLAG_NV)
    } else {
        ((r as i32) as i64 as u64, if r != v { FFLAG_NX } else { 0 })
    }
}

fn f64_to_u32(v: f64) -> (u64, u64) {
    if v.is_nan() {
        return (u32::MAX as i32 as i64 as u64, FFLAG_NV);
    }
    let r = libm::trunc(v);
    if r >= 4294967296.0 {
        (u32::MAX as i32 as i64 as u64, FFLAG_NV)
    } else if r <= -1.0 {
        (0, FFLAG_NV)
    } else {
        ((r as u32) as i32 as i64 as u64, if r != v { FFLAG_NX } else { 0 })
    }
}

/// FMIN/FMAX: NaN-quieting, and -0.0 compares below +0.0.
fn min_max<T: PartialOrd + Copy>(a: T, b: T, a_nan: bool, b_nan: bool, is_max: bool, neg_a: bool)
    -> (Option<T>, bool)
{
    // Returns (result, both_nan). Caller substitutes a canonical NaN when None.
    match (a_nan, b_nan) {
        (true, true) => (None, true),
        (true, false) => (Some(b), false),
        (false, true) => (Some(a), false),
        (false, false) => {
            if a == b {
                // Equal magnitudes but possibly differing zero signs: min picks
                // the negative zero, max the positive one.
                let pick_a = if is_max { !neg_a } else { neg_a };
                (Some(if pick_a { a } else { b }), false)
            } else if (a < b) != is_max {
                (Some(a), false)
            } else {
                (Some(b), false)
            }
        }
    }
}

/// Execute one OP-FP or fused-multiply-add instruction.
///
/// Returns `ok: false` when `raw` is not a valid FP encoding, so the caller can
/// raise IllegalInstruction instead of silently doing nothing.
pub fn execute(cpu: &mut Cpu, raw: u32) -> FpResult {
    let opcode = raw & 0x7f;
    let rd = ((raw >> 7) & 0x1f) as u8;
    let rs1 = ((raw >> 15) & 0x1f) as u8;
    let rs2 = ((raw >> 20) & 0x1f) as u8;
    let rs3 = ((raw >> 27) & 0x1f) as u8;
    let funct7 = ((raw >> 25) & 0x7f) as u8;
    let fmt = (raw >> 25) & 3; // 0 = single, 1 = double
    let mut r = FpResult::new();

    // ---- fused multiply-add family ----
    if matches!(opcode, 0x43 | 0x47 | 0x4b | 0x4f) {
        // The N forms negate the PRODUCT, and the spec's names read backwards
        // relative to the addend: FNMSUB *adds* rs3, FNMADD *subtracts* it.
        let (neg_prod, neg_add) = match opcode {
            0x43 => (false, false), // fmadd:   (a*b) + c
            0x47 => (false, true),  // fmsub:   (a*b) - c
            0x4b => (true, false),  // fnmsub: -(a*b) + c
            _ => (true, true),      // fnmadd: -(a*b) - c
        };
        r.dirty = true;
        if fmt == 0 {
            let a = unbox_s(cpu.f[rs1 as usize]);
            let b = unbox_s(cpu.f[rs2 as usize]);
            let c = unbox_s(cpu.f[rs3 as usize]);
            let (a, c) = (if neg_prod { -a } else { a }, if neg_add { -c } else { c });
            let v = libm::fmaf(a, b, c);
            if a.is_finite() && b.is_finite() && c.is_finite()
                && inexact_s(a as f64 * b as f64 + c as f64, v)
            {
                r.flags |= FFLAG_NX;
            }
            if a.is_nan() || b.is_nan() || c.is_nan() {
                r.flags |= FFLAG_NV
                    * (is_snan_s(cpu.f[rs1 as usize] as u32)
                        || is_snan_s(cpu.f[rs2 as usize] as u32)
                        || is_snan_s(cpu.f[rs3 as usize] as u32)) as u64;
            }
            cpu.f[rd as usize] = if v.is_nan() { box_s(f32::from_bits(CANON_NAN_S)) } else { box_s(v) };
        } else if fmt == 1 {
            let a = as_d(cpu.f[rs1 as usize]);
            let b = as_d(cpu.f[rs2 as usize]);
            let c = as_d(cpu.f[rs3 as usize]);
            let (a, c) = (if neg_prod { -a } else { a }, if neg_add { -c } else { c });
            let v = libm::fma(a, b, c);
            if a.is_finite() && b.is_finite() && c.is_finite() && v.is_finite() {
                // Exact a*b+c as a double-double: product error plus the 2Sum
                // error of adding c. Non-zero means the fma rounded.
                let p = a * b;
                let pe = libm::fma(a, b, -p);
                let s = p + c;
                let bb = s - p;
                let se = (p - (s - bb)) + (c - bb);
                if pe + se != 0.0 {
                    r.flags |= FFLAG_NX;
                }
            }
            if is_snan_d(cpu.f[rs1 as usize]) || is_snan_d(cpu.f[rs2 as usize]) || is_snan_d(cpu.f[rs3 as usize]) {
                r.flags |= FFLAG_NV;
            }
            cpu.f[rd as usize] = if v.is_nan() { CANON_NAN_D } else { v.to_bits() };
        } else {
            return FpResult::bad();
        }
        return r;
    }

    if opcode != 0x53 {
        return FpResult::bad();
    }

    // ---- OP-FP ----
    // funct7[6:2] selects the operation, funct7[1:0] the format.
    match funct7 >> 2 {
        // FADD / FSUB / FMUL / FDIV
        0b00000 | 0b00001 | 0b00010 | 0b00011 => {
            r.dirty = true;
            let op = funct7 >> 2;
            if fmt == 0 {
                let a = unbox_s(cpu.f[rs1 as usize]);
                let b = unbox_s(cpu.f[rs2 as usize]);
                if is_snan_s(cpu.f[rs1 as usize] as u32) || is_snan_s(cpu.f[rs2 as usize] as u32) {
                    r.flags |= FFLAG_NV;
                }
                let v = match op {
                    0b00000 => a + b,
                    0b00001 => a - b,
                    0b00010 => a * b,
                    _ => {
                        if b == 0.0 && !a.is_nan() && a != 0.0 {
                            r.flags |= FFLAG_DZ;
                        } else if a == 0.0 && b == 0.0 {
                            r.flags |= FFLAG_NV;
                        }
                        a / b
                    }
                };
                // A NaN out of non-NaN inputs means the operation itself was
                // invalid: Inf-Inf, 0*Inf, 0/0, Inf/Inf.
                if v.is_nan() && !a.is_nan() && !b.is_nan() {
                    r.flags |= FFLAG_NV;
                }
                if v.is_infinite() && a.is_finite() && b.is_finite() && op != 0b00011 {
                    r.flags |= FFLAG_OF | FFLAG_NX;
                } else if a.is_finite() && b.is_finite() {
                    let (x, y) = (a as f64, b as f64);
                    let exact = match op {
                        0b00000 => x + y,
                        0b00001 => x - y,
                        0b00010 => x * y,
                        _ => x / y,
                    };
                    if inexact_s(exact, v) {
                        r.flags |= FFLAG_NX;
                    }
                }
                cpu.f[rd as usize] = if v.is_nan() { box_s(f32::from_bits(CANON_NAN_S)) } else { box_s(v) };
            } else if fmt == 1 {
                let a = as_d(cpu.f[rs1 as usize]);
                let b = as_d(cpu.f[rs2 as usize]);
                if is_snan_d(cpu.f[rs1 as usize]) || is_snan_d(cpu.f[rs2 as usize]) {
                    r.flags |= FFLAG_NV;
                }
                let v = match op {
                    0b00000 => a + b,
                    0b00001 => a - b,
                    0b00010 => a * b,
                    _ => {
                        if b == 0.0 && !a.is_nan() && a != 0.0 {
                            r.flags |= FFLAG_DZ;
                        } else if a == 0.0 && b == 0.0 {
                            r.flags |= FFLAG_NV;
                        }
                        a / b
                    }
                };
                if v.is_nan() && !a.is_nan() && !b.is_nan() {
                    r.flags |= FFLAG_NV;
                }
                if v.is_infinite() && a.is_finite() && b.is_finite() && op != 0b00011 {
                    r.flags |= FFLAG_OF | FFLAG_NX;
                } else if a.is_finite() && b.is_finite() && v.is_finite()
                    && residual_d(op, a, b, v) != 0.0
                {
                    r.flags |= FFLAG_NX;
                }
                cpu.f[rd as usize] = if v.is_nan() { CANON_NAN_D } else { v.to_bits() };
            } else {
                return FpResult::bad();
            }
        }

        // FSQRT
        0b01011 => {
            r.dirty = true;
            if fmt == 0 {
                let a = unbox_s(cpu.f[rs1 as usize]);
                if a < 0.0 {
                    r.flags |= FFLAG_NV;
                }
                let v = libm::sqrtf(a);
                if a.is_finite() && a > 0.0 && inexact_s(libm::sqrt(a as f64), v) {
                    r.flags |= FFLAG_NX;
                }
                cpu.f[rd as usize] = if v.is_nan() { box_s(f32::from_bits(CANON_NAN_S)) } else { box_s(v) };
            } else {
                let a = as_d(cpu.f[rs1 as usize]);
                if a < 0.0 {
                    r.flags |= FFLAG_NV;
                }
                let v = libm::sqrt(a);
                // a - v*v is the exact residual; non-zero means it rounded.
                if a.is_finite() && a > 0.0 && libm::fma(-v, v, a) != 0.0 {
                    r.flags |= FFLAG_NX;
                }
                cpu.f[rd as usize] = if v.is_nan() { CANON_NAN_D } else { v.to_bits() };
            }
        }

        // FSGNJ / FSGNJN / FSGNJX — pure bit manipulation, never raise flags.
        0b00100 => {
            r.dirty = true;
            let funct3 = (raw >> 12) & 7;
            if fmt == 0 {
                // Operands must be unboxed first: fsgnj.s on an improperly
                // boxed register works on a canonical NaN, not on whatever
                // happens to sit in the low 32 bits.
                let a = unbox_s(cpu.f[rs1 as usize]).to_bits();
                let b = unbox_s(cpu.f[rs2 as usize]).to_bits();
                let sign = match funct3 {
                    0 => b & 0x8000_0000,
                    1 => !b & 0x8000_0000,
                    2 => (a ^ b) & 0x8000_0000,
                    _ => return FpResult::bad(),
                };
                cpu.f[rd as usize] = box_s(f32::from_bits((a & 0x7fff_ffff) | sign));
            } else if fmt == 1 {
                let a = cpu.f[rs1 as usize];
                let b = cpu.f[rs2 as usize];
                let sign = match funct3 {
                    0 => b & 0x8000_0000_0000_0000,
                    1 => !b & 0x8000_0000_0000_0000,
                    2 => (a ^ b) & 0x8000_0000_0000_0000,
                    _ => return FpResult::bad(),
                };
                cpu.f[rd as usize] = (a & 0x7fff_ffff_ffff_ffff) | sign;
            } else {
                return FpResult::bad();
            }
        }

        // FMIN / FMAX
        0b00101 => {
            r.dirty = true;
            let is_max = (raw >> 12) & 7 == 1;
            if fmt == 0 {
                let (ab, bb) = (cpu.f[rs1 as usize] as u32, cpu.f[rs2 as usize] as u32);
                let (a, b) = (unbox_s(cpu.f[rs1 as usize]), unbox_s(cpu.f[rs2 as usize]));
                if is_snan_s(ab) || is_snan_s(bb) {
                    r.flags |= FFLAG_NV;
                }
                let (v, both_nan) =
                    min_max(a, b, a.is_nan(), b.is_nan(), is_max, a.is_sign_negative());
                cpu.f[rd as usize] = if both_nan {
                    box_s(f32::from_bits(CANON_NAN_S))
                } else {
                    box_s(v.unwrap())
                };
            } else if fmt == 1 {
                let (ab, bb) = (cpu.f[rs1 as usize], cpu.f[rs2 as usize]);
                let (a, b) = (as_d(ab), as_d(bb));
                if is_snan_d(ab) || is_snan_d(bb) {
                    r.flags |= FFLAG_NV;
                }
                let (v, both_nan) =
                    min_max(a, b, a.is_nan(), b.is_nan(), is_max, a.is_sign_negative());
                cpu.f[rd as usize] = if both_nan { CANON_NAN_D } else { v.unwrap().to_bits() };
            } else {
                return FpResult::bad();
            }
        }

        // FCVT.S.D / FCVT.D.S — format conversion between float widths.
        0b01000 => {
            r.dirty = true;
            match (fmt, rs2) {
                (0, 1) => {
                    // fcvt.s.d
                    let a = as_d(cpu.f[rs1 as usize]);
                    if is_snan_d(cpu.f[rs1 as usize]) {
                        r.flags |= FFLAG_NV;
                    }
                    let v = a as f32;
                    cpu.f[rd as usize] = if v.is_nan() {
                        box_s(f32::from_bits(CANON_NAN_S))
                    } else {
                        if v.is_infinite() && a.is_finite() {
                            r.flags |= FFLAG_OF | FFLAG_NX;
                        } else if (v as f64) != a {
                            r.flags |= FFLAG_NX;
                        }
                        box_s(v)
                    };
                }
                (1, 0) => {
                    // fcvt.d.s — always exact
                    let a = unbox_s(cpu.f[rs1 as usize]);
                    if is_snan_s(cpu.f[rs1 as usize] as u32) {
                        r.flags |= FFLAG_NV;
                    }
                    let v = a as f64;
                    cpu.f[rd as usize] = if v.is_nan() { CANON_NAN_D } else { v.to_bits() };
                }
                _ => return FpResult::bad(),
            }
        }

        // FEQ / FLT / FLE — write an integer register.
        0b10100 => {
            let funct3 = (raw >> 12) & 7;
            let (a_nan, b_nan, res, sig) = if fmt == 0 {
                let (ab, bb) = (cpu.f[rs1 as usize] as u32, cpu.f[rs2 as usize] as u32);
                let (a, b) = (unbox_s(cpu.f[rs1 as usize]), unbox_s(cpu.f[rs2 as usize]));
                let res = match funct3 {
                    2 => a == b,
                    1 => a < b,
                    0 => a <= b,
                    _ => return FpResult::bad(),
                };
                (a.is_nan(), b.is_nan(), res, is_snan_s(ab) || is_snan_s(bb))
            } else if fmt == 1 {
                let (ab, bb) = (cpu.f[rs1 as usize], cpu.f[rs2 as usize]);
                let (a, b) = (as_d(ab), as_d(bb));
                let res = match funct3 {
                    2 => a == b,
                    1 => a < b,
                    0 => a <= b,
                    _ => return FpResult::bad(),
                };
                (a.is_nan(), b.is_nan(), res, is_snan_d(ab) || is_snan_d(bb))
            } else {
                return FpResult::bad();
            };
            // FEQ is the quiet comparison: only a signalling NaN raises. FLT and
            // FLE are signalling: any NaN raises.
            if (funct3 == 2 && sig) || (funct3 != 2 && (a_nan || b_nan)) {
                r.flags |= FFLAG_NV;
            }
            cpu.write_reg(rd, res as u64);
        }

        // FCLASS / FMV.X.W / FMV.X.D — float to integer register, no conversion.
        0b11100 => {
            let funct3 = (raw >> 12) & 7;
            let v = match (funct3, fmt) {
                (1, 0) => fclass_s(cpu.f[rs1 as usize] as u32),
                (1, 1) => fclass_d(cpu.f[rs1 as usize]),
                (0, 0) => (cpu.f[rs1 as usize] as u32 as i32) as i64 as u64, // fmv.x.w sign-extends
                (0, 1) => cpu.f[rs1 as usize],                               // fmv.x.d
                _ => return FpResult::bad(),
            };
            cpu.write_reg(rd, v);
        }

        // FMV.W.X / FMV.D.X — integer register to float, no conversion.
        0b11110 => {
            r.dirty = true;
            match fmt {
                0 => cpu.f[rd as usize] = box_s(f32::from_bits(cpu.read_reg(rs1) as u32)),
                1 => cpu.f[rd as usize] = cpu.read_reg(rs1),
                _ => return FpResult::bad(),
            }
        }

        // FCVT.{W,WU,L,LU}.{S,D} — float to integer.
        0b11000 => {
            let v = if fmt == 0 {
                unbox_s(cpu.f[rs1 as usize]) as f64
            } else if fmt == 1 {
                as_d(cpu.f[rs1 as usize])
            } else {
                return FpResult::bad();
            };
            let (res, fl) = match rs2 {
                0 => f64_to_i32(v),
                1 => f64_to_u32(v),
                2 => f64_to_i64(v),
                3 => f64_to_u64(v),
                _ => return FpResult::bad(),
            };
            r.flags |= fl;
            cpu.write_reg(rd, res);
        }

        // FCVT.{S,D}.{W,WU,L,LU} — integer to float.
        0b11010 => {
            r.dirty = true;
            let x = cpu.read_reg(rs1);
            let v: f64 = match rs2 {
                0 => (x as i32) as f64,
                1 => (x as u32) as f64,
                2 => (x as i64) as f64,
                3 => x as f64,
                _ => return FpResult::bad(),
            };
            if fmt == 0 {
                let f = v as f32;
                if (f as f64) != v {
                    r.flags |= FFLAG_NX;
                }
                cpu.f[rd as usize] = box_s(f);
            } else if fmt == 1 {
                // i64/u64 -> f64 can be inexact; i32/u32 never is.
                if matches!(rs2, 2 | 3) {
                    let back = if rs2 == 2 { (v as i64) as u64 } else { v as u64 };
                    if back != x {
                        r.flags |= FFLAG_NX;
                    }
                }
                cpu.f[rd as usize] = v.to_bits();
            } else {
                return FpResult::bad();
            }
        }

        _ => return FpResult::bad(),
    }

    r
}

/// Is this opcode handled by [`execute`]?
pub fn is_fp_opcode(raw: u32) -> bool {
    matches!(raw & 0x7f, 0x53 | 0x43 | 0x47 | 0x4b | 0x4f)
}
