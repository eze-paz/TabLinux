//! Instruction encoder — convert Instr enum back to raw u32 bytes
//!
//! Useful for test assembly, trap vector setup, and mini-boot code generation.

extern crate alloc;
use alloc::vec::Vec;

use crate::types::Instr;

/// Encode an instruction into its 32-bit (or 16-bit compressed) raw form.
/// Returns the raw bytes and the width (2 or 4).
pub fn encode(instr: Instr) -> Option<(u32, u8)> {
    use Instr::*;
    
    let raw = match instr {
        // -- U-Type --
        Lui { rd, imm } => {
            let imm_31_12 = ((imm as i64) >> 12) as u32 & 0xFFFFF;
            (imm_31_12 << 12) | ((rd as u32) << 7) | 0b0110111
        }
        Auipc { rd, imm } => {
            let imm_31_12 = ((imm as i64) >> 12) as u32 & 0xFFFFF;
            (imm_31_12 << 12) | ((rd as u32) << 7) | 0b0010111
        }
        
        // -- J-Type --
        Jal { rd, imm } => {
            let imm = imm as i64;
            let bit20    = ((imm >> 20) & 1) as u32;
            let bits10_1 = ((imm >> 1) & 0x3FF) as u32;
            let bit11    = ((imm >> 11) & 1) as u32;
            let bits19_12= ((imm >> 12) & 0xFF) as u32;
            let j_imm = (bit20 << 31) | (bits19_12 << 12) | (bit11 << 20) | (bits10_1 << 21);
            j_imm | ((rd as u32) << 7) | 0b1101111
        }
        
        // -- I-Type --
        Jalr { rd, rs1, imm } => {
            let imm_11_0 = (imm as i64) as u32 & 0xFFF;
            (imm_11_0 << 20) | ((rs1 as u32) << 15) | ((rd as u32) << 7) | 0b1100111
        }
        Lb { rd, rs1, imm } => i_type(imm, rs1, 0b000, rd, 0b0000011),
        Lh { rd, rs1, imm } => i_type(imm, rs1, 0b001, rd, 0b0000011),
        Lw { rd, rs1, imm } => i_type(imm, rs1, 0b010, rd, 0b0000011),
        Ld { rd, rs1, imm } => i_type(imm, rs1, 0b011, rd, 0b0000011),
        Lbu { rd, rs1, imm } => i_type(imm, rs1, 0b100, rd, 0b0000011),
        Lhu { rd, rs1, imm } => i_type(imm, rs1, 0b101, rd, 0b0000011),
        Lwu { rd, rs1, imm } => i_type(imm, rs1, 0b110, rd, 0b0000011),
        Fld { rd, rs1, imm } => i_type(imm, rs1, 0b011, rd, 0b0000111),
        Addi { rd, rs1, imm } => i_type(imm, rs1, 0b000, rd, 0b0010011),
        Slti { rd, rs1, imm } => i_type(imm, rs1, 0b010, rd, 0b0010011),
        Sltiu { rd, rs1, imm } => i_type(imm, rs1, 0b011, rd, 0b0010011),
        Xori { rd, rs1, imm } => i_type(imm, rs1, 0b100, rd, 0b0010011),
        Ori { rd, rs1, imm } => i_type(imm, rs1, 0b110, rd, 0b0010011),
        Andi { rd, rs1, imm } => i_type(imm, rs1, 0b111, rd, 0b0010011),
        Slli { rd, rs1, shamt } => {
            ((0b000000 as u32) << 26) | ((shamt as u32) << 20) | ((rs1 as u32) << 15) | (0b001 << 12) | ((rd as u32) << 7) | 0b0010011
        }
        Srli { rd, rs1, shamt } => {
            ((0b000000 as u32) << 26) | ((shamt as u32) << 20) | ((rs1 as u32) << 15) | (0b101 << 12) | ((rd as u32) << 7) | 0b0010011
        }
        Srai { rd, rs1, shamt } => {
            ((0b010000 as u32) << 26) | ((shamt as u32) << 20) | ((rs1 as u32) << 15) | (0b101 << 12) | ((rd as u32) << 7) | 0b0010011
        }
        Addiw { rd, rs1, imm } => i_type(imm, rs1, 0b000, rd, 0b0011011),
        Slliw { rd, rs1, shamt } => {
            ((0b0000000 as u32) << 25) | ((shamt as u32) << 20) | ((rs1 as u32) << 15) | (0b001 << 12) | ((rd as u32) << 7) | 0b0011011
        }
        Srliw { rd, rs1, shamt } => {
            ((0b0000000 as u32) << 25) | ((shamt as u32) << 20) | ((rs1 as u32) << 15) | (0b101 << 12) | ((rd as u32) << 7) | 0b0011011
        }
        Sraiw { rd, rs1, shamt } => {
            ((0b0100000 as u32) << 25) | ((shamt as u32) << 20) | ((rs1 as u32) << 15) | (0b101 << 12) | ((rd as u32) << 7) | 0b0011011
        }
        
        // -- B-Type --
        Beq { rs1, rs2, imm } => b_type(imm, rs2, rs1, 0b000),
        Bne { rs1, rs2, imm } => b_type(imm, rs2, rs1, 0b001),
        Blt { rs1, rs2, imm } => b_type(imm, rs2, rs1, 0b100),
        Bge { rs1, rs2, imm } => b_type(imm, rs2, rs1, 0b101),
        Bltu { rs1, rs2, imm } => b_type(imm, rs2, rs1, 0b110),
        Bgeu { rs1, rs2, imm } => b_type(imm, rs2, rs1, 0b111),
        
        // -- S-Type --
        Sb { rs1, rs2, imm } => s_type(imm, rs2, rs1, 0b000),
        Sh { rs1, rs2, imm } => s_type(imm, rs2, rs1, 0b001),
        Sw { rs1, rs2, imm } => s_type(imm, rs2, rs1, 0b010),
        Sd { rs1, rs2, imm } => s_type(imm, rs2, rs1, 0b011),
        Fsd { rs1, rs2, imm } => s_type(imm, rs2, rs1, 0b011),
        
        // -- R-Type --
        Add { rd, rs1, rs2 } => r_type(0b0000000, rs2, rs1, 0b000, rd, 0b0110011),
        Sub { rd, rs1, rs2 } => r_type(0b0100000, rs2, rs1, 0b000, rd, 0b0110011),
        Sll { rd, rs1, rs2 } => r_type(0b0000000, rs2, rs1, 0b001, rd, 0b0110011),
        Slt { rd, rs1, rs2 } => r_type(0b0000000, rs2, rs1, 0b010, rd, 0b0110011),
        Sltu { rd, rs1, rs2 } => r_type(0b0000000, rs2, rs1, 0b011, rd, 0b0110011),
        Xor { rd, rs1, rs2 } => r_type(0b0000000, rs2, rs1, 0b100, rd, 0b0110011),
        Srl { rd, rs1, rs2 } => r_type(0b0000000, rs2, rs1, 0b101, rd, 0b0110011),
        Sra { rd, rs1, rs2 } => r_type(0b0100000, rs2, rs1, 0b101, rd, 0b0110011),
        Or { rd, rs1, rs2 } => r_type(0b0000000, rs2, rs1, 0b110, rd, 0b0110011),
        And { rd, rs1, rs2 } => r_type(0b0000000, rs2, rs1, 0b111, rd, 0b0110011),
        
        // -- R-Type M extension --
        Mul { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b000, rd, 0b0110011),
        Mulh { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b001, rd, 0b0110011),
        Mulhsu { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b010, rd, 0b0110011),
        Mulhu { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b011, rd, 0b0110011),
        Div { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b100, rd, 0b0110011),
        Divu { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b101, rd, 0b0110011),
        Rem { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b110, rd, 0b0110011),
        Remu { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b111, rd, 0b0110011),
        
        // -- R-Type W extension --
        Addw { rd, rs1, rs2 } => r_type(0b0000000, rs2, rs1, 0b000, rd, 0b0111011),
        Subw { rd, rs1, rs2 } => r_type(0b0100000, rs2, rs1, 0b000, rd, 0b0111011),
        Sllw { rd, rs1, rs2 } => r_type(0b0000000, rs2, rs1, 0b001, rd, 0b0111011),
        Srlw { rd, rs1, rs2 } => r_type(0b0000000, rs2, rs1, 0b101, rd, 0b0111011),
        Sraw { rd, rs1, rs2 } => r_type(0b0100000, rs2, rs1, 0b101, rd, 0b0111011),
        Mulw { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b000, rd, 0b0111011),
        Divw { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b100, rd, 0b0111011),
        Divuw { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b101, rd, 0b0111011),
        Remw { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b110, rd, 0b0111011),
        Remuw { rd, rs1, rs2 } => r_type(0b0000001, rs2, rs1, 0b111, rd, 0b0111011),
        
        // -- System --
        Ecall => 0x00000073,
        Ebreak => 0x00100073,
        Mret => 0x30200073,
        Sret => 0x10200073,
        Uret => 0x00200073,
        Wfi => 0x10500073,
        
        // -- Fence --
        Fence { pred, succ } => {
            ((pred as u32) << 20) | ((succ as u32) << 24) | 0b0001111
        }
        FenceI => 0x0000100F,
        
        // -- Zicsr --
        Csrrw { rd, rs1, csr } => csr_type(csr, rs1, 0b001, rd),
        Csrrs { rd, rs1, csr } => csr_type(csr, rs1, 0b010, rd),
        Csrrc { rd, rs1, csr } => csr_type(csr, rs1, 0b011, rd),
        Csrrwi { rd, zimm, csr } => csr_type(csr, zimm, 0b101, rd),
        Csrrsi { rd, zimm, csr } => csr_type(csr, zimm, 0b110, rd),
        Csrrci { rd, zimm, csr } => csr_type(csr, zimm, 0b111, rd),
        
        // -- SFENCE.VMA --
        SfenceVma { rs1, rs2 } => {
            ((rs2 as u32) << 20) | ((rs1 as u32) << 15) | (0b000 << 12) | (0b0001001 << 25) | 0b1110011
        }
        
        // -- AMO (not yet implemented for encoding) --
        _ => return None,
    };
    
    Some((raw, 4))
}

/// Encode a 32-bit instruction and return as little-endian bytes.
pub fn encode_bytes(instr: Instr) -> Option<([u8; 4], u8)> {
    encode(instr).map(|(raw, width)| (raw.to_le_bytes(), width))
}

/// Assemble a slice of instructions into a byte vector (little-endian).
pub fn assemble(instrs: &[Instr]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for &instr in instrs {
        if let Some((raw, width)) = encode(instr) {
            if width == 2 {
                bytes.extend_from_slice(&(raw as u16).to_le_bytes());
            } else {
                bytes.extend_from_slice(&raw.to_le_bytes());
            }
        }
    }
    bytes
}

// -- Helper constructors for common instructions --

/// `addi rd, rs1, imm`
pub fn addi(rd: u8, rs1: u8, imm: i64) -> Instr {
    Instr::Addi { rd, rs1, imm }
}

/// `li rd, imm` (expanded to addi if small, or lui+addi if large)
pub fn li(rd: u8, imm: i64) -> Vec<Instr> {
    let mut v = Vec::new();
    if imm >= -2048 && imm < 2048 {
        v.push(Instr::Addi { rd, rs1: 0, imm });
    } else {
        let hi = (imm as i32 >> 12) as i64;
        let lo = ((imm as i32) << 20 >> 20) as i64;
        v.push(Instr::Lui { rd, imm: (hi as u64) << 12 });
        if lo != 0 {
            v.push(Instr::Addi { rd, rs1: rd, imm: lo });
        }
    }
    v
}

/// `mv rd, rs` (= addi rd, rs, 0)
pub fn mv(rd: u8, rs: u8) -> Instr {
    Instr::Addi { rd, rs1: rs, imm: 0 }
}

/// `nop` (= addi x0, x0, 0)
pub fn nop() -> Instr {
    Instr::Addi { rd: 0, rs1: 0, imm: 0 }
}

/// `j offset` (= jal x0, offset)
pub fn j(imm: i64) -> Instr {
    Instr::Jal { rd: 0, imm }
}

/// `jr rs1` (= jalr x0, rs1, 0)
pub fn jr(rs1: u8) -> Instr {
    Instr::Jalr { rd: 0, rs1, imm: 0 }
}

/// `ret` (= jalr x0, x1, 0)
pub fn ret() -> Instr {
    Instr::Jalr { rd: 0, rs1: 1, imm: 0 }
}

/// `ecall`
pub fn ecall() -> Instr {
    Instr::Ecall
}

/// `ebreak`
pub fn ebreak() -> Instr {
    Instr::Ebreak
}

/// `mret`
pub fn mret() -> Instr {
    Instr::Mret
}

/// `sret`
pub fn sret() -> Instr {
    Instr::Sret
}

/// `wfi`
pub fn wfi() -> Instr {
    Instr::Wfi
}

/// `csrr rd, csr` (= csrrs rd, csr, x0)
pub fn csrr(rd: u8, csr: u16) -> Instr {
    Instr::Csrrs { rd, rs1: 0, csr }
}

/// `csrw csr, rs1` (= csrrw x0, csr, rs1)
pub fn csrw(csr: u16, rs1: u8) -> Instr {
    Instr::Csrrw { rd: 0, rs1, csr }
}

// -- Internal format helpers --

fn i_type(imm: i64, rs1: u8, funct3: u8, rd: u8, opcode: u32) -> u32 {
    let imm_11_0 = (imm as i64) as u32 & 0xFFF;
    (imm_11_0 << 20) | ((rs1 as u32) << 15) | ((funct3 as u32) << 12) | ((rd as u32) << 7) | opcode
}

fn s_type(imm: i64, rs2: u8, rs1: u8, funct3: u8) -> u32 {
    let imm_11_0 = (imm as i64) as u32 & 0xFFF;
    let imm_hi = (imm_11_0 >> 5) & 0x7F;
    let imm_lo = imm_11_0 & 0x1F;
    (imm_hi << 25) | ((rs2 as u32) << 20) | ((rs1 as u32) << 15) | ((funct3 as u32) << 12) | (imm_lo << 7) | 0b0100011
}

fn b_type(imm: i64, rs2: u8, rs1: u8, funct3: u8) -> u32 {
    let imm = imm as i64;
    let bit12   = ((imm >> 12) & 1) as u32;
    let bits10_5= ((imm >> 5) & 0x3F) as u32;
    let bits4_1 = ((imm >> 1) & 0xF) as u32;
    let bit11   = ((imm >> 11) & 1) as u32;
    (bit12 << 31) | (bits10_5 << 25) | (bit11 << 7) | (bits4_1 << 8) |
    ((rs2 as u32) << 20) | ((rs1 as u32) << 15) | ((funct3 as u32) << 12) | 0b1100011
}

fn r_type(funct7: u32, rs2: u8, rs1: u8, funct3: u8, rd: u8, opcode: u32) -> u32 {
    (funct7 << 25) | ((rs2 as u32) << 20) | ((rs1 as u32) << 15) | ((funct3 as u32) << 12) | ((rd as u32) << 7) | opcode
}

fn csr_type(csr: u16, rs1: u8, funct3: u8, rd: u8) -> u32 {
    ((csr as u32) << 20) | ((rs1 as u32) << 15) | ((funct3 as u32) << 12) | ((rd as u32) << 7) | 0b1110011
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode;

    fn instr_eq(a: &Instr, b: &Instr) -> bool {
        std::format!("{:?}", a) == std::format!("{:?}", b)
    }

    #[test]
    fn roundtrip_addi() {
        let instr = Instr::Addi { rd: 1, rs1: 2, imm: 42 };
        let (raw, _) = encode(instr).unwrap();
        let decoded = decode(raw);
        assert!(instr_eq(&decoded, &instr));
    }

    #[test]
    fn roundtrip_jal() {
        let instr = Instr::Jal { rd: 1, imm: 0x1234 };
        let (raw, _) = encode(instr).unwrap();
        let decoded = decode(raw);
        assert!(instr_eq(&decoded, &instr));
    }

    #[test]
    fn roundtrip_beq() {
        let instr = Instr::Beq { rs1: 1, rs2: 2, imm: 0x100 };
        let (raw, _) = encode(instr).unwrap();
        let decoded = decode(raw);
        assert!(instr_eq(&decoded, &instr));
    }

    #[test]
    fn roundtrip_store() {
        let instr = Instr::Sw { rs1: 2, rs2: 3, imm: 0x80 };
        let (raw, _) = encode(instr).unwrap();
        let decoded = decode(raw);
        assert!(instr_eq(&decoded, &instr));
    }

    #[test]
    fn roundtrip_ecall() {
        let instr = Instr::Ecall;
        let (raw, _) = encode(instr).unwrap();
        let decoded = decode(raw);
        assert!(instr_eq(&decoded, &instr));
    }

    #[test]
    fn roundtrip_csr() {
        let instr = Instr::Csrrw { rd: 1, rs1: 2, csr: 0x305 };
        let (raw, _) = encode(instr).unwrap();
        let decoded = decode(raw);
        assert!(instr_eq(&decoded, &instr));
    }
    
    #[test]
    fn test_li_small() {
        let v = li(1, 42);
        assert_eq!(v.len(), 1);
    }
    
    #[test]
    fn test_li_large() {
        let v = li(1, 0x12345);
        assert_eq!(v.len(), 2);
    }
    
    #[test]
    fn test_assemble() {
        let bytes = assemble(&[
            addi(1, 0, 42),
            ecall(),
        ]);
        assert_eq!(bytes.len(), 8);
        // First 4 bytes should be addi x1, x0, 42
        let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert!(matches!(decode(raw), Instr::Addi { rd: 1, rs1: 0, imm: 42 }));
    }
}
