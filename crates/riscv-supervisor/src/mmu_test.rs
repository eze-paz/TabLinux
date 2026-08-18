//! MMU unit tests — Sv39 page table walk, TLB, permissions, faults

#[cfg(test)]
mod tests {
    use super::super::*;
    use riscv_core::execute::Bus;
    use riscv_core::types::{Trap, Exception};
    use crate::types::{Satp, Privilege, AccessType};
    use std::collections::HashMap;

    /// Simple DRAM backed by a HashMap so we can place page tables arbitrarily.
    struct TestBus {
        mem: HashMap<u64, u8>,
    }

    impl TestBus {
        fn new() -> Self { Self { mem: HashMap::new() } }
        fn write_u64(&mut self, addr: u64, val: u64) {
            for i in 0..8 { self.mem.insert(addr + i, (val >> (8*i)) as u8); }
        }
        fn read_ptr(&self, addr: u64) -> u64 {
            self.read_u64(addr)
        }
    }

    impl Bus for TestBus {
        fn read_u8(&self, addr: u64) -> u8 {
            *self.mem.get(&addr).unwrap_or(&0)
        }
        fn read_u16(&self, addr: u64) -> u16 {
            (0..2).map(|i| (self.read_u8(addr + i) as u16) << (8*i)).fold(0, |a, b| a | b)
        }
        fn read_u32(&self, addr: u64) -> u32 {
            (0..4).map(|i| (self.read_u8(addr + i) as u32) << (8*i)).fold(0, |a, b| a | b)
        }
        fn read_u64(&self, addr: u64) -> u64 {
            (0..8).map(|i| (self.read_u8(addr + i) as u64) << (8*i)).fold(0, |a, b| a | b)
        }
        fn write_u8(&mut self, _addr: u64, _val: u8) {}
        fn write_u16(&mut self, _addr: u64, _val: u16) {}
        fn write_u32(&mut self, _addr: u64, _val: u32) {}
        fn write_u64(&mut self, _addr: u64, _val: u64) {}
    }

    // Helper: construct PTE bitflags
    const PTE_V: u64 = 1;
    const PTE_R: u64 = 2;
    const PTE_W: u64 = 4;
    const PTE_X: u64 = 8;
    const PTE_U: u64 = 16;
    const PTE_A: u64 = 64;
    const PTE_D: u64 = 128;

    fn make_pte(ppn: u64, flags: u64) -> u64 {
        ((ppn & 0xFFFFFFFFFFF) << 10) | flags
    }

    fn satp_sv39(ppn: u64) -> Satp {
        Satp { mode: 8, asid: 0, ppn }
    }

    /// Identity-map a single 4KB page: VA 0x0 -> PA 0x0 via a 1-level page table.
    fn setup_one_level(bus: &mut TestBus) -> Satp {
        // Page table at PA 0x1000
        let root = 0x1000;
        // PTE for vpn[2]=0: points to next-level table at 0x2000
        bus.write_u64(root, make_pte(0x2, PTE_V)); // ppn=0x2 -> 0x2000
        // Level-1 table at PA 0x2000
        // PTE for vpn[1]=0: points to level-0 table at 0x3000
        bus.write_u64(0x2000, make_pte(0x3, PTE_V));
        // Leaf PTE at level 0: vpn[0]=0 -> PA 0x0 (identity), RWX
        bus.write_u64(0x3000, make_pte(0x0, PTE_V | PTE_R | PTE_W | PTE_X));
        satp_sv39(0x1) // root at 0x1000
    }

    /// Identity-map a single 1GB page: VA 0x0 -> PA 0x0 (gigapage at level 2).
    fn setup_gigapage(bus: &mut TestBus) -> Satp {
        let root = 0x1000;
        bus.write_u64(root, make_pte(0x0, PTE_V | PTE_R | PTE_W | PTE_X));
        satp_sv39(0x1)
    }

    // --- Basic identity-mapping tests ---
    #[test]
    fn identity_map_4kb_load() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        let mut mmu = Mmu::new();

        let pa = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, 0x0).unwrap();
        assert_eq!(pa, 0x0);
    }

    #[test]
    fn identity_map_4kb_store() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        let mut mmu = Mmu::new();

        let pa = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Store, 0x500).unwrap();
        assert_eq!(pa, 0x500);
    }

    #[test]
    fn identity_map_4kb_fetch() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        let mut mmu = Mmu::new();

        let pa = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Instruction, 0xFFC).unwrap();
        assert_eq!(pa, 0xFFC);
    }

    #[test]
    fn offset_within_page_preserved() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        let mut mmu = Mmu::new();

        // VA = 0xABC within page 0
        let pa = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, 0xABC).unwrap();
        assert_eq!(pa, 0xABC);
    }

    // --- Multi-page identity mapping ---
    #[test]
    fn second_page() {
        let mut bus = TestBus::new();
        let root = 0x1000;
        bus.write_u64(root, make_pte(0x2, PTE_V));
        bus.write_u64(0x2000, make_pte(0x3, PTE_V));
        // vpn[0]=1 -> PA 0x1000 (second 4KB page)
        bus.write_u64(0x3008, make_pte(0x1, PTE_V | PTE_R | PTE_W | PTE_X));
        let satp = satp_sv39(0x1);
        let mut mmu = Mmu::new();

        let pa = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, 0x1000).unwrap();
        assert_eq!(pa, 0x1000);
    }

    // --- Gigapage ---
    #[test]
    fn gigapage_identity() {
        let mut bus = TestBus::new();
        let satp = setup_gigapage(&mut bus);
        let mut mmu = Mmu::new();

        let pa = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, 0x1234_5678).unwrap();
        assert_eq!(pa, 0x1234_5678);
    }

    // --- Permission faults ---
    #[test]
    fn no_execute_fault() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        // Overwrite leaf to remove X
        bus.write_u64(0x3000, make_pte(0x0, PTE_V | PTE_R | PTE_W));
        let mut mmu = Mmu::new();

        let r = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Instruction, 0x0);
        assert!(matches!(r, Err(Trap::Exception(Exception::InstructionPageFault))));
    }

    #[test]
    fn no_read_fault() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        // Overwrite leaf to remove R
        bus.write_u64(0x3000, make_pte(0x0, PTE_V | PTE_W | PTE_X));
        let mut mmu = Mmu::new();

        let r = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, 0x0);
        assert!(matches!(r, Err(Trap::Exception(Exception::LoadPageFault))));
    }

    #[test]
    fn no_write_fault() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        bus.write_u64(0x3000, make_pte(0x0, PTE_V | PTE_R | PTE_X));
        let mut mmu = Mmu::new();

        let r = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Store, 0x0);
        assert!(matches!(r, Err(Trap::Exception(Exception::StorePageFault))));
    }

    // --- User / SUM ---
    #[test]
    fn u_page_from_s_without_sum_fault() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        bus.write_u64(0x3000, make_pte(0x0, PTE_V | PTE_R | PTE_X | PTE_U));
        let mut mmu = Mmu::new();

        let r = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, 0x0);
        assert!(matches!(r, Err(Trap::Exception(Exception::LoadPageFault))));
    }

    #[test]
    fn u_page_from_s_with_sum_ok() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        bus.write_u64(0x3000, make_pte(0x0, PTE_V | PTE_R | PTE_X | PTE_U));
        let mut mmu = Mmu::new();

        let pa = mmu.translate(&mut bus, &satp, Privilege::Supervisor, true, false,
                                AccessType::Load, 0x0).unwrap();
        assert_eq!(pa, 0x0);
    }

    #[test]
    fn u_page_from_u_ok() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        bus.write_u64(0x3000, make_pte(0x0, PTE_V | PTE_R | PTE_X | PTE_U));
        let mut mmu = Mmu::new();

        let pa = mmu.translate(&mut bus, &satp, Privilege::User, false, false,
                                AccessType::Load, 0x0).unwrap();
        assert_eq!(pa, 0x0);
    }

    // --- MXR --- (load from executable-only page)
    #[test]
    fn mxr_allows_load_from_x_only() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        bus.write_u64(0x3000, make_pte(0x0, PTE_V | PTE_X));
        let mut mmu = Mmu::new();

        let r_no_mxr = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                     AccessType::Load, 0x0);
        assert!(matches!(r_no_mxr, Err(Trap::Exception(Exception::LoadPageFault))));

        let r_mxr = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, true,
                                   AccessType::Load, 0x0);
        assert_eq!(r_mxr.unwrap(), 0x0);
    }

    // --- Invalid PTE type (W=1, R=0) ---
    #[test]
    fn write_no_read_invalid_pte() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        bus.write_u64(0x3000, make_pte(0x0, PTE_V | PTE_W | PTE_X));
        let mut mmu = Mmu::new();

        let r = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                              AccessType::Load, 0x0);
        assert!(matches!(r, Err(Trap::Exception(Exception::LoadPageFault))));
    }

    // --- Non-canonical address ---
    #[test]
    fn non_canonical_high_fault() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        let mut mmu = Mmu::new();
        let bad = 0xFFFF_FFFF_8000_0000u64; // sign-extended but wrong
        let r = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, bad);
        assert!(matches!(r, Err(Trap::Exception(Exception::LoadPageFault))));
    }

    // --- TLB tests ---
    #[test]
    fn tlb_hit_after_miss() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        let mut mmu = Mmu::new();

        let pa1 = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, 0x0).unwrap();
        assert_eq!(pa1, 0x0);
        // Corrupt the page table!
        bus.write_u64(0x3000, 0);
        // Second translation should hit TLB and succeed despite corruption
        let pa2 = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, 0x0).unwrap();
        assert_eq!(pa2, 0x0);
    }

    #[test]
    fn tlb_flush_invalidate() {
        let mut bus = TestBus::new();
        let satp = setup_one_level(&mut bus);
        let mut mmu = Mmu::new();

        mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                      AccessType::Load, 0x0).unwrap();
        // Corrupt page table
        bus.write_u64(0x3000, 0);
        // Flush TLB
        mmu.flush_tlb();
        // Now the walk should see the corrupted PTE and fault
        let r = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, 0x0);
        assert!(matches!(r, Err(Trap::Exception(Exception::LoadPageFault))));
    }

    // --- satp.mode = 0 (bare) ---
    #[test]
    fn bare_mode_no_translation() {
        let mut bus = TestBus::new();
        let satp = Satp { mode: 0, asid: 0, ppn: 0 };
        let mut mmu = Mmu::new();

        let pa = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, 0xDEAD_BEEF).unwrap();
        assert_eq!(pa, 0xDEAD_BEEF);
    }

    // --- Aligned gigapage ---
    #[test]
    fn unaligned_gigapage_fault() {
        let mut bus = TestBus::new();
        let root = 0x1000;
        // ppn has bits 17:0 = 1, not aligned to 1GB
        bus.write_u64(root, make_pte(0x1, PTE_V | PTE_R | PTE_W | PTE_X));
        let satp = satp_sv39(0x1);
        let mut mmu = Mmu::new();

        let r = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, 0x0);
        assert!(matches!(r, Err(Trap::Exception(Exception::LoadPageFault))));
    }

    // --- Two 2MB pages via level-1 leaf ---
    #[test]
    fn megapage_identity() {
        let mut bus = TestBus::new();
        let root = 0x1000;
        // vpn[2]=0 -> points to level-1 table
        bus.write_u64(root, make_pte(0x2, PTE_V));
        // Level-1 leaf: vpn[1] = 0, 2MB page at PA 0
        bus.write_u64(0x2000, make_pte(0x0, PTE_V | PTE_R | PTE_W | PTE_X));
        let satp = satp_sv39(0x1);
        let mut mmu = Mmu::new();

        let pa = mmu.translate(&mut bus, &satp, Privilege::Supervisor, false, false,
                                AccessType::Load, 0x1F_FFFF).unwrap();
        assert_eq!(pa, 0x1F_FFFF);
    }
}
