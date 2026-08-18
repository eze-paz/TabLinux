//! Compressed (RVC) instruction decompression — RV64
use crate::types::Instr;

fn sext(v: u32, n: u32) -> i64 {
    let shift = 32u32 - n;
    ((v.wrapping_shl(shift)) as i32 >> shift) as i64
}

pub fn decompress(raw: u16) -> Option<Instr> {
    let r = raw as u32;
    let q = raw & 0b11;
    let f3 = (raw >> 13) & 0b111;

    match q {
        0b00 => match f3 {
            0b000 => {
                let rd = ((r >> 2) & 0b111) as u8 + 8;
                let imm = (((r >> 6) & 1) << 2)
                        | (((r >> 5) & 1) << 3)
                        | (((r >> 11) & 0b11) << 4)
                        | (((r >> 7) & 0b1111) << 6);
                if imm == 0 {
                    // c.addi4spn with nzimm=0 is RESERVED, and the all-zero
                    // halfword is the spec DEFINED ILLEGAL instruction. Both
                    // must trap - executing zeroed memory as no-ops turns any
                    // wild jump into a silent march instead of a fault.
                    return None;
                }
                Some(Instr::Addi { rd, rs1: 2, imm: imm as i64 })
            }
            0b001 => {
                let rd = ((r >> 2) & 0b111) as u8 + 8;
                let rs1 = ((r >> 7) & 0b111) as u8 + 8;
                let imm = (((r >> 10) & 0b111) << 3)
                        | (((r >> 5) & 0b11) << 6);
                Some(Instr::Fld { rd, rs1, imm: imm as i64 })
            }
            0b010 => {
                let imm = (((r >> 6) & 1) << 2)
                        | (((r >> 10) & 0b111) << 3)
                        | (((r >> 5) & 1) << 6);
                let rd = ((r >> 2) & 0b111) as u8 + 8;
                let rs1 = ((r >> 7) & 0b111) as u8 + 8;
                Some(Instr::Lw { rd, rs1, imm: imm as i64 })
            }
            0b011 => {
                let imm = (((r >> 10) & 0b111) << 3)
                        | (((r >> 5) & 0b11) << 6);
                let rd = ((r >> 2) & 0b111) as u8 + 8;
                let rs1 = ((r >> 7) & 0b111) as u8 + 8;
                Some(Instr::Ld { rd, rs1, imm: imm as i64 })
            }
            0b100 => {
                // Zcb extension: c.lbu, c.sb, c.lhu, c.sh
                let funct2 = (r >> 10) & 0b11;
                let rs1p = ((r >> 7) & 0b111) as u8 + 8;
                match funct2 {
                    0b00 => {
                        let rd = ((r >> 2) & 0b111) as u8 + 8;
                        let imm = (((r >> 12) & 1) << 2)
                                | (((r >> 6) & 1) << 1)
                                | ((r >> 5) & 1);
                        Some(Instr::Lbu { rd, rs1: rs1p, imm: imm as i64 })
                    }
                    0b01 => {
                        let rs2 = ((r >> 2) & 0b111) as u8 + 8;
                        let imm = (((r >> 12) & 1) << 2)
                                | (((r >> 6) & 1) << 1)
                                | ((r >> 5) & 1);
                        Some(Instr::Sb { rs1: rs1p, rs2, imm: imm as i64 })
                    }
                    0b10 => {
                        let rd = ((r >> 2) & 0b111) as u8 + 8;
                        let imm = (((r >> 12) & 1) << 1)
                                | ((r >> 6) & 1);
                        Some(Instr::Lhu { rd, rs1: rs1p, imm: (imm as i64) * 2 })
                    }
                    0b11 => {
                        let rs2 = ((r >> 2) & 0b111) as u8 + 8;
                        let imm = (((r >> 12) & 1) << 1)
                                | ((r >> 6) & 1);
                        Some(Instr::Sh { rs1: rs1p, rs2, imm: (imm as i64) * 2 })
                    }
                    _ => None,
                }
            }
            0b101 => {
                let rs2 = ((r >> 2) & 0b111) as u8 + 8;
                let rs1 = ((r >> 7) & 0b111) as u8 + 8;
                let imm = (((r >> 10) & 0b111) << 3)
                        | (((r >> 5) & 0b11) << 6);
                Some(Instr::Fsd { rs1, rs2, imm: imm as i64 })
            }
            0b110 => {
                let imm = (((r >> 6) & 1) << 2)
                        | (((r >> 10) & 0b111) << 3)
                        | (((r >> 5) & 1) << 6);
                let rs2 = ((r >> 2) & 0b111) as u8 + 8;
                let rs1 = ((r >> 7) & 0b111) as u8 + 8;
                Some(Instr::Sw { rs1, rs2, imm: imm as i64 })
            }
            0b111 => {
                let imm = (((r >> 10) & 0b111) << 3)
                        | (((r >> 5) & 0b11) << 6);
                let rs2 = ((r >> 2) & 0b111) as u8 + 8;
                let rs1 = ((r >> 7) & 0b111) as u8 + 8;
                Some(Instr::Sd { rs1, rs2, imm: imm as i64 })
            }
            _ => None,
        },
        0b01 => match f3 {
            0b000 => {
                let rd = ((r >> 7) & 0b11111) as u8;
                let imm = sext(((r >> 12) & 1) << 5 | ((r >> 2) & 0b11111), 6);
                Some(Instr::Addi { rd, rs1: rd, imm })
            }
            0b001 => {
                let rd = ((r >> 7) & 0b11111) as u8;
                let imm = sext(((r >> 12) & 1) << 5 | ((r >> 2) & 0b11111), 6);
                Some(Instr::Addiw { rd, rs1: rd, imm })
            }
            0b010 => {
                // C.LI: addi rd, x0, imm
                let rd = ((r >> 7) & 0b11111) as u8;
                let imm = sext(((r >> 12) & 1) << 5 | ((r >> 2) & 0b11111), 6);
                Some(Instr::Addi { rd, rs1: 0, imm })
            }
            0b011 => {
                let rd = ((r >> 7) & 0b11111) as u8;
                if rd == 2 {
                    let imm = sext(
                        ((r >> 12) & 1) << 9
                            | ((r >> 3) & 0b11) << 7
                            | ((r >> 5) & 1) << 6
                            | ((r >> 2) & 1) << 5
                            | ((r >> 6) & 1) << 4,
                        10,
                    );
                    Some(Instr::Addi { rd: 2, rs1: 2, imm })
                } else {
                    let imm = sext(
                        ((r >> 12) & 1) << 17 | ((r >> 2) & 0b11111) << 12,
                        18,
                    );
                    Some(Instr::Lui { rd, imm: imm as u64 })
                }
            }
            0b100 => {
                let rd = ((r >> 7) & 0b111) as u8 + 8;
                let funct2 = (raw >> 10) & 0b11;
                if funct2 == 0b00 {
                    let shamt = ((r >> 12) & 1) << 5 | ((r >> 2) & 0b11111);
                    Some(Instr::Srli { rd, rs1: rd, shamt: shamt as u8 })
                } else if funct2 == 0b01 {
                    let shamt = ((r >> 12) & 1) << 5 | ((r >> 2) & 0b11111);
                    Some(Instr::Srai { rd, rs1: rd, shamt: shamt as u8 })
                } else if funct2 == 0b10 {
                    let imm = sext(((r >> 12) & 1) << 5 | ((r >> 2) & 0b11111), 6);
                    Some(Instr::Andi { rd, rs1: rd, imm })
                } else {
                    let rs2 = ((r >> 2) & 0b111) as u8 + 8;
                    let funct6 = (raw >> 5) & 0b11;
                    if (raw >> 12) & 1 == 0 {
                        match funct6 {
                            0b00 => Some(Instr::Sub { rd, rs1: rd, rs2 }),
                            0b01 => Some(Instr::Xor { rd, rs1: rd, rs2 }),
                            0b10 => Some(Instr::Or  { rd, rs1: rd, rs2 }),
                            0b11 => Some(Instr::And { rd, rs1: rd, rs2 }),
                            _ => None,
                        }
                    } else {
                        match funct6 {
                            0b00 => Some(Instr::Subw { rd, rs1: rd, rs2 }),
                            0b01 => Some(Instr::Addw { rd, rs1: rd, rs2 }),
                            _ => None,
                        }
                    }
                }
            }
            0b101 => Some(Instr::Jal { rd: 0, imm: cj_imm(raw) }),
            0b110 => {
                let rs1 = ((r >> 7) & 0b111) as u8 + 8;
                Some(Instr::Beq { rs1, rs2: 0, imm: cb_imm(raw) })
            }
            0b111 => {
                let rs1 = ((r >> 7) & 0b111) as u8 + 8;
                Some(Instr::Bne { rs1, rs2: 0, imm: cb_imm(raw) })
            }
            _ => None,
        },
        0b10 => match f3 {
            0b000 => {
                let rd = ((r >> 7) & 0b11111) as u8;
                let shamt = ((r >> 12) & 1) << 5 | ((r >> 2) & 0b11111);
                Some(Instr::Slli { rd, rs1: rd, shamt: shamt as u8 })
            }
            0b001 => {
                let rd = ((r >> 7) & 0b11111) as u8;
                let imm = ((r >> 12) & 1) << 5
                        | ((r >> 5) & 0b11) << 3
                        | ((r >> 2) & 0b111) << 6;
                Some(Instr::Fld { rd, rs1: 2, imm: imm as i64 })
            }
            0b010 => {
                let rd = ((r >> 7) & 0b11111) as u8;
                if rd == 0 { return None; }
                let imm = ((r >> 12) & 1) << 5
                        | ((r >> 4) & 0b111) << 2
                        | ((r >> 2) & 0b11) << 6;
                Some(Instr::Lw { rd, rs1: 2, imm: imm as i64 })
            }
            0b011 => {
                let rd = ((r >> 7) & 0b11111) as u8;
                if rd == 0 { return None; }
                let imm = ((r >> 12) & 1) << 5
                        | ((r >> 5) & 0b11) << 3
                        | ((r >> 2) & 0b111) << 6;
                Some(Instr::Ld { rd, rs1: 2, imm: imm as i64 })
            }
            0b100 => {
                let rd = ((r >> 7) & 0b11111) as u8;
                let rs2 = ((r >> 2) & 0b11111) as u8;
                if (raw >> 12) & 1 == 0 {
                    if rs2 == 0 {
                        if rd == 0 { return None; }
                        Some(Instr::Jalr { rd: 0, rs1: rd, imm: 0 })
                    } else {
                        Some(Instr::Add { rd, rs1: 0, rs2 })
                    }
                } else {
                    if rs2 == 0 {
                        if rd == 0 {
                            Some(Instr::Ebreak)
                        } else {
                            Some(Instr::Jalr { rd: 1, rs1: rd, imm: 0 })
                        }
                    } else {
                        Some(Instr::Add { rd, rs1: rd, rs2 })
                    }
                }
            }
            0b101 => {
                let rs2 = ((r >> 2) & 0b11111) as u8;
                let imm = ((r >> 10) & 0b111) << 3
                        | ((r >> 7) & 0b111) << 6;
                Some(Instr::Fsd { rs1: 2, rs2, imm: imm as i64 })
            }
            0b110 => {
                let rs2 = ((r >> 2) & 0b11111) as u8;
                let imm = ((r >> 9) & 0b1111) << 2
                        | ((r >> 7) & 0b11) << 6;
                Some(Instr::Sw { rs1: 2, rs2, imm: imm as i64 })
            }
            0b111 => {
                let rs2 = ((r >> 2) & 0b11111) as u8;
                let imm = ((r >> 10) & 0b111) << 3
                        | ((r >> 7) & 0b111) << 6;
                Some(Instr::Sd { rs1: 2, rs2, imm: imm as i64 })
            }
            _ => None,
        },
        _ => None,
    }
}

fn cj_imm(raw: u16) -> i64 {
    let imm11 = ((raw >> 12) & 1) as u32;
    let v = (imm11 << 10)
        | (((raw >> 8) & 1) as u32) << 9
        | (((raw >> 9) & 0b11) as u32) << 7
        | (((raw >> 6) & 1) as u32) << 6
        | (((raw >> 7) & 1) as u32) << 5
        | (((raw >> 2) & 1) as u32) << 4
        | (((raw >> 11) & 1) as u32) << 3
        | (((raw >> 3) & 0b111) as u32);
    if imm11 != 0 {
        ((v | 0xFFFF_F800u32) as i32 as i64) << 1
    } else {
        (v as i64) << 1
    }
}

fn cb_imm(raw: u16) -> i64 {
    let imm8 = ((raw >> 12) & 1) as u32;
    let v = (imm8 << 7)
        | (((raw >> 5) & 0b11) as u32) << 5
        | (((raw >> 2) & 1) as u32) << 4
        | (((raw >> 10) & 0b11) as u32) << 2
        | (((raw >> 3) & 0b11) as u32);
    if imm8 != 0 {
        ((v | 0xFFFF_FF00u32) as i32 as i64) << 1
    } else {
        (v as i64) << 1
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_decompress_3e00() {
        let result = decompress(0x3e00);
        assert!(result.is_some(), "decompress(0x3e00) returned None");
    }
    #[test]
    fn test_decompress_zcb_lbu_8160() {
        // 0x8160 = c.lbu x8, 3(x10)  (Zcb extension)
        let result = decompress(0x8160);
        assert!(result.is_some(), "decompress(0x8160) returned None");
        assert_eq!(result, Some(Instr::Lbu { rd: 8, rs1: 10, imm: 3 }));
    }
}
