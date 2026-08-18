//! A 32-bit instruction whose two halves land on different pages.
//!
//! With the C extension an instruction only needs 2-byte alignment, so one
//! starting at `...ffe` straddles a page boundary. Consecutive *virtual* pages
//! are not physically contiguous, so the upper half must be translated
//! separately — reading `paddr + 2` fetches whatever physically follows the
//! first page.
//!
//! This is nasty to spot in the wild: the low half still yields the correct
//! opcode, rd and rs1, and only the immediate comes out corrupt. It presented
//! as ext4 rejecting a perfectly good block, and took a load trace through the
//! kernel's rbtree walk to find.

use riscv_core::execute::Bus;
use riscv_core::types::Status;
use riscv_devices::DeviceBus;
use riscv_supervisor::types::{Privilege, Satp};
use riscv_supervisor::Supervisor;

const DRAM: u64 = 0x8000_0000;

const ROOT: u64 = DRAM + 0x10_0000;
const MID: u64 = DRAM + 0x10_1000;
const LEAF: u64 = DRAM + 0x10_2000;

// The two code pages, deliberately NOT physically adjacent.
const CODE_PA0: u64 = DRAM + 0x20_0000;
const CODE_PA1: u64 = DRAM + 0x30_0000;

const CODE_VA0: u64 = 0x1000;
const CODE_VA1: u64 = 0x2000;

/// V|R|W|X|A|D — fetchable, readable, writable, already accessed.
const LEAF_FLAGS: u64 = 0xCF;
/// V only — an interior node.
const NODE_FLAGS: u64 = 0x01;

fn pte(pa: u64, flags: u64) -> u64 {
    ((pa >> 12) << 10) | flags
}

fn map(bus: &mut DeviceBus) {
    for p in [ROOT, MID, LEAF] {
        for i in 0..512u64 {
            bus.write_u64(p + i * 8, 0);
        }
    }
    bus.write_u64(ROOT + ((CODE_VA0 >> 30) & 0x1FF) * 8, pte(MID, NODE_FLAGS));
    bus.write_u64(MID + ((CODE_VA0 >> 21) & 0x1FF) * 8, pte(LEAF, NODE_FLAGS));
    bus.write_u64(LEAF + ((CODE_VA0 >> 12) & 0x1FF) * 8, pte(CODE_PA0, LEAF_FLAGS));
    bus.write_u64(LEAF + ((CODE_VA1 >> 12) & 0x1FF) * 8, pte(CODE_PA1, LEAF_FLAGS));
}

fn machine() -> (DeviceBus, Supervisor) {
    let mut bus = DeviceBus::new(8 << 20);
    map(&mut bus);
    let mut s = Supervisor::new(CODE_VA0 + 0xFFE, 0);
    s.priv_level = Privilege::Supervisor;
    s.satp = Satp { mode: 8, asid: 0, ppn: ROOT >> 12 };
    (bus, s)
}

/// Split a 32-bit instruction across the boundary at CODE_VA0 + 0xFFE, and
/// poison what physically follows the first page — that poison is what a buggy
/// fetch reads instead of the real upper half.
fn place_straddling(bus: &mut DeviceBus, insn: u32, poison: u16) {
    bus.write_u16(CODE_PA0 + 0xFFE, insn as u16);
    bus.write_u16(CODE_PA1, (insn >> 16) as u16);
    bus.write_u16(CODE_PA0 + 0x1000, poison);
}

#[test]
fn addi_across_a_page_boundary_keeps_its_immediate() {
    // addi a0, zero, 0x123
    let insn: u32 = (0x123 << 20) | (10 << 7) | 0x13;
    let (mut bus, mut s) = machine();
    // The poison decodes as a different immediate, so a bad fetch produces a
    // wrong a0 rather than accidentally passing.
    place_straddling(&mut bus, insn, 0x7FF0);

    let st = s.step(&mut bus);
    assert!(matches!(st, Status::Running), "straddling fetch trapped: {st:?}");
    assert_eq!(
        s.cpu.read_reg(10),
        0x123,
        "the immediate came from the physically-following page instead of the \
         next VIRTUAL page"
    );
    assert_eq!(s.cpu.pc, CODE_VA0 + 0xFFE + 4, "pc must advance by 4");
}

#[test]
fn load_across_a_page_boundary_uses_the_right_offset() {
    // lwu a0, 32(a5) — the exact instruction that exposed this.
    let insn: u32 = 0x0207_e503;
    let (mut bus, mut s) = machine();
    place_straddling(&mut bus, insn, 0x0000);

    s.cpu.write_reg(15, CODE_VA1);
    bus.write_u32(CODE_PA1 + 32, 0x1234_5678);

    let st = s.step(&mut bus);
    assert!(matches!(st, Status::Running), "straddling load trapped: {st:?}");
    assert_eq!(
        s.cpu.read_reg(10),
        0x1234_5678,
        "a corrupt upper half gives the right register but the wrong offset, so \
         the load lands somewhere else entirely"
    );
}

#[test]
fn compressed_instruction_at_the_page_end_still_works() {
    // c.addi a0, 1 — two bytes, entirely on the first page. Guards against a
    // fix that translates the second half unconditionally.
    let (mut bus, mut s) = machine();
    bus.write_u16(CODE_PA0 + 0xFFE, 0x0505);
    bus.write_u16(CODE_PA1, 0x0001); // c.nop on the next page

    let st = s.step(&mut bus);
    assert!(matches!(st, Status::Running), "compressed fetch trapped: {st:?}");
    assert_eq!(s.cpu.read_reg(10), 1);
    assert_eq!(s.cpu.pc, CODE_VA0 + 0xFFE + 2, "pc must advance by 2");
}
