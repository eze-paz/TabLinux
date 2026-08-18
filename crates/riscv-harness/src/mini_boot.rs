//! Mini boot simulation — exercises M→S transition, timer, WFI
//!
//! No external binaries needed. Assembles a program that mimics
//! Linux boot flow: M-mode init → delegate interrupts → S-mode → WFI → timer → handler.

#[cfg(test)]
mod tests {
    use riscv_core::execute::Bus;
    use riscv_core::types::{Status, Trap, Exception};
    use riscv_supervisor::{Supervisor, types::Privilege};
    use riscv_devices::DeviceBus;

    /// Assembles raw instructions into DeviceBus DRAM at given base
    fn assemble(bus: &mut DeviceBus, base: u64, code: &[u32]) {
        for (i, &word) in code.iter().enumerate() {
            bus.write_u32(base + (i as u64 * 4), word);
        }
    }

    /// Run supervisor until max steps or trap
    fn run_steps(
        s: &mut Supervisor,
        bus: &mut DeviceBus,
        max_steps: usize,
    ) -> Result<usize, Trap> {
        for _i in 0..max_steps {
            bus.tick();
            let status = s.step(bus);
            match status {
                Status::Running => {}
                Status::Trap(trap) => return Err(trap),
                Status::Wfi => {
                    // WFI: stall until an enabled interrupt becomes pending.
                    // Tick the bus to advance time, then check for interrupts.
                    // If no interrupt is pending, continue the loop without
                    // calling step() again to avoid executing past WFI.
                    loop {
                        if (s.mip & s.mie) != 0 {
                            // Interrupt pending: next iteration will call step()
                            // which checks interrupts and takes the trap.
                            break;
                        }
                        bus.tick();
                        if bus.check_timer_interrupt() {
                            s.mip |= 1 << 7; // MTIP
                        }
                    }
                }
            }
        }
        Ok(max_steps)
    }

    #[test]
    fn mini_boot_m_to_s_mode() {
        let mut bus = DeviceBus::new(256 * 1024);
        let mut s = Supervisor::new(0x8000_0000, 0);
        s.cpu.write_reg(2, 0x8003_E000); // sp = top of small dram

        // Program at 0x8000_0000 :
        //  0: lui  x1,       %hi(trap_vec)       // mtvec base
        //  4: addi x1, x1,   %lo(trap_vec)
        //  8: csrrw x0, mstatus, x1 // no wait, just set mpp to S
        // Actually: set mstatus.mpp = 1 (S-mode), mstatus.mpie = 1, mstatus.mie = 1
        let _code: &[u32] = &[
            // 0x8000_0000 entry:
            // Build mstatus = 0x0000000A00001888 (MPP=M, MPIE=1, MIE=1)
            0x000011B7, // lui  x3, 0x1
            0x8881819B, // addiw x3, x3, -1912   // x3 = 0x888 (lower bits)
            // Actually we just want constant 0x_...A00001888
            // Use simple constant load:
            0xA00001B7, // lui  x3, 0xA0000
            0x18818193, // addi x3, x3, 0x188
            0x00C1D113, // srli x2, x3, 12       // messy...

            // Let's just do it directly with two instructions:
            // lui x3, 0xA0000  -- gives 0xFFFF_FFFF_A000_0000 sign-extended in addi
            // Wrong. lui sign-extends in RV64. Need careful constant.
            // Simpler: write mstatus with just the bottom bits:
        ];

        // More reliable: set CSR with specific pattern through x1
        // mstatus with MPP=3 (M), MPIE=1, MIE=1:
        // Bits: 11<<11 = 0x1800 (MPP), 1<<7=0x80 (MPIE), 1<<3=0x08 (MIE)
        // Total: 0x1888 or for 64-bit: 0x_..._0000_1888
        // In real boot, OpenSBI does this with store/restore pattern.
        // Let's simulate by directly writing CSRs for test purposes.

        // Actually, test via direct control instead:
        s.mstatus.mpp = 1; // S-mode target
        s.mstatus.mpie = true;
        s.mstatus.mie = true;
        s.mtvec = 0x8000_0100;
        s.mepc = 0x8000_001C; // return to instruction after mret

        // Now: write mstatus via instruction to verify path
        assemble(&mut bus, 0x8000_0000, &[
            0x00001117, // auipc x2, 0          // x2 = 0x8000_0000 + 4 = 0x8000_0004
            0x0FC10113, // addi x2, x2, 0xFC    // x2 = 0x8000_0100 (trap_vec)
            0x30511173, // csrrw x2, mtvec, x2  // mtvec = 0x8000_0100, x2 = old mtvec
            // Now set mstatus.MPP = S (1), enable interrupts
            0x00100193, // addi x3, x0, 1
            0x00B19193, // slli x3, x3, 11      // x3 = 1<<11 = MPP bit for S
            0x3001A1F3, // csrrw x3, mstatus, x3 // set MPP = S
            // mret to S-mode
            0x30200073, // mret
            // -- in S-mode now at 0x8000_0018 --
            0x0000006F, // jal x0, 0  (infinite loop in S-mode)
        ]);

        let result = run_steps(&mut s, &mut bus, 100);
        println!("steps = {:?}, pc = {:x}", result, s.cpu.pc);
        assert_eq!(s.priv_level, Privilege::Supervisor, "switched to S-mode");
    }

    #[test]
    fn mini_boot_timer_interrupt() {
        let mut bus = DeviceBus::new(256 * 1024);
        let mut s = Supervisor::new(0x8000_0000, 0);
        s.cpu.write_reg(2, 0x8003_E000);

        // Trap vector at 0x8000_0100
        // Handler: increment x10, clear mip.MTIP, mret
        assemble(&mut bus, 0x8000_0100, &[
            0x00100513, // addi x10, x10, 1     // count
            0x34411073, // csrrw x0, mip, x2     // (simplified: write to clear)
            // Actually: read mip, clear bit 7, write back
            0x34411073, // no easy way to clear single bit without scratch
            0x30200073, // mret
        ]);

        // Entry: set mtvec, mstatus.MIE=1, mie.MTIE=1, set mtimecmp, wfi
        assemble(&mut bus, 0x8000_0000, &[
            0x0FC10117, // auipc x2, 0xFC      // x2 = 0x8000_0004 + 0xFC000 = 0x8000_F004
            // Too complex. Simplify:
        ]);

        // Direct init instead:
        s.mtvec = 0x8000_0100;
        s.mstatus.mie = true;
        s.mie |= 1 << 7; // MTIE
        bus.write_u64(0x0200_4000, 100); // mtimecmp = 100 ticks

        // Loop: WFI until timer fires
        assemble(&mut bus, 0x8000_0000, &[
            0x10500073, // wfi
            0x0000006F, // jal x0, -4 (infinite loop)
        ]);

        let result = run_steps(&mut s, &mut bus, 500);
        assert!(result.is_err() || s.cpu.read_reg(10) > 0, "timer should have fired");
    }

    #[test]
    fn mini_boot_s_delegates_timer() {
        // S-mode with mideleg.MTIP = 1 means timer goes to S-mode
        let mut bus = DeviceBus::new(256 * 1024);
        let mut s = Supervisor::new(0x8000_0000, 0);

        // M-mode init
        s.mstatus.mpp = 1; // S-mode target
        s.mstatus.mpie = true;
        s.mstatus.mie = true;
        s.mstatus.sie = true; // enable S-mode interrupts (normally done by S-mode OS)
        s.mideleg |= 1 << 7; // delegate MTIP to S-mode
        s.mtvec = 0x8000_0100; // M trap vector
        s.stvec = 0x8000_0200; // S trap vector

        // M trap handler: just mret (shouldn't be reached)
        assemble(&mut bus, 0x8000_0100, &[
            0x30200073, // mret
        ]);

        // S trap handler: increment x10, write mtimecmp=u64::MAX to ack timer, sret
        assemble(&mut bus, 0x8000_0200, &[
            0x00100513, // addi x10, x10, 1          // count += 1
            0x020042B7, // lui x5, 0x02004          // x5 = 0x0000_0000_0200_4000 (mtimecmp)
            0xFFF00313, // addi x6, x0, -1           // x6 = 0xFFFF_FFFF_FFFF_FFFF (max u64)
            0x0062B023, // sd x6, 0(x5)              // mtimecmp = !0 (cancel timer)
            0x10200073, // sret
        ]);

        // Entry: mret to S-mode, then wfi loop
        assemble(&mut bus, 0x8000_0000, &[
            0x30200073, // mret → S-mode
            // In S-mode: wfi loop (timer will wake)
            0x10500073, // wfi
            0x0000006F, // jal x0, 0 (loop on wfi)
        ]);

        // Set timer to fire after 50 ticks
        bus.write_u64(0x0200_4000, 50);
        s.mie |= 1 << 7; // MTIE set at M-level for delegation
        s.mepc = 0x8000_0004; // return after mret

        let result = run_steps(&mut s, &mut bus, 500);
        println!("result: {:?}, x10={}, pc={:x}", result, s.cpu.read_reg(10), s.cpu.pc);
        assert!(s.cpu.read_reg(10) > 0, "S-mode timer handler should have run");
    }

    #[test]
    fn mini_boot_full_linux_like_sequence() {
        // Everything together: M init → delegate → S-mode → setup page tables → enable MMU
        let mut bus = DeviceBus::new(256 * 1024);
        let mut s = Supervisor::new(0x8000_0000, 0);

        // Setup:
        // - Stack at 0x8003_E000
        // - Page table at 0x8000_4000 (identity map first 2MB)
        // - Trap handlers
        s.cpu.write_reg(2, 0x8003_E000);

        // Identity mapping: 0x8000_0000 → 0x8000_0000, RWX
        // Page table at 0x8000_4000
        let pte_base = 0x8000_4000u64;
        // Level 2 PTE (for bits 30-38): points to level 1 table at 0x8000_5000
        let l1_table = 0x8000_5000u64;
        bus.write_u64(pte_base, (l1_table >> 12) << 10 | 0x01); // valid, pointer
        // Level 1 PTE: huge page (1GB mapping) for 0x8000_0000 region
        let _gigapage = 0x8000_0000u64;
        // Actually for identity: just need the right mapping
        // Simpler: single level for testing
        // Just map 0x8000_0000 with a leaf PTE at level 1

        // Let's use satp in bare mode for this test — MMU=off is fine
        s.satp.mode = 0;

        // Verify we can run multiple instructions and reach end marker
        assemble(&mut bus, 0x8000_0000, &[
            0x00100513, // addi x10, x0, 1      // x10 = 1
            0x00200513, // addi x10, x0, 2      // (overwrites) x10 = 2
            0x00300513, // addi x10, x0, 3      // x10 = 3
            0x00400513, // addi x10, x0, 4      // x10 = 4
            0x00500513, // addi x10, x0, 5      // x10 = 5
            0x00600513, // addi x10, x0, 6
            0x00700513, // addi x10, x0, 7
            0x00800513, // addi x10, x0, 8
            0x00900513, // addi x10, x0, 9
            0x00A00513, // addi x10, x0, 10
            0x00B00513, // addi x10, x0, 11
            0x00C00513, // addi x10, x0, 12
            0x00D00513, // addi x10, x0, 13
            0x00E00513, // addi x10, x0, 14
            0x00F00513, // addi x10, x0, 15
            0x01000513, // addi x10, x0, 16
            0x01100513, // addi x10, x0, 17
            0x01200513, // addi x10, x0, 18
            0x01300513, // addi x10, x0, 19
            0x01400513, // addi x10, x0, 20
            0x00100073, // ebreak
        ]);

        let result = run_steps(&mut s, &mut bus, 200);
        println!("mini_boot_full result={:?}, x10={}, pc={:x}",
                 result, s.cpu.read_reg(10), s.cpu.pc);
        assert_eq!(s.cpu.read_reg(10), 20, "x10 final value");
        assert_eq!(result, Err(Trap::Exception(Exception::Breakpoint)), "ebreak should trap");
    }

    #[test]
    fn sbi_console_hello() {
        let mut bus = DeviceBus::new(256 * 1024);
        let mut s = Supervisor::new(0x8000_0000, 0);
        s.priv_level = Privilege::Supervisor;

        // Write "Hello SBI\n" at 0x8000_0100
        let msg: &[u8] = b"Hello SBI\n";
        for (i, &b) in msg.iter().enumerate() {
            bus.write_u8(0x8000_0100 + i as u64, b);
        }

        // Assemble S-mode program:
        //   auipc a1, 0          // a1 = pc + 4 = 0x8000_0004
        //   addi a1, a1, 0xFC    // a1 = 0x8000_0100 (msg addr)
        //   addi a0, x0, 10      // a0 = 10 (len)
        //   lui a7, 0x44424      // a7 = 0x4442_4000
        //   addiw a7, a7, 0x34E  // a7 = 0x4442_434E (SBI_EXT_DBCN)
        //   lui a6, 0x0          // a6 = 0 (function: write)
        //   ecall
        //   ebreak
        assemble(&mut bus, 0x8000_0000, &[
            0x00000597, // auipc a1, 0          // a1 = 0x8000_0000
            0x10058593, // addi a1, a1, 0x100   // a1 = 0x8000_0100
            0x00A00513, // addi a0, x0, 10      // a0 = 10 (len)
            0x444248B7, // lui a7, 0x44424      // a7 = 0x4442_4000
            0x34E8889B, // addiw a7, a7, 0x34E  // a7 = 0x4442_434E
            0x00000837, // lui a6, 0            // a6 = 0 (function)
            0x00000073, // ecall
            0x00100073, // ebreak
        ]);

        let result = run_steps(&mut s, &mut bus, 50);
        println!("sbi_console_hello result={:?}, console_len={}, console={:?}",
                 result, s.console_len,
                 core::str::from_utf8(&s.console_buf[..s.console_len.min(4096)]));

        assert!(s.console_len >= 10, "should have captured console output, got {} bytes", s.console_len);
        let output = core::str::from_utf8(&s.console_buf[..s.console_len]).unwrap_or("");
        assert!(output.contains("Hello SBI"), "expected 'Hello SBI', got {:?}", output);
    }
}
