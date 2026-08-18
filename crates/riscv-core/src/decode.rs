//! Instruction decoder — RISC-V RV64IMA + RVC decompression
//!
//! Decodes raw u32 → Instr enum. No memory access, no side effects.

use crate::types::Instr;
pub use crate::compressed::decompress;

/// Decode a single 32-bit RISC-V instruction
pub fn decode(raw: u32) -> Instr {
    let opcode = raw & 0x7F;
    let rd     = ((raw >> 7) & 0x1F) as u8;
    let funct3 = ((raw >> 12) & 0x07) as u8;
    let rs1    = ((raw >> 15) & 0x1F) as u8;
    let rs2    = ((raw >> 20) & 0x1F) as u8;
    let funct7 = ((raw >> 25) & 0x7F) as u8;

    match opcode {
        0b0110111 => Instr::Lui { rd, imm: (((raw & 0xFFFFF000) as i32) as i64) as u64 },
        0b0010111 => Instr::Auipc { rd, imm: (((raw & 0xFFFFF000) as i32) as i64) as u64 },
        0b1101111 => {
            let imm = j_imm(raw);
            Instr::Jal { rd, imm }
        }
        0b1100111 => {
            Instr::Jalr { rd, rs1, imm: i_imm(raw) }
        }
        0b1100011 => {
            let imm = b_imm(raw);
            match funct3 {
                0b000 => Instr::Beq { rs1, rs2, imm },
                0b001 => Instr::Bne { rs1, rs2, imm },
                0b100 => Instr::Blt { rs1, rs2, imm },
                0b101 => Instr::Bge { rs1, rs2, imm },
                0b110 => Instr::Bltu { rs1, rs2, imm },
                0b111 => Instr::Bgeu { rs1, rs2, imm },
                _ => Instr::Unimp, // illegal
            }
        }
        0b0000011 => {
            let imm = i_imm(raw);
            match funct3 {
                0b000 => Instr::Lb  { rd, rs1, imm },
                0b001 => Instr::Lh  { rd, rs1, imm },
                0b010 => Instr::Lw  { rd, rs1, imm },
                0b011 => Instr::Ld  { rd, rs1, imm },
                0b100 => Instr::Lbu { rd, rs1, imm },
                0b101 => Instr::Lhu { rd, rs1, imm },
                0b110 => Instr::Lwu { rd, rs1, imm },
                _ => Instr::Unimp,
            }
        }
        0b0000111 => {
            let imm = i_imm(raw);
            match funct3 {
                0b010 => Instr::Flw { rd, rs1, imm },
                0b011 => Instr::Fld { rd, rs1, imm },
                _ => Instr::Unimp,
            }
        }
        0b0100011 => {
            let imm = s_imm(raw);
            match funct3 {
                0b000 => Instr::Sb { rs1, rs2, imm },
                0b001 => Instr::Sh { rs1, rs2, imm },
                0b010 => Instr::Sw { rs1, rs2, imm },
                0b011 => Instr::Sd { rs1, rs2, imm },
                _ => Instr::Unimp,
            }
        }
        0b0100111 => {
            let imm = s_imm(raw);
            match funct3 {
                0b010 => Instr::Fsw { rs1, rs2, imm },
                0b011 => Instr::Fsd { rs1, rs2, imm },
                _ => Instr::Unimp,
            }
        }
        0b0010011 => {
            let imm = i_imm(raw);
            match funct3 {
                0b000 => Instr::Addi { rd, rs1, imm },
                0b010 => Instr::Slti { rd, rs1, imm },
                0b011 => Instr::Sltiu { rd, rs1, imm },
                0b100 => Instr::Xori { rd, rs1, imm },
                0b110 => Instr::Ori  { rd, rs1, imm },
                0b111 => Instr::Andi { rd, rs1, imm },
                0b001 => {
                    let shamt = ((raw >> 20) & 0x3F) as u8;
                    Instr::Slli { rd, rs1, shamt }
                }
                0b101 => {
                    // RV64: shamt is 6 bits; funct7 bit 0 is shamt[5]. Match on
                    // funct6 (funct7 >> 1), not the full funct7, or shamt >= 32
                    // becomes an illegal instruction.
                    let shamt = ((raw >> 20) & 0x3F) as u8;
                    match funct7 >> 1 {
                        0b000000 => Instr::Srli { rd, rs1, shamt },
                        0b010000 => Instr::Srai { rd, rs1, shamt },
                        _ => Instr::Unimp,
                    }
                }
                _ => Instr::Unimp,
            }
        }
        0b0110011 => {
            if funct7 == 0b0000001 {
                // RV64M
                match funct3 {
                    0b000 => Instr::Mul    { rd, rs1, rs2 },
                    0b001 => Instr::Mulh   { rd, rs1, rs2 },
                    0b010 => Instr::Mulhsu { rd, rs1, rs2 },
                    0b011 => Instr::Mulhu  { rd, rs1, rs2 },
                    0b100 => Instr::Div    { rd, rs1, rs2 },
                    0b101 => Instr::Divu   { rd, rs1, rs2 },
                    0b110 => Instr::Rem    { rd, rs1, rs2 },
                    0b111 => Instr::Remu   { rd, rs1, rs2 },
                    _ => Instr::Unimp,
                }
            } else if funct7 == 0b0000000 {
                match funct3 {
                    0b000 => Instr::Add { rd, rs1, rs2 },
                    0b001 => Instr::Sll { rd, rs1, rs2 },
                    0b010 => Instr::Slt { rd, rs1, rs2 },
                    0b011 => Instr::Sltu { rd, rs1, rs2 },
                    0b100 => Instr::Xor { rd, rs1, rs2 },
                    0b101 => Instr::Srl { rd, rs1, rs2 },
                    0b110 => Instr::Or  { rd, rs1, rs2 },
                    0b111 => Instr::And { rd, rs1, rs2 },
                    _ => Instr::Unimp,
                }
            } else if funct7 == 0b0100000 {
                match funct3 {
                    0b000 => Instr::Sub { rd, rs1, rs2 },
                    0b101 => Instr::Sra { rd, rs1, rs2 },
                    _ => Instr::Unimp,
                }
            } else {
                // Unknown funct7 is an ILLEGAL instruction, not a breakpoint.
                // Decoding it as Ebreak turned every unimplemented extension into
                // a fake BUG() and hid the real cause.
                Instr::Unimp
            }
        }
        0b0101111 => {
            // AMO operations (RV64A) -- opcode 0x2F
            let funct5 = ((raw >> 27) & 0x1F) as u8;
            let aq = ((raw >> 26) & 1) != 0;
            let rl = ((raw >> 25) & 1) != 0;
            match funct3 {
                0b010 => {
                    match funct5 {
                        0b00010 => Instr::Lrw { rd, rs1, aq, rl },
                        0b00011 => Instr::Scw { rd, rs1, rs2, aq, rl },
                        0b00001 => Instr::Amoswapw { rd, rs1, rs2, aq, rl },
                        0b00000 => Instr::Amoaddw  { rd, rs1, rs2, aq, rl },
                        0b00100 => Instr::Amoxorw  { rd, rs1, rs2, aq, rl },
                        0b01100 => Instr::Amoandw  { rd, rs1, rs2, aq, rl },
                        0b01000 => Instr::Amoorw   { rd, rs1, rs2, aq, rl },
                        0b10000 => Instr::Amominw  { rd, rs1, rs2, aq, rl },
                        0b10100 => Instr::Amomaxw  { rd, rs1, rs2, aq, rl },
                        0b11000 => Instr::Amominuw { rd, rs1, rs2, aq, rl },
                        0b11100 => Instr::Amomaxuw { rd, rs1, rs2, aq, rl },
                        _ => Instr::Unimp,
                    }
                }
                0b011 => {
                    match funct5 {
                        0b00010 => Instr::Lrd { rd, rs1, aq, rl },
                        0b00011 => Instr::Scd { rd, rs1, rs2, aq, rl },
                        0b00001 => Instr::Amoswapd { rd, rs1, rs2, aq, rl },
                        0b00000 => Instr::Amoaddd  { rd, rs1, rs2, aq, rl },
                        0b00100 => Instr::Amoxord  { rd, rs1, rs2, aq, rl },
                        0b01100 => Instr::Amoandd  { rd, rs1, rs2, aq, rl },
                        0b01000 => Instr::Amoord   { rd, rs1, rs2, aq, rl },
                        0b10000 => Instr::Amomind  { rd, rs1, rs2, aq, rl },
                        0b10100 => Instr::Amomaxd  { rd, rs1, rs2, aq, rl },
                        0b11000 => Instr::Amominud { rd, rs1, rs2, aq, rl },
                        0b11100 => Instr::Amomaxud { rd, rs1, rs2, aq, rl },
                        _ => Instr::Unimp,
                    }
                }
                _ => Instr::Unimp,
            }
        }
        0b0011011 => {
            let imm = i_imm_sext5(raw); // sign-extend 5-bit immediate for addiw
            match funct3 {
                0b000 => Instr::Addiw { rd, rs1, imm },
                0b001 => {
                    let shamt = ((raw >> 20) & 0x1F) as u8;
                    Instr::Slliw { rd, rs1, shamt }
                }
                0b101 => {
                    let shamt = ((raw >> 20) & 0x1F) as u8;
                    if funct7 == 0b0000000 {
                        Instr::Srliw { rd, rs1, shamt }
                    } else if funct7 == 0b0100000 {
                        Instr::Sraiw { rd, rs1, shamt }
                    } else {
                        Instr::Unimp
                    }
                }
                _ => Instr::Unimp,
            }
        }
        0b0111011 => {
            if funct7 == 0b0000001 {
                match funct3 {
                    0b000 => Instr::Mulw  { rd, rs1, rs2 },
                    0b100 => Instr::Divw  { rd, rs1, rs2 },
                    0b101 => Instr::Divuw { rd, rs1, rs2 },
                    0b110 => Instr::Remw  { rd, rs1, rs2 },
                    0b111 => Instr::Remuw { rd, rs1, rs2 },
                    _ => Instr::Unimp,
                }
            } else if funct7 == 0b0000000 {
                match funct3 {
                    0b000 => Instr::Addw { rd, rs1, rs2 },
                    0b001 => Instr::Sllw { rd, rs1, rs2 },
                    0b101 => Instr::Srlw { rd, rs1, rs2 },
                    _ => Instr::Unimp,
                }
            } else if funct7 == 0b0100000 {
                match funct3 {
                    0b000 => Instr::Subw { rd, rs1, rs2 },
                    0b101 => Instr::Sraw { rd, rs1, rs2 },
                    _ => Instr::Unimp,
                }
            } else {
                // NOTE: there used to be an `funct7 == 0b0000101 => LR/SC` arm here.
                // LR/SC live in the AMO major opcode 0b0101111, never in OP-32
                // (0b0111011); that arm could only ever misdecode something else.
                Instr::Unimp
            }
        }
        0b0001111 => {
            match funct3 {
                0b000 => Instr::Fence { pred: ((raw >> 20) & 0x0F) as u8, succ: ((raw >> 24) & 0x0F) as u8 },
                0b001 => Instr::FenceI,
                _ => Instr::Unimp,
            }
        }
        0b1110011 => {
            let csr = ((raw >> 20) & 0xFFF) as u16;
            match funct3 {
                0b000 => {
                    // Privileged / trap-return ops are keyed on funct12 (bits[31:20]),
                    // EXCEPT SFENCE.VMA, which encodes rs2 in the low 5 bits of funct12
                    // and so must be matched on funct7 alone. Matching it on the exact
                    // funct12 0x120 only recognised `sfence.vma rs1, x0`; the kernel's
                    // ASID-scoped `sfence.vma addr, asid` (__flush_tlb_range) has
                    // rs2 != 0, decoded as illegal, and killed the first execve.
                    match funct7 {
                        0b0001001 => Instr::SfenceVma { rs1, rs2 },
                        _ => match csr {
                            0x000 => Instr::Ecall,
                            0x001 => Instr::Ebreak,
                            0x002 => Instr::Uret,
                            0x102 => Instr::Sret,
                            0x105 => Instr::Wfi, // WFI: funct12=0x105, rd=0, rs1=0, funct3=0
                            0x302 => Instr::Mret,
                            _ => Instr::Unimp,
                        },
                    }
                }
                0b001 => Instr::Csrrw  { rd, rs1, csr },
                0b010 => Instr::Csrrs  { rd, rs1, csr },
                0b011 => Instr::Csrrc  { rd, rs1, csr },
                0b101 => Instr::Csrrwi { rd, zimm: rs1, csr },
                0b110 => Instr::Csrrsi { rd, zimm: rs1, csr },
                0b111 => Instr::Csrrci { rd, zimm: rs1, csr },
                _ => Instr::Unimp,
            }
        }
        // RV64F/D: OP-FP plus the four fused multiply-add forms. Alpine
        // userspace is built for rv64gc, so these are not optional — musl's
        // printf and busybox's sleep/dd all execute them.
        0b1010011 | 0b1000011 | 0b1000111 | 0b1001011 | 0b1001111 => Instr::Fp { raw },
        _ => Instr::Unimp,
    }
}

// --- Immediate extraction helpers ---

fn i_imm(raw: u32) -> i64 {
    ((raw as i32) >> 20) as i64
}

fn i_imm_sext5(raw: u32) -> i64 {
    ((raw as i32) >> 20) as i64
}

fn s_imm(raw: u32) -> i64 {
    let hi = ((raw >> 25) & 0x7F) as i64;
    let lo = ((raw >> 7) & 0x1F) as i64;
    let val = (hi << 5) | lo;
    (val << 52) >> 52 // sign-extend 12-bit
}

fn b_imm(raw: u32) -> i64 {
    let bit12 = ((raw >> 31) & 1) as i64;
    let bits10_5 = ((raw >> 25) & 0x3F) as i64;
    let bits4_1 = ((raw >> 8) & 0x0F) as i64;
    let bit11 = ((raw >> 7) & 1) as i64;
    let val = (bit12 << 12) | (bit11 << 11) | (bits10_5 << 5) | (bits4_1 << 1);
    (val << 51) >> 51 // sign-extend 13-bit
}

fn j_imm(raw: u32) -> i64 {
    let bit20    = ((raw >> 31) & 1) as i64;
    let bits10_1 = ((raw >> 21) & 0x3FF) as i64;
    let bit11    = ((raw >> 20) & 1) as i64;
    let bits19_12= ((raw >> 12) & 0xFF) as i64;
    let val = (bit20 << 20) | (bits19_12 << 12) | (bit11 << 11) | (bits10_1 << 1);
    (val << 43) >> 43 // sign-extend 21-bit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_addi() {
        // addi x1, x2, 42
        let raw: u32 = (42 << 20) | (2 << 15) | (0b000 << 12) | (1 << 7) | 0b0010011;
        assert!(matches!(decode(raw), Instr::Addi { rd: 1, rs1: 2, imm: 42 }));
    }

    #[test]
    fn decode_add() {
        // add x3, x4, x5
        let raw: u32 = (0b0000000 << 25) | (5 << 20) | (4 << 15) | (0b000 << 12) | (3 << 7) | 0b0110011;
        assert!(matches!(decode(raw), Instr::Add { rd: 3, rs1: 4, rs2: 5 }));
    }

    #[test]
    fn decode_lui() {
        // lui x1, 0x12345
        let raw: u32 = (0x12345 << 12) | (1 << 7) | 0b0110111;
        assert!(matches!(decode(raw), Instr::Lui { rd: 1, imm: 0x12345000 }));
    }

    #[test]
    fn decode_jal() {
        // jal x0, 4 (infinite loop)
        let raw: u32 = 0x0040006F;
        assert!(matches!(decode(raw), Instr::Jal { rd: 0, imm: 4 }));
    }

    #[test]
    fn decode_mul() {
        // mul x3, x4, x5
        let raw: u32 = (0b0000001 << 25) | (5 << 20) | (4 << 15) | (0b000 << 12) | (3 << 7) | 0b0110011;
        assert!(matches!(decode(raw), Instr::Mul { rd: 3, rs1: 4, rs2: 5 }));
    }

    #[test]
    fn decode_ecall() {
        assert!(matches!(decode(0x00000073), Instr::Ecall));
    }

    #[test]
    fn decode_mret() {
        assert!(matches!(decode(0x30200073), Instr::Mret));
    }
}
