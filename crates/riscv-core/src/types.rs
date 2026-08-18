//! Core types: instruction enum, register indices, traps

/// 6 instruction formats in RISC-V, all 32-bit fixed width
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instr {
    // RV64I
    Lui   { rd: u8, imm: u64 },
    Auipc { rd: u8, imm: u64 },
    Jal   { rd: u8, imm: i64 },
    Jalr  { rd: u8, rs1: u8, imm: i64 },
    Beq   { rs1: u8, rs2: u8, imm: i64 },
    Bne   { rs1: u8, rs2: u8, imm: i64 },
    Blt   { rs1: u8, rs2: u8, imm: i64 },
    Bge   { rs1: u8, rs2: u8, imm: i64 },
    Bltu  { rs1: u8, rs2: u8, imm: i64 },
    Bgeu  { rs1: u8, rs2: u8, imm: i64 },
    Lb    { rd: u8, rs1: u8, imm: i64 },
    Lh    { rd: u8, rs1: u8, imm: i64 },
    Lw    { rd: u8, rs1: u8, imm: i64 },
    Ld    { rd: u8, rs1: u8, imm: i64 },
    Lbu   { rd: u8, rs1: u8, imm: i64 },
    Lhu   { rd: u8, rs1: u8, imm: i64 },
    Lwu   { rd: u8, rs1: u8, imm: i64 },
    Sb    { rs1: u8, rs2: u8, imm: i64 },
    Sh    { rs1: u8, rs2: u8, imm: i64 },
    Sw    { rs1: u8, rs2: u8, imm: i64 },
    Sd    { rs1: u8, rs2: u8, imm: i64 },
    Flw   { rd: u8, rs1: u8, imm: i64 },
    Fld   { rd: u8, rs1: u8, imm: i64 },
    Fsw   { rs1: u8, rs2: u8, imm: i64 },
    Fsd   { rs1: u8, rs2: u8, imm: i64 },
    Addi  { rd: u8, rs1: u8, imm: i64 },
    Slti  { rd: u8, rs1: u8, imm: i64 },
    Sltiu { rd: u8, rs1: u8, imm: i64 },
    Xori  { rd: u8, rs1: u8, imm: i64 },
    Ori   { rd: u8, rs1: u8, imm: i64 },
    Andi  { rd: u8, rs1: u8, imm: i64 },
    Slli  { rd: u8, rs1: u8, shamt: u8 },
    Srli  { rd: u8, rs1: u8, shamt: u8 },
    Srai  { rd: u8, rs1: u8, shamt: u8 },
    Add   { rd: u8, rs1: u8, rs2: u8 },
    Sub   { rd: u8, rs1: u8, rs2: u8 },
    Sll   { rd: u8, rs1: u8, rs2: u8 },
    Slt   { rd: u8, rs1: u8, rs2: u8 },
    Sltu  { rd: u8, rs1: u8, rs2: u8 },
    Xor   { rd: u8, rs1: u8, rs2: u8 },
    Srl   { rd: u8, rs1: u8, rs2: u8 },
    Sra   { rd: u8, rs1: u8, rs2: u8 },
    Or    { rd: u8, rs1: u8, rs2: u8 },
    And   { rd: u8, rs1: u8, rs2: u8 },
    // RV64I-only (W variants)
    Addiw { rd: u8, rs1: u8, imm: i64 },
    Slliw { rd: u8, rs1: u8, shamt: u8 },
    Srliw { rd: u8, rs1: u8, shamt: u8 },
    Sraiw { rd: u8, rs1: u8, shamt: u8 },
    Addw  { rd: u8, rs1: u8, rs2: u8 },
    Subw  { rd: u8, rs1: u8, rs2: u8 },
    Sllw  { rd: u8, rs1: u8, rs2: u8 },
    Srlw  { rd: u8, rs1: u8, rs2: u8 },
    Sraw  { rd: u8, rs1: u8, rs2: u8 },

    // RV64M
    Mul    { rd: u8, rs1: u8, rs2: u8 },
    Mulh   { rd: u8, rs1: u8, rs2: u8 },
    Mulhsu { rd: u8, rs1: u8, rs2: u8 },
    Mulhu  { rd: u8, rs1: u8, rs2: u8 },
    Div    { rd: u8, rs1: u8, rs2: u8 },
    Divu   { rd: u8, rs1: u8, rs2: u8 },
    Rem    { rd: u8, rs1: u8, rs2: u8 },
    Remu   { rd: u8, rs1: u8, rs2: u8 },
    Mulw   { rd: u8, rs1: u8, rs2: u8 },
    Divw   { rd: u8, rs1: u8, rs2: u8 },
    Divuw  { rd: u8, rs1: u8, rs2: u8 },
    Remw   { rd: u8, rs1: u8, rs2: u8 },
    Remuw  { rd: u8, rs1: u8, rs2: u8 },

    // RV64A — simplified: single-hart, so atomics = regular ops
    Lrw      { rd: u8, rs1: u8, aq: bool, rl: bool },
    Scw      { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amoswapw { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amoaddw  { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amoxorw  { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amoandw  { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amoorw   { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amominw  { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amomaxw  { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amominuw { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amomaxuw { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    // D (64-bit) variants
    Lrd      { rd: u8, rs1: u8, aq: bool, rl: bool },
    Scd      { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amoswapd { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amoaddd  { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amoxord  { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amoandd  { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amoord   { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amomind  { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amomaxd  { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amominud { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },
    Amomaxud { rd: u8, rs1: u8, rs2: u8, aq: bool, rl: bool },

    // RV64F / RV64D. The family is carried as its raw encoding and decoded in
    // `fpu::execute`; spelling out ~60 variants here would swamp this enum for
    // no benefit, since nothing outside the FPU inspects them individually.
    Fp { raw: u32 },

    // System
    Ecall,
    Ebreak,
    Fence { pred: u8, succ: u8 },
    FenceI,

    // Zicsr (CSR ops)
    Csrrw  { rd: u8, rs1: u8, csr: u16 },
    Csrrs  { rd: u8, rs1: u8, csr: u16 },
    Csrrc  { rd: u8, rs1: u8, csr: u16 },
    Csrrwi { rd: u8, zimm: u8, csr: u16 },
    Csrrsi { rd: u8, zimm: u8, csr: u16 },
    Csrrci { rd: u8, zimm: u8, csr: u16 },

    // Pseudo: mret/sret
    Mret,
    Sret,
    Uret,
    Wfi,
    SfenceVma { rs1: u8, rs2: u8 },
    Unimp,
}

/// Trap cause as per RISC-V privilege spec
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    Exception(Exception),  // Synchronous
    Interrupt(Interrupt),  // Asynchronous
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exception {
    InstructionAddressMisaligned = 0,
    InstructionAccessFault       = 1,
    IllegalInstruction           = 2,
    Breakpoint                   = 3,
    LoadAddressMisaligned        = 4,
    LoadAccessFault              = 5,
    StoreAddressMisaligned       = 6,
    StoreAccessFault             = 7,
    EnvironmentCallFromU         = 8,
    EnvironmentCallFromS         = 9,
    EnvironmentCallFromM         = 11,
    InstructionPageFault         = 12,
    LoadPageFault                = 13,
    StorePageFault               = 15,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    SupervisorSoftware   = 1,
    MachineSoftware      = 3,
    SupervisorTimer      = 5,
    MachineTimer         = 7,
    SupervisorExternal   = 9,
    MachineExternal      = 11,
    // Custom interrupts 16+
}

/// Result of one instruction step
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Trap(Trap),
    Wfi, // waiting for interrupt
}
