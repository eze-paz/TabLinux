//! CPU execution engine â€” RV64IMA, single-step with memory trait

use crate::types::{Instr, Status, Trap, Exception};

/// Minimal bus interface: load/store at physical addresses
///
/// In supervisor layer, vaddr â†’ paddr translation happens before calling these.
pub trait Bus {
    fn read_u8(&self, addr: u64) -> u8;
    fn read_u16(&self, addr: u64) -> u16;
    fn read_u32(&self, addr: u64) -> u32;
    fn read_u64(&self, addr: u64) -> u64;
    fn write_u8(&mut self, addr: u64, val: u8);
    fn write_u16(&mut self, addr: u64, val: u16);
    fn write_u32(&mut self, addr: u64, val: u32);
    fn write_u64(&mut self, addr: u64, val: u64);
    /// Poll for timer interrupt (DeviceBus overrides)
    fn check_timer_interrupt(&self) -> bool { false }
    /// Read the current mtime value (for time CSR)
    fn read_mtime(&self) -> u64 { 0 }
    /// Does the platform interrupt controller (PLIC) have a claimable interrupt?
    /// Drives the supervisor external interrupt pending bit (SEIP).
    fn check_external_interrupt(&self) -> bool { false }
}

/// RISC-V CPU state (integer registers + PC)
///
/// x[0] is hardwired to zero.
///
/// `repr(C)` is load-bearing, not tidiness. JIT-generated wasm reads and writes
/// `x` directly in linear memory and looks for the fault flag at `x + 256`, so
/// the order of these two fields is part of the contract with generated code.
#[repr(C)]
pub struct Cpu {
    pub x: [u64; 32],
    /// Set by the host when a guest access made from compiled code faults.
    ///
    /// Must stay immediately after `x`: generated code reads it as an i32 at
    /// offset 256 from the register base and returns from the block when it is
    /// non-zero. Without this field that offset lands on `f[0]`.
    pub jit_fault: u64,
    /// Where a compiled block that ended in a branch is sending control.
    ///
    /// Offset 264 from the register base, immediately after `jit_fault`, for
    /// the same reason: generated code writes it directly. Only meaningful for
    /// blocks whose last instruction is a branch or jump; others fall through
    /// and the host advances the PC by the block's length.
    pub jit_next_pc: u64,
    /// Instructions retired by the current chain of compiled blocks.
    ///
    /// Offset 272 from the register base. Each block adds its own count, so the
    /// host can tell how much a chain did without seeing each hop.
    pub jit_insns: u64,
    /// Chain budget, offset 280. A block stops chaining once `jit_insns`
    /// reaches this, which is what bounds interrupt latency.
    pub jit_budget: u64,
    pub f: [u64; 32],
    pub pc: u64,
    /// Floating-point control and status: fflags in [4:0], frm in [7:5].
    /// Exception flags are sticky — only software clears them.
    pub fcsr: u64,
    /// Set when an instruction wrote an f register, so the supervisor can move
    /// mstatus.FS to Dirty. Linux uses FS to decide whether a task's FP state
    /// needs saving on a context switch; leaving it Clean loses registers.
    pub fs_dirty: bool,
}

impl Cpu {
    pub fn new(pc: u64) -> Self {
        let mut cpu = Self { x: [0; 32], jit_fault: 0, jit_next_pc: 0, jit_insns: 0,
            jit_budget: 0, f: [0; 32], pc, fcsr: 0, fs_dirty: false };
        cpu.x[0] = 0;
        cpu
    }

    pub fn read_reg(&self, i: u8) -> u64 {
        self.x[i as usize & 31]
    }

    pub fn write_reg(&mut self, i: u8, val: u64) {
        if i != 0 {
            self.x[i as usize] = val;
        }
    }

    pub fn read_freg(&self, i: u8) -> u64 {
        self.f[i as usize & 31]
    }

    pub fn write_freg(&mut self, i: u8, val: u64) {
        self.f[i as usize & 31] = val;
    }

    /// Execute one decoded instruction
    pub fn execute(&mut self, instr: Instr, bus: &mut dyn Bus) -> Status {
        self.execute_width(instr, 4, bus)
    }

    pub fn execute_width(&mut self, instr: Instr, width: u8, bus: &mut dyn Bus) -> Status {
        use Instr::*;

        match instr {
            // --- U-Type ---
            Lui { rd, imm } => { self.write_reg(rd, imm); self.pc = self.pc.wrapping_add(width as u64); }
            Auipc { rd, imm } => { self.write_reg(rd, self.pc.wrapping_add(imm)); self.pc = self.pc.wrapping_add(width as u64); }

            // --- J-Type ---
            Jal { rd, imm } => { self.write_reg(rd, self.pc.wrapping_add(width as u64)); self.pc = (self.pc as i64).wrapping_add(imm) as u64; }
            Jalr { rd, rs1, imm } => {
                let target = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64 & !1;
                self.write_reg(rd, self.pc.wrapping_add(width as u64));
                self.pc = target;
            }

            // --- B-Type ---
            Beq { rs1, rs2, imm } => {
                if self.read_reg(rs1) == self.read_reg(rs2) {
                    self.pc = (self.pc as i64).wrapping_add(imm) as u64;
                } else { self.pc = self.pc.wrapping_add(width as u64); }
            }
            Bne { rs1, rs2, imm } => {
                if self.read_reg(rs1) != self.read_reg(rs2) {
                    self.pc = (self.pc as i64).wrapping_add(imm) as u64;
                } else { self.pc = self.pc.wrapping_add(width as u64); }
            }

            Blt { rs1, rs2, imm } => {
                if (self.read_reg(rs1) as i64) < (self.read_reg(rs2) as i64) {
                    self.pc = (self.pc as i64).wrapping_add(imm) as u64;
                } else { self.pc = self.pc.wrapping_add(width as u64); }
            }
            Bge { rs1, rs2, imm } => {
                if (self.read_reg(rs1) as i64) >= (self.read_reg(rs2) as i64) {
                    self.pc = (self.pc as i64).wrapping_add(imm) as u64;
                } else { self.pc = self.pc.wrapping_add(width as u64); }
            }
            Bltu { rs1, rs2, imm } => {
                if self.read_reg(rs1) < self.read_reg(rs2) {
                    self.pc = (self.pc as i64).wrapping_add(imm) as u64;
                } else { self.pc = self.pc.wrapping_add(width as u64); }
            }
            Bgeu { rs1, rs2, imm } => {
                if self.read_reg(rs1) >= self.read_reg(rs2) {
                    self.pc = (self.pc as i64).wrapping_add(imm) as u64;
                } else { self.pc = self.pc.wrapping_add(width as u64); }
            }

            // --- I-Type Load ---
            Lb { rd, rs1, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                let val = bus.read_u8(addr) as i8 as i64 as u64;
                self.write_reg(rd, val);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Lh { rd, rs1, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                let val = bus.read_u16(addr) as i16 as i64 as u64;
                self.write_reg(rd, val);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Lw { rd, rs1, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                let val = bus.read_u32(addr) as i32 as i64 as u64;
                self.write_reg(rd, val);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Ld { rd, rs1, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                let val = bus.read_u64(addr);
                self.write_reg(rd, val);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Lbu { rd, rs1, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                self.write_reg(rd, bus.read_u8(addr) as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Lhu { rd, rs1, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                self.write_reg(rd, bus.read_u16(addr) as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Lwu { rd, rs1, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                self.write_reg(rd, bus.read_u32(addr) as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Flw { rd, rs1, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                // NaN-box: a single loaded into a 64-bit f register must have
                // all upper bits set. Zero-extending made every flw read back
                // as a canonical NaN in any single-precision op, because an
                // improperly boxed operand is defined to read as NaN.
                self.f[rd as usize] = 0xFFFF_FFFF_0000_0000 | bus.read_u32(addr) as u64;
                self.fs_dirty = true;
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Fld { rd, rs1, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                self.f[rd as usize] = bus.read_u64(addr);
                self.fs_dirty = true;
                self.pc = self.pc.wrapping_add(width as u64);
            }

            // --- S-Type Store ---
            Sb { rs1, rs2, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                bus.write_u8(addr, self.read_reg(rs2) as u8);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Sh { rs1, rs2, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                bus.write_u16(addr, self.read_reg(rs2) as u16);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Sw { rs1, rs2, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                bus.write_u32(addr, self.read_reg(rs2) as u32);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Sd { rs1, rs2, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                bus.write_u64(addr, self.read_reg(rs2));
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Fsw { rs1, rs2, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                bus.write_u32(addr, self.f[rs2 as usize] as u32);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Fsd { rs1, rs2, imm } => {
                let addr = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                bus.write_u64(addr, self.f[rs2 as usize]);
                self.pc = self.pc.wrapping_add(width as u64);
            }

            // --- I-Type ALU ---
            Addi { rd, rs1, imm } => {
                let res = (self.read_reg(rs1) as i64).wrapping_add(imm) as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Slti { rd, rs1, imm } => {
                let res = if (self.read_reg(rs1) as i64) < imm { 1 } else { 0 };
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Sltiu { rd, rs1, imm } => {
                let res = if self.read_reg(rs1) < (imm as u64) { 1 } else { 0 };
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Xori { rd, rs1, imm } => {
                self.write_reg(rd, self.read_reg(rs1) ^ (imm as u64));
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Ori { rd, rs1, imm } => {
                self.write_reg(rd, self.read_reg(rs1) | (imm as u64));
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Andi { rd, rs1, imm } => {
                self.write_reg(rd, self.read_reg(rs1) & (imm as u64));
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Slli { rd, rs1, shamt } => {
                self.write_reg(rd, self.read_reg(rs1) << shamt);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Srli { rd, rs1, shamt } => {
                self.write_reg(rd, self.read_reg(rs1) >> shamt);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Srai { rd, rs1, shamt } => {
                self.write_reg(rd, ((self.read_reg(rs1) as i64) >> shamt) as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }

            // --- R-Type ---
            Add { rd, rs1, rs2 } => {
                let res = (self.read_reg(rs1) as i64).wrapping_add(self.read_reg(rs2) as i64) as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Sub { rd, rs1, rs2 } => {
                let res = (self.read_reg(rs1) as i64).wrapping_sub(self.read_reg(rs2) as i64) as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Sll { rd, rs1, rs2 } => {
                let sh = self.read_reg(rs2) & 0x3F;
                self.write_reg(rd, self.read_reg(rs1) << sh);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Slt { rd, rs1, rs2 } => {
                let res = if (self.read_reg(rs1) as i64) < (self.read_reg(rs2) as i64) { 1 } else { 0 };
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Sltu { rd, rs1, rs2 } => {
                let res = if self.read_reg(rs1) < self.read_reg(rs2) { 1 } else { 0 };
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Xor { rd, rs1, rs2 } => {
                self.write_reg(rd, self.read_reg(rs1) ^ self.read_reg(rs2));
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Srl { rd, rs1, rs2 } => {
                let sh = self.read_reg(rs2) & 0x3F;
                self.write_reg(rd, self.read_reg(rs1) >> sh);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Sra { rd, rs1, rs2 } => {
                let sh = self.read_reg(rs2) & 0x3F;
                self.write_reg(rd, ((self.read_reg(rs1) as i64) >> sh) as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Or { rd, rs1, rs2 } => {
                self.write_reg(rd, self.read_reg(rs1) | self.read_reg(rs2));
                self.pc = self.pc.wrapping_add(width as u64);
            }
            And { rd, rs1, rs2 } => {
                self.write_reg(rd, self.read_reg(rs1) & self.read_reg(rs2));
                self.pc = self.pc.wrapping_add(width as u64);
            }

            // --- RV64I W-type ---
            Addiw { rd, rs1, imm } => {
                let res = (self.read_reg(rs1) as i32).wrapping_add(imm as i32) as i64 as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Slliw { rd, rs1, shamt } => {
                let res = ((self.read_reg(rs1) as i32) << shamt) as i64 as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Srliw { rd, rs1, shamt } => {
                // Every *W op sign-extends its 32-bit result into the 64-bit
                // register. `u32 as u64` zero-extends, so any result with bit
                // 31 set came out as a positive 32-bit value instead of a
                // negative 64-bit one.
                let res = ((self.read_reg(rs1) as u32) >> shamt) as i32 as i64 as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Sraiw { rd, rs1, shamt } => {
                let res = ((self.read_reg(rs1) as i32) >> shamt) as i64 as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Addw { rd, rs1, rs2 } => {
                let res = (self.read_reg(rs1) as i32).wrapping_add(self.read_reg(rs2) as i32) as i64 as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Subw { rd, rs1, rs2 } => {
                let res = (self.read_reg(rs1) as i32).wrapping_sub(self.read_reg(rs2) as i32) as i64 as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Sllw { rd, rs1, rs2 } => {
                let sh = self.read_reg(rs2) & 0x1F;
                let res = ((self.read_reg(rs1) as u32) << sh) as i32 as i64 as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Srlw { rd, rs1, rs2 } => {
                let sh = self.read_reg(rs2) & 0x1F;
                let res = ((self.read_reg(rs1) as u32) >> sh) as i32 as i64 as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Sraw { rd, rs1, rs2 } => {
                let sh = self.read_reg(rs2) & 0x1F;
                let res = ((self.read_reg(rs1) as i32) >> sh) as i64 as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }

            // --- RV64M ---
            Mul { rd, rs1, rs2 } => {
                let res = self.read_reg(rs1).wrapping_mul(self.read_reg(rs2));
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Mulh { rd, rs1, rs2 } => {
                let a = self.read_reg(rs1) as i64 as i128;
                let b = self.read_reg(rs2) as i64 as i128;
                let res = ((a * b) >> 64) as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Mulhsu { rd, rs1, rs2 } => {
                let a = self.read_reg(rs1) as i64 as i128;
                let b = self.read_reg(rs2) as u128;
                let res = ((a as u128 * b) >> 64) as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Mulhu { rd, rs1, rs2 } => {
                let a = self.read_reg(rs1) as u128;
                let b = self.read_reg(rs2) as u128;
                let res = ((a * b) >> 64) as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Div { rd, rs1, rs2 } => {
                let a = self.read_reg(rs1) as i64;
                let b = self.read_reg(rs2) as i64;
                let res = if b == 0 { u64::MAX }
                          else if a == i64::MIN && b == -1 { a as u64 }
                          else { (a / b) as u64 };
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Divu { rd, rs1, rs2 } => {
                let a = self.read_reg(rs1);
                let b = self.read_reg(rs2);
                let res = if b == 0 { u64::MAX } else { a / b };
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Rem { rd, rs1, rs2 } => {
                let a = self.read_reg(rs1) as i64;
                let b = self.read_reg(rs2) as i64;
                let res = if b == 0 { a as u64 }
                          else if a == i64::MIN && b == -1 { 0 }
                          else { (a % b) as u64 };
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Remu { rd, rs1, rs2 } => {
                let a = self.read_reg(rs1);
                let b = self.read_reg(rs2);
                let res = if b == 0 { a } else { a % b };
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Mulw { rd, rs1, rs2 } => {
                let res = (self.read_reg(rs1) as i32).wrapping_mul(self.read_reg(rs2) as i32) as i64 as u64;
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Divw { rd, rs1, rs2 } => {
                let a = self.read_reg(rs1) as i32;
                let b = self.read_reg(rs2) as i32;
                let res = if b == 0 { u64::MAX }
                          else if a == i32::MIN && b == -1 { a as u64 }
                          else { (a / b) as i64 as u64 };
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Divuw { rd, rs1, rs2 } => {
                let a = self.read_reg(rs1) as u32;
                let b = self.read_reg(rs2) as u32;
                let res = if b == 0 { u64::MAX } else { (a / b) as i32 as i64 as u64 };
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Remw { rd, rs1, rs2 } => {
                let a = self.read_reg(rs1) as i32;
                let b = self.read_reg(rs2) as i32;
                let res = if b == 0 { a as u64 }
                          else if a == i32::MIN && b == -1 { 0 }
                          else { (a % b) as i64 as u64 };
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Remuw { rd, rs1, rs2 } => {
                let a = self.read_reg(rs1) as u32;
                let b = self.read_reg(rs2) as u32;
                let res = if b == 0 { a as i32 as i64 as u64 } else { (a % b) as i32 as i64 as u64 };
                self.write_reg(rd, res);
                self.pc = self.pc.wrapping_add(width as u64);
            }

            // --- RV64A (single hart = regular ops) ---
            Lrw { rd, rs1, .. } => {
                let addr = self.read_reg(rs1);
                let val = bus.read_u32(addr) as i32 as u64;
                self.write_reg(rd, val);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Scw { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                bus.write_u32(addr, self.read_reg(rs2) as u32);
                self.write_reg(rd, 0); // success
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amoswapw { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u32(addr) as i32 as u64;
                bus.write_u32(addr, self.read_reg(rs2) as u32);
                self.write_reg(rd, old);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amoaddw { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u32(addr) as i32;
                let new = old.wrapping_add(self.read_reg(rs2) as i32);
                bus.write_u32(addr, new as u32);
                self.write_reg(rd, old as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amoxorw { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u32(addr);
                bus.write_u32(addr, old ^ self.read_reg(rs2) as u32);
                self.write_reg(rd, old as i32 as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amoandw { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u32(addr);
                bus.write_u32(addr, old & self.read_reg(rs2) as u32);
                self.write_reg(rd, old as i32 as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amoorw { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u32(addr);
                bus.write_u32(addr, old | self.read_reg(rs2) as u32);
                self.write_reg(rd, old as i32 as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amominw { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u32(addr) as i32;
                let new = self.read_reg(rs2) as i32;
                let min = if old < new { old } else { new };
                bus.write_u32(addr, min as u32);
                self.write_reg(rd, old as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amomaxw { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u32(addr) as i32;
                let new = self.read_reg(rs2) as i32;
                let max = if old > new { old } else { new };
                bus.write_u32(addr, max as u32);
                self.write_reg(rd, old as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amominuw { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u32(addr);
                let new = self.read_reg(rs2) as u32;
                let min = if old < new { old } else { new };
                bus.write_u32(addr, min);
                // Sign-extended, like every other .w atomic: the "u" in AMOMINU.W
                // selects an unsigned COMPARISON, it does not change how the old
                // value is placed in rd. This read zero-extended until the JIT
                // difftest disagreed with it.
                self.write_reg(rd, old as i32 as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amomaxuw { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u32(addr);
                let new = self.read_reg(rs2) as u32;
                let max = if old > new { old } else { new };
                bus.write_u32(addr, max);
                // Sign-extended, like every other .w atomic: the "u" in AMOMINU.W
                // selects an unsigned COMPARISON, it does not change how the old
                // value is placed in rd. This read zero-extended until the JIT
                // difftest disagreed with it.
                self.write_reg(rd, old as i32 as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            // 64-bit atomics
            Lrd { rd, rs1, .. } => {
                let addr = self.read_reg(rs1);
                let val = bus.read_u64(addr);
                self.write_reg(rd, val);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Scd { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                bus.write_u64(addr, self.read_reg(rs2));
                self.write_reg(rd, 0);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amoswapd { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u64(addr);
                bus.write_u64(addr, self.read_reg(rs2));
                self.write_reg(rd, old);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amoaddd { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u64(addr) as i64;
                let new = old.wrapping_add(self.read_reg(rs2) as i64);
                bus.write_u64(addr, new as u64);
                self.write_reg(rd, old as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amoxord { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u64(addr);
                bus.write_u64(addr, old ^ self.read_reg(rs2));
                self.write_reg(rd, old);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amoandd { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u64(addr);
                bus.write_u64(addr, old & self.read_reg(rs2));
                self.write_reg(rd, old);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amoord { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u64(addr);
                bus.write_u64(addr, old | self.read_reg(rs2));
                self.write_reg(rd, old);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amomind { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u64(addr) as i64;
                let new = self.read_reg(rs2) as i64;
                let min = if old < new { old } else { new };
                bus.write_u64(addr, min as u64);
                self.write_reg(rd, old as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amomaxd { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u64(addr) as i64;
                let new = self.read_reg(rs2) as i64;
                let max = if old > new { old } else { new };
                bus.write_u64(addr, max as u64);
                self.write_reg(rd, old as u64);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amominud { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u64(addr);
                let new = self.read_reg(rs2);
                let min = if old < new { old } else { new };
                bus.write_u64(addr, min);
                self.write_reg(rd, old);
                self.pc = self.pc.wrapping_add(width as u64);
            }
            Amomaxud { rd, rs1, rs2, .. } => {
                let addr = self.read_reg(rs1);
                let old = bus.read_u64(addr);
                let new = self.read_reg(rs2);
                let max = if old > new { old } else { new };
                bus.write_u64(addr, max);
                self.write_reg(rd, old);
                self.pc = self.pc.wrapping_add(width as u64);
            }

            // --- System ---
            Ecall => return Status::Trap(Trap::Exception(Exception::EnvironmentCallFromM)),
            Ebreak => return Status::Trap(Trap::Exception(Exception::Breakpoint)),
            Fp { raw } => {
                let r = crate::fpu::execute(self, raw);
                if !r.ok {
                    return Status::Trap(Trap::Exception(Exception::IllegalInstruction));
                }
                self.fcsr |= r.flags;
                self.fs_dirty |= r.dirty;
                self.pc = self.pc.wrapping_add(width as u64);
            }

            Unimp => return Status::Trap(Trap::Exception(Exception::IllegalInstruction)),
            Fence { .. } | FenceI => { self.pc = self.pc.wrapping_add(width as u64); } // no-op for single core
            Mret => return Status::Trap(Trap::Exception(Exception::IllegalInstruction)), // handled by supervisor layer
            Sret => return Status::Trap(Trap::Exception(Exception::IllegalInstruction)),
            Uret => return Status::Trap(Trap::Exception(Exception::IllegalInstruction)),
            Wfi => return Status::Wfi,
            SfenceVma { .. } => { self.pc = self.pc.wrapping_add(width as u64); } // no-op without MMU
            Csrrw { .. } | Csrrs { .. } | Csrrc { .. } | Csrrwi { .. } | Csrrsi { .. } | Csrrci { .. } => {
                return Status::Trap(Trap::Exception(Exception::IllegalInstruction)); // handled by supervisor
            }
        }

        Status::Running
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestBus([u8; 1024]);
    impl TestBus {
        fn new() -> Self { Self([0; 1024]) }
    }
    impl Bus for TestBus {
        fn read_u8(&self, addr: u64) -> u8 { self.0[addr as usize] }
        fn read_u16(&self, addr: u64) -> u16 { u16::from_le_bytes([self.0[addr as usize], self.0[addr as usize + 1]]) }
        fn read_u32(&self, addr: u64) -> u32 { u32::from_le_bytes([self.0[addr as usize], self.0[addr as usize + 1], self.0[addr as usize + 2], self.0[addr as usize + 3]]) }
        fn read_u64(&self, addr: u64) -> u64 { (self.read_u32(addr) as u64) | ((self.read_u32(addr + 4) as u64) << 32) }
        fn write_u8(&mut self, addr: u64, val: u8) { self.0[addr as usize] = val; }
        fn write_u16(&mut self, addr: u64, val: u16) { self.0[addr as usize..addr as usize + 2].copy_from_slice(&val.to_le_bytes()); }
        fn write_u32(&mut self, addr: u64, val: u32) { self.0[addr as usize..addr as usize + 4].copy_from_slice(&val.to_le_bytes()); }
        fn write_u64(&mut self, addr: u64, val: u64) { self.0[addr as usize..addr as usize + 8].copy_from_slice(&val.to_le_bytes()); }
    }

    #[test]
    fn addi_sequence() {
        let mut cpu = Cpu::new(0);
        let mut bus = TestBus::new();
        cpu.write_reg(1, 10);
        cpu.execute(Instr::Addi { rd: 2, rs1: 1, imm: 5 }, &mut bus);
        assert_eq!(cpu.read_reg(2), 15);
        assert_eq!(cpu.pc, 4);
    }

    #[test]
    fn load_store_byte() {
        let mut cpu = Cpu::new(0);
        let mut bus = TestBus::new();
        cpu.write_reg(1, 100);
        cpu.write_reg(2, 0x42);
        cpu.execute(Instr::Sb { rs1: 1, rs2: 2, imm: 0 }, &mut bus);
        assert_eq!(bus.read_u8(100), 0x42);

        cpu.execute(Instr::Lb { rd: 3, rs1: 1, imm: 0 }, &mut bus);
        assert_eq!(cpu.read_reg(3), 0x42);
    }

    #[test]
    fn mul_hi() {
        let mut cpu = Cpu::new(0);
        let mut bus = TestBus::new();
        cpu.write_reg(1, 0x00000000FFFFFFFF);
        cpu.write_reg(2, 0x00000000FFFFFFFF);
        cpu.execute(Instr::Mulhu { rd: 3, rs1: 1, rs2: 2 }, &mut bus);
        // 0xFFFFFFFF * 0xFFFFFFFF = 0xFFFFFFFE00000001 (64-bit result, upper 64 bits = 0)
        assert_eq!(cpu.read_reg(3), 0);
        // now test with values that do overflow 64 bits
        cpu.write_reg(1, 0xFFFFFFFFFFFFFFFF);
        cpu.write_reg(2, 0xFFFFFFFFFFFFFFFF);
        cpu.execute(Instr::Mulhu { rd: 3, rs1: 1, rs2: 2 }, &mut bus);
        assert_eq!(cpu.read_reg(3), 0xFFFFFFFFFFFFFFFE);
    }
}