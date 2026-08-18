// Reference decodes verified with riscv-none-elf-objdump -M no-aliases (GCC 15.2 toolchain).
use riscv_core::compressed::decompress;
use riscv_core::types::Instr;

#[test]
fn c_sdsp_css_fields() {
    // e022 = c.sdsp s0,0(sp) ; e406 = c.sdsp ra,8(sp) ; e0a2 = c.sdsp s0,64(sp)
    assert_eq!(decompress(0xe022), Some(Instr::Sd { rs1: 2, rs2: 8, imm: 0 }));
    assert_eq!(decompress(0xe406), Some(Instr::Sd { rs1: 2, rs2: 1, imm: 8 }));
    assert_eq!(decompress(0xe0a2), Some(Instr::Sd { rs1: 2, rs2: 8, imm: 64 }));
}

#[test]
fn c_ldsp_ci_fields() {
    // 6086 = c.ldsp ra,64(sp) ; 6622 = c.ldsp a2,8(sp)
    assert_eq!(decompress(0x6086), Some(Instr::Ld { rd: 1, rs1: 2, imm: 64 }));
    assert_eq!(decompress(0x6622), Some(Instr::Ld { rd: 12, rs1: 2, imm: 8 }));
}

#[test]
fn c_cl_format_fields() {
    // objdump: 618c = c.ld a1,0(a1) ; 4188 = c.lw a0,0(a1)
    // Guards rs1'=bits[9:7], rd'=bits[4:2] against off-by-one "fixes".
    assert_eq!(decompress(0x618c), Some(Instr::Ld { rd: 11, rs1: 11, imm: 0 }));
    assert_eq!(decompress(0x4188), Some(Instr::Lw { rd: 10, rs1: 11, imm: 0 }));
}

#[test]
fn c3_family_csdsp_cfldsp() {
    // q=11 (C3 family) instructions - C.SDSP and C.FSDSP
    // From RISC-V RVC spec C3 quadrant
    
    // objdump: 4086 = c.lwsp ra,64(sp). (0x6006 is C.LDSP rd=0 = RESERVED -> None.)
    assert_eq!(
        decompress(0x4086),
        Some(Instr::Lw { rd: 1, rs1: 2, imm: 64 })
    );
    assert_eq!(decompress(0x6006), None);
    
    // C.LDSP: 0x6622 = c.ldsp a2, 8(sp)
    assert_eq!(
        decompress(0x6622),
        Some(Instr::Ld { rd: 12, rs1: 2, imm: 8 })
    );
    
    // objdump: 6606 = c.ldsp a2,64(sp)  (funct3=011 is LDSP, not FLDSP)
    assert_eq!(
        decompress(0x6606),
        Some(Instr::Ld { rd: 12, rs1: 2, imm: 64 })
    );
    
    // C.SDSP: 0xe022 = c.sdsp s0, 0(sp)
    assert_eq!(
        decompress(0xe022),
        Some(Instr::Sd { rs1: 2, rs2: 8, imm: 0 })
    );
    
    // C.SDSP: 0xe406 = c.sdsp ra, 8(sp)
    assert_eq!(
        decompress(0xe406),
        Some(Instr::Sd { rs1: 2, rs2: 1, imm: 8 })
    );
    
    // objdump: e422 = c.sdsp s0,8(sp)  (funct3=111 is SDSP, not FSDSP)
    assert_eq!(
        decompress(0xe422),
        Some(Instr::Sd { rs1: 2, rs2: 8, imm: 8 })
    );
}

#[test]
fn c_ca_format_fields() {
    // objdump: 8d5d = c.or a0,a5 ; 8fd9 = c.or a5,a4 (CA regs are x8+rd', x8+rs2')
    assert_eq!(decompress(0x8d5d), Some(Instr::Or { rd: 10, rs1: 10, rs2: 15 }));
    assert_eq!(decompress(0x8fd9), Some(Instr::Or { rd: 15, rs1: 15, rs2: 14 }));
    // 0x0000 is the defined-illegal instruction: must NOT decode
    assert_eq!(decompress(0x0000), None);
}

#[test]
fn s_type_imm_sign_extends() {
    use riscv_core::decode::decode;
    // objdump: fac43c23 = sd a2,-72(s0) ; fcb43c23 = sd a1,-40(s0) ; 00c53023 = sd a2,0(a0)
    assert_eq!(decode(0xfac43c23), Instr::Sd { rs1: 8, rs2: 12, imm: -72 });
    assert_eq!(decode(0xfcb43c23), Instr::Sd { rs1: 8, rs2: 11, imm: -40 });
    assert_eq!(decode(0x00c53023), Instr::Sd { rs1: 10, rs2: 12, imm: 0 });
}

#[test]
fn rv64_shift_shamt_over_31() {
    use riscv_core::decode::decode;
    // objdump: 020ada93 = srli s5,s5,0x20 ; 420ada93 = srai s5,s5,0x20 ; 02059593 = slli a1,a1,0x20
    assert_eq!(decode(0x020ada93), Instr::Srli { rd: 21, rs1: 21, shamt: 32 });
    assert_eq!(decode(0x420ada93), Instr::Srai { rd: 21, rs1: 21, shamt: 32 });
    assert_eq!(decode(0x02059593), Instr::Slli { rd: 11, rs1: 11, shamt: 32 });
}

/// SFENCE.VMA encodes rs2 (the ASID) in the low 5 bits of funct12, so it must be
/// matched on funct7 == 0b0001001, not on the exact funct12 0x120. Matching the
/// funct12 only recognised ; the ASID-scoped form the kernel
/// emits from  decoded as illegal and killed the first execve.
///
/// objdump -M no-aliases:
///   13030073  sfence.vma t1,a6
///   12630073  sfence.vma t1,t1
///   12000073  sfence.vma zero,zero
#[test]
fn sfence_vma_with_asid() {
    use riscv_core::decode::decode;
    assert_eq!(decode(0x13030073), Instr::SfenceVma { rs1: 6, rs2: 16 });
    assert_eq!(decode(0x12630073), Instr::SfenceVma { rs1: 6, rs2: 6 });
    assert_eq!(decode(0x12000073), Instr::SfenceVma { rs1: 0, rs2: 0 });
}

/// An encoding we do not implement must raise IllegalInstruction, never
/// Ebreak. The OP / OP-32 / OP-IMM-32 fallthroughs used to yield , which
/// turned every unimplemented extension into a fake BUG() and hid the real cause.
///
/// 2872de13 / 28735313 are Zbb  (objdump:  — unknown to a
/// plain rv64 disassembler), which the kernel patches in when the DTB claims Zbb.
#[test]
fn unimplemented_encodings_are_illegal_not_ebreak() {
    use riscv_core::decode::decode;
    for raw in [
        0x2872de13u32, // orc.b t3,t0   (OP-IMM, funct7 0b0010100)
        0x28735313,    // orc.b t1,t1
        0x0a86c033,    // OP with funct7 0b0000101 (min/max/clmul family)
        0x0a86c03b,    // OP-32 with the same unknown funct7
        0x1005d09b,    // OP-IMM-32 shift with an unknown funct7
    ] {
        assert_eq!(decode(raw), Instr::Unimp, "raw {raw:#010x} should decode as Unimp");
    }
}
