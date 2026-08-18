//! Run the official riscv-tests ISA suite against the emulator.
//!
//! These are self-checking: each test runs a sequence of instructions in
//! machine mode and signals completion by storing to the `tohost` symbol —
//! 1 means pass, anything else encodes which numbered sub-test failed. That
//! makes them far better at localising a decode or execute bug than inferring
//! it from a wrecked Linux kernel.
//!
//! Build the binaries first (they are not checked in):
//!     bash ~/riscv-tests/build-p.sh
//!
//! Then:
//!     cargo test --release -p riscv-harness --test isa_suite -- --nocapture

use riscv_core::types::Status;
use riscv_devices::DeviceBus;
use riscv_supervisor::{types::Privilege, Supervisor};

const DRAM_BASE: u64 = 0x8000_0000;
const DRAM_SIZE: usize = 16 << 20;
const MAX_STEPS: u64 = 20_000_000;

struct Elf {
    entry: u64,
    /// (physical address, bytes)
    segments: Vec<(u64, Vec<u8>)>,
    tohost: Option<u64>,
}

fn rd16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) }
fn rd32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
fn rd64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

/// Minimal ELF64 little-endian reader: PT_LOAD segments plus the `tohost` symbol.
fn parse_elf(b: &[u8]) -> Option<Elf> {
    if b.len() < 64 || &b[0..4] != b"\x7fELF" || b[4] != 2 || b[5] != 1 {
        return None;
    }
    let entry = rd64(b, 24);
    let phoff = rd64(b, 32) as usize;
    let shoff = rd64(b, 40) as usize;
    let phentsize = rd16(b, 54) as usize;
    let phnum = rd16(b, 56) as usize;
    let shentsize = rd16(b, 58) as usize;
    let shnum = rd16(b, 60) as usize;

    let mut segments = Vec::new();
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if rd32(b, p) != 1 {
            continue; // not PT_LOAD
        }
        let off = rd64(b, p + 8) as usize;
        let paddr = rd64(b, p + 24);
        let filesz = rd64(b, p + 32) as usize;
        let memsz = rd64(b, p + 40) as usize;
        let mut data = vec![0u8; memsz];
        let n = filesz.min(b.len().saturating_sub(off));
        data[..n].copy_from_slice(&b[off..off + n]);
        segments.push((paddr, data));
    }

    // Walk section headers for .symtab and its string table.
    let mut tohost = None;
    for i in 0..shnum {
        let s = shoff + i * shentsize;
        if rd32(b, s + 4) != 2 {
            continue; // not SHT_SYMTAB
        }
        let symoff = rd64(b, s + 24) as usize;
        let symsize = rd64(b, s + 32) as usize;
        let strtab_idx = rd32(b, s + 40) as usize;
        let st = shoff + strtab_idx * shentsize;
        let stroff = rd64(b, st + 24) as usize;
        let mut o = symoff;
        while o + 24 <= symoff + symsize {
            let nameoff = rd32(b, o) as usize;
            let value = rd64(b, o + 8);
            let start = stroff + nameoff;
            if start < b.len() {
                let end = b[start..].iter().position(|&c| c == 0).unwrap_or(0) + start;
                if &b[start..end] == b"tohost" {
                    tohost = Some(value);
                    break;
                }
            }
            o += 24;
        }
    }
    Some(Elf { entry, segments, tohost })
}

enum Outcome {
    Pass,
    /// riscv-tests encodes failure as (subtest << 1) | 1. Carries the last
    /// trap's cause/epc/tval, which is usually what the test's handler
    /// disagreed with.
    Failed(u64, u64, u64, u64),
    /// (pc, TESTNUM). riscv-tests keeps the subtest number in gp, so this says
    /// which case was in flight when the run stalled.
    Timeout(u64, u64),
    Trapped(String, u64),
}

fn run_one(path: &std::path::Path) -> Outcome {
    let bytes = std::fs::read(path).expect("read test");
    let elf = match parse_elf(&bytes) {
        Some(e) => e,
        None => return Outcome::Trapped("not an ELF64".into(), 0),
    };

    let mut bus = DeviceBus::new(DRAM_SIZE);
    for (paddr, data) in &elf.segments {
        if *paddr >= DRAM_BASE && *paddr + data.len() as u64 <= DRAM_BASE + DRAM_SIZE as u64 {
            bus.load_blob(*paddr, data);
        }
    }

    let mut s = Supervisor::new(elf.entry, 0);
    s.priv_level = Privilege::Machine;
    // Reset state, not the Linux-boot conveniences Supervisor::new sets up.
    // The tests configure delegation themselves, and they need ecall to trap
    // rather than be answered as SBI.
    s.sbi_enabled = false;
    s.medeleg = 0;
    s.mideleg = 0;
    s.mie = 0;

    let tohost = match elf.tohost {
        Some(t) => t,
        None => return Outcome::Trapped("no tohost symbol".into(), 0),
    };
    let tohost_off = (tohost - DRAM_BASE) as usize;

    let trace = std::env::var("ISA_TRACE")
        .map(|v| path.file_name().unwrap().to_string_lossy().contains(&v))
        .unwrap_or(false);

    let mut step = 0u64;
    while step < MAX_STEPS {
        if trace && (60..120).contains(&step) {
            eprintln!(
                "  [{step:3}] pc={:#012x} priv={:?} mtvec={:#x}",
                s.cpu.pc, s.priv_level, s.mtvec
            );
        }
        bus.tick();
        match s.step(&mut bus) {
            Status::Running | Status::Wfi => {}
            Status::Trap(_) => {
                // The tests install their own trap handlers; a trap here is
                // normal control flow, not a failure.
            }
        }
        // Poll tohost rather than hooking stores: cheaper than it looks, and
        // these tests are short.
        if step % 64 == 0 {
            let v = u64::from_le_bytes(bus.get_dram()[tohost_off..tohost_off + 8].try_into().unwrap());
            if v != 0 {
                return if v == 1 {
                    Outcome::Pass
                } else {
                    Outcome::Failed(v >> 1, s.mcause, s.mepc, s.mtval)
                };
            }
        }
        step += 1;
    }
    let v = u64::from_le_bytes(bus.get_dram()[tohost_off..tohost_off + 8].try_into().unwrap());
    if v == 1 {
        return Outcome::Pass;
    }
    Outcome::Timeout(s.cpu.pc, s.cpu.read_reg(3))
}

#[test]
fn riscv_isa_suite() {
    let dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("riscv-tests/build-p");
    if !dir.is_dir() {
        eprintln!("SKIP: {} not built — run bash ~/riscv-tests/build-p.sh", dir.display());
        return;
    }
    let mut tests: Vec<_> = std::fs::read_dir(&dir)
        .expect("read build dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    tests.sort();

    let mut pass = 0;
    let mut failures: Vec<String> = Vec::new();
    for t in &tests {
        let name = t.file_name().unwrap().to_string_lossy().to_string();
        match run_one(t) {
            Outcome::Pass => pass += 1,
            Outcome::Failed(n, cause, epc, tval) => failures.push(format!(
                "{name}: FAILED subtest {n} (last trap: mcause={cause} mepc={epc:#x} mtval={tval:#x})"
            )),
            Outcome::Timeout(pc, tn) => {
                failures.push(format!("{name}: TIMEOUT at pc={pc:#x} TESTNUM={tn}"))
            }
            Outcome::Trapped(why, _) => failures.push(format!("{name}: {why}")),
        }
    }

    eprintln!("\n=== riscv-tests: {pass}/{} passed ===", tests.len());
    for f in &failures {
        eprintln!("  {f}");
    }
    assert!(
        failures.is_empty(),
        "{} ISA test(s) failed — see list above",
        failures.len()
    );
}
