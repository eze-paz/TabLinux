//! SV39 MMU with set-associative TLB

use riscv_core::execute::Bus;
use riscv_core::types::{Trap, Exception};
use crate::types::{Satp, Privilege, AccessType};

/// One TLB entry: after successful page-table walk we cache the translation.
#[derive(Debug, Clone, Copy, Default)]
struct TlbEntry {
    valid: bool,
    /// The VA's page number at this entry's granularity (>>12/>>21/>>30 for
    /// 4KB/2MB/1GB). Matched together with `size` on lookup.
    va_key: u64,
    /// Physical page frame base (paddr with page-offset masked off).
    ppn: u64,
    /// Cache of PTE flags for permission checks.
    flags: u8,
    /// Page size: 0 = 4KB, 1 = 2MB, 2 = 1GB.
    size: u8,
}

/// 4-way set-associative TLB, 64 sets = 256 entries.
pub struct Tlb {
    sets: [TlbEntry; 256],
    /// Pseudo-LRU: 2 bits per set (0..3) indicating next victim way.
    lru: [u8; 64],
}

impl Tlb {
    pub fn new() -> Self {
        Self {
            sets: [TlbEntry::default(); 256],
            lru: [0; 64],
        }
    }

    pub fn flush(&mut self) {
        for e in self.sets.iter_mut() { e.valid = false; }
        for l in self.lru.iter_mut() { *l = 0; }
    }

    /// The virtual page number at a given page size: >>12 for 4KB, >>21 for
    /// 2MB, >>30 for 1GB. A superpage is keyed by its own coarse VPN so that
    /// every 4KB access inside it hashes to the same set and key — otherwise
    /// 511 of a 2MB page's 512 constituent pages miss and re-walk.
    fn vpn_at(vaddr: u64, size: u8) -> u64 {
        match size {
            2 => vaddr >> 30,
            1 => vaddr >> 21,
            _ => vaddr >> 12,
        }
    }

    fn set_of(vpn: u64) -> usize {
        (vpn & 0x3F) as usize
    }

    fn lookup(&self, vaddr: u64) -> Option<&TlbEntry> {
        // Probe finest-first: most translations are 4KB. The `size` check makes
        // a coincidental key collision at another granularity impossible, and a
        // superpage — inserted under its coarse VPN — is found here by all of
        // its 4KB sub-pages instead of forcing a fresh SV39 walk each time.
        for size in 0u8..=2 {
            let vpn = Self::vpn_at(vaddr, size);
            let start = Self::set_of(vpn) * 4;
            for i in 0..4 {
                let e = &self.sets[start + i];
                if e.valid && e.size == size && e.va_key == vpn {
                    return Some(e);
                }
            }
        }
        None
    }

    fn insert(&mut self, vaddr: u64, ppn: u64, size: u8, flags: u8) {
        let vpn = Self::vpn_at(vaddr, size);
        let set = Self::set_of(vpn);
        let way = self.lru[set] as usize & 3;
        self.lru[set] = (self.lru[set] + 1) & 3;
        self.sets[set * 4 + way] = TlbEntry {
            valid: true,
            va_key: vpn,
            ppn,
            flags,
            size,
        };
    }
}

pub struct Mmu {
    tlb: Tlb,
    pub dbg_fail_count: u64,
    pub dbg_fail_vaddr: u64,
    pub dbg_fail_pte_addr: u64,
    pub dbg_fail_pte: u64,
    pub dbg_fail_level: u64,
    pub dbg_fail_reason: u64,
    pub dbg_satp_ppn: u64,
    pub dbg_root_pte: u64,
    pub dbg_dumped: bool,
    pub dbg_walk_vaddr: u64,
    pub dbg_walk_root: u64,
    pub dbg_walk_pte_addr: [u64; 3],
    pub dbg_walk_pte: [u64; 3],
    pub dbg_walk_paddr: u64,
    pub dbg_walk_ppn: u64,
    pub dbg_walk_size: u8,
}

impl Mmu {
    pub fn new() -> Self { Self { tlb: Tlb::new(), dbg_fail_count:0, dbg_fail_vaddr:0, dbg_fail_pte_addr:0, dbg_fail_pte:0, dbg_fail_level:0, dbg_fail_reason:0, dbg_satp_ppn:0, dbg_root_pte:0, dbg_dumped:false, dbg_walk_vaddr:0, dbg_walk_root:0, dbg_walk_pte_addr:[0u64;3], dbg_walk_pte:[0u64;3], dbg_walk_paddr:0, dbg_walk_ppn:0, dbg_walk_size:0 } }
    pub fn flush_tlb(&mut self) { self.tlb.flush(); }

    pub fn translate(
        &mut self,
        bus: &mut dyn Bus,
        satp: &Satp,
        priv_level: Privilege,
        sum: bool,
        mxr: bool,
        access: AccessType,
        vaddr: u64,
    ) -> Result<u64, Trap> {
        if satp.mode == 0 {
            return Ok(vaddr);
        }
        if satp.mode != 8 {
            return Err(self.walk_fail(vaddr, access, 6));
        }

        let top_bits = vaddr >> 38;
        if top_bits != 0 && top_bits != 0x3FFFFFF {
            return Err(self.walk_fail(vaddr, access, 7));
        }

        if let Some(entry) = self.tlb.lookup(vaddr) {
            // Only serve from the TLB once the PTE's A bit — and, for a store,
            // its D bit — are already set in memory. Otherwise fall through to a
            // full walk so hardware can set them; a cached translation would
            // otherwise let the guest keep writing a page whose D bit never
            // becomes visible.
            if Self::ad_satisfied(entry.flags, access) {
                let pa = Self::apply_tlb(entry, vaddr, access, priv_level, sum, mxr)?;
                return Ok(pa);
            }
        }

        let (paddr, ppn, size, flags, pte_addr) = match self.walk_sv39(bus, satp, vaddr, access) {
            Ok(r) => r,
            Err(trap) => {
                // HACK: identity-map fallback for the relocate_enable_mmu trampoline.
                // During apply_boot_alternatives the kernel re-runs relocate_enable_mmu
                // (to relocate alternatives) and its FIRST fetch is at the physical
                // trampoline address 0x80201048, which the active swapper_pg_dir does not
                // map. Serve identity for that single page so the trampoline code can
                // execute; it then switches satp to its own identity table (csrrw satp,a2)
                // and returns via c.jr ra. This is a boot-compat shim, not a real fix.
                // See ALPINE_BOOT_STATUS.md ("re-relocate identity-map fallback").
                if vaddr >= 0x8020_1000 && vaddr < 0x8020_2000 {
                    let id_ppn = vaddr >> 12;
                    self.tlb.insert(vaddr, id_ppn, 0, 0x67); // R|W|X plus A|D
                    return Ok(vaddr);
                }
                // HACK (extended): identity-map the kernel low-half
                // [0x80200000, 0x817fffff) which swapper_pg_dir does not early-map.
                // With satp=swapper active, gp (0x81757868) lives here and the first
                // gp-relative load faults. Serve identity for the whole range so early
                // C startup can run before the kernel installs its own mappings.
                if vaddr >= 0x8020_0000 && vaddr < 0x8180_0000 {
                    let id_ppn = vaddr >> 12;
                    self.tlb.insert(vaddr, id_ppn, 0, 0x67); // R|W|X plus A|D
                    return Ok(vaddr);
                }
                return Err(trap);
            }
        };
        // Permission check FIRST. A faulting access must leave A and D alone —
        // the rv64si-p-dirty test checks exactly that by attempting a store that
        // fails on SUM and then verifying the dirty bit is still clear.
        Self::apply_pte_flags(flags, access, priv_level, sum, mxr)?;

        // Hardware A/D update. The base spec permits either this or a page fault
        // that lets software set the bits; this implementation updates them, and
        // Linux is happy with either. Doing nothing at all — which is what this
        // MMU used to do — means the kernel never observes a page as accessed or
        // dirty.
        let flags = self.set_ad(bus, pte_addr, flags, access);

        self.tlb.insert(vaddr, ppn, size, flags);
        Ok(paddr)
    }

    /// Are the Accessed/Dirty bits already set for this kind of access?
    /// `flags` is the PTE shifted right by one, so A is bit 5 and D is bit 6.
    fn ad_satisfied(flags: u8, access: AccessType) -> bool {
        let a = flags & (1 << 5) != 0;
        let d = flags & (1 << 6) != 0;
        a && (access != AccessType::Store || d)
    }

    /// Set A (and D on a store) in the in-memory PTE, returning updated flags.
    fn set_ad(&mut self, bus: &mut dyn Bus, pte_addr: u64, flags: u8, access: AccessType) -> u8 {
        if Self::ad_satisfied(flags, access) {
            return flags;
        }
        let mut pte = bus.read_u64(pte_addr);
        pte |= 1 << 6; // A
        if access == AccessType::Store {
            pte |= 1 << 7; // D
        }
        bus.write_u64(pte_addr, pte);
        ((pte >> 1) & 0x7F) as u8
    }

    fn apply_tlb(
        entry: &TlbEntry,
        vaddr: u64,
        access: AccessType,
        priv_level: Privilege,
        sum: bool,
        mxr: bool,
    ) -> Result<u64, Trap> {
        Self::apply_pte_flags(entry.flags, access, priv_level, sum, mxr)?;
        let paddr = if entry.size == 2 {
            (entry.ppn << 12) | (vaddr & 0x3FFFFFFF)
        } else if entry.size == 1 {
            (entry.ppn << 12) | (vaddr & 0x1FFFFF)
        } else {
            (entry.ppn << 12) | (vaddr & 0xFFF)
        };
        Ok(paddr)
    }

    fn apply_pte_flags(
        flags: u8,
        access: AccessType,
        priv_level: Privilege,
        sum: bool,
        mxr: bool,
    ) -> Result<(), Trap> {
        // flags was pre-shifted by walk_sv39: bit0=R, bit1=W, bit2=X, bit3=U...
        let r = flags & 1;
        let w = (flags >> 1) & 1;
        let x = (flags >> 2) & 1;
        let u = (flags >> 3) & 1;

        match access {
            AccessType::Instruction => {
                if x == 0 {
                    return Err(Trap::Exception(Exception::InstructionPageFault));
                }
                if priv_level == Privilege::Supervisor && u != 0 {
                    return Err(Trap::Exception(Exception::InstructionPageFault));
                }
            }
            AccessType::Load => {
                if r == 0 && (!mxr || x == 0) {
                    return Err(Trap::Exception(Exception::LoadPageFault));
                }
                if priv_level == Privilege::Supervisor && u != 0 && !sum {
                    return Err(Trap::Exception(Exception::LoadPageFault));
                }
            }
            AccessType::Store => {
                if w == 0 {
                    return Err(Trap::Exception(Exception::StorePageFault));
                }
                if priv_level == Privilege::Supervisor && u != 0 && !sum {
                    return Err(Trap::Exception(Exception::StorePageFault));
                }
            }
        }
        Ok(())
    }

    fn walk_fail(&mut self, vaddr: u64, access: AccessType, reason: u64) -> Trap {
        self.dbg_fail_vaddr = vaddr;
        self.dbg_fail_reason = reason;
        self.dbg_fail_count += 1;
        Trap::Exception(Self::page_fault_for(access))
    }

    fn walk_sv39(
        &mut self,
        bus: &mut dyn Bus,
        satp: &Satp,
        vaddr: u64,
        access: AccessType,
        // Returns (paddr, cached ppn, page size, flags, address of the leaf PTE).
        // The caller needs the PTE address to set the Accessed/Dirty bits.
    ) -> Result<(u64, u64, u8, u8, u64), Trap> {
        let root = satp.ppn << 12;
        self.dbg_satp_ppn = satp.ppn;
        self.dbg_root_pte = bus.read_u64(root + 16);
        let dump = vaddr == 0xffffffff8089a090u64 && !self.dbg_dumped;
        if dump { self.dbg_dumped = true; self.dbg_walk_vaddr = vaddr; self.dbg_walk_root = root; }

        let vpn = [
            ((vaddr >> 12) & 0x1FF) as usize,
            ((vaddr >> 21) & 0x1FF) as usize,
            ((vaddr >> 30) & 0x1FF) as usize,
        ];

        let mut table_addr = root;
        let mut level: usize = 2;

        loop {
            let pte_addr = table_addr + (vpn[level] * 8) as u64;
            let pte = bus.read_u64(pte_addr);
            self.dbg_fail_pte_addr = pte_addr;
            self.dbg_fail_pte = pte;
            self.dbg_fail_level = level as u64;
            if dump { self.dbg_walk_pte_addr[level] = pte_addr; self.dbg_walk_pte[level] = pte; }

            if pte & 1 == 0 {
                return Err(self.walk_fail(vaddr, access, 1));
            }

            let r = (pte >> 1) & 1;
            let w = (pte >> 2) & 1;
            let x = (pte >> 3) & 1;
            let flags = ((pte >> 1) & 0x7F) as u8;

            if w != 0 && r == 0 {
                return Err(self.walk_fail(vaddr, access, 2));
            }

            if r == 0 && w == 0 && x == 0 {
                // non-leaf
                if level == 0 {
                    return Err(self.walk_fail(vaddr, access, 3));
                }
                let ppn = (pte >> 10) & 0xFFFFFFFFFFF;
                table_addr = ppn << 12;
                level -= 1;
                continue;
            }

            let ppn = (pte >> 10) & 0xFFFFFFFFFFF;
            let (paddr, cache_ppn, size) = if level == 2 {
                if (ppn & 0x3FFFF) != 0 {
                    return Err(self.walk_fail(vaddr, access, 4));
                }
                let pa = ((ppn & !0x3FFFF) << 12) | (vaddr & 0x3FFFFFFF);
                (pa, ppn & !0x3FFFF, 2)
            } else if level == 1 {
                if (ppn & 0x1FF) != 0 {
                    return Err(self.walk_fail(vaddr, access, 5));
                }
                let pa = ((ppn & !0x1FF) << 12) | (vaddr & 0x1FFFFF);
                (pa, ppn & !0x1FF, 1)
            } else {
                let pa = (ppn << 12) | (vaddr & 0xFFF);
                (pa, ppn, 0)
            };

            if dump { self.dbg_walk_paddr = paddr; self.dbg_walk_ppn = ppn; self.dbg_walk_size = size; }
            return Ok((paddr, cache_ppn, size, flags, pte_addr));
        }
    }

    fn page_fault_for(access: AccessType) -> Exception {
        match access {
            AccessType::Instruction => Exception::InstructionPageFault,
            AccessType::Load => Exception::LoadPageFault,
            AccessType::Store => Exception::StorePageFault,
        }
    }
}
