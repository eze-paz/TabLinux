//! DeviceBus integration test — DRAM, CLINT, console output

#[cfg(test)]
mod tests {
    use riscv_core::execute::Bus;
    use crate::DeviceBus;

    #[test]
    fn dram_roundtrip() {
        let mut bus = DeviceBus::new(256 * 1024); // 256KB
        bus.write_u64(0x8000_0000, 0xDEADBEEFCAFEBABE);
        assert_eq!(bus.read_u64(0x8000_0000), 0xDEADBEEFCAFEBABE);
    }

    #[test]
    fn dram_byte_write_readback() {
        let mut bus = DeviceBus::new(4096);
        bus.write_u8(0x8000_0000, 0x42);
        assert_eq!(bus.read_u32(0x8000_0000), 0x42);
    }

    #[test]
    fn clint_mtimecmp() {
        let mut bus = DeviceBus::new(4096);
        bus.write_u64(0x0200_4000, 0x123456789ABCDEF0); // mtimecmp
        assert_eq!(bus.read_u64(0x0200_4000), 0x123456789ABCDEF0);
    }

    /// `tick()` runs once per retired instruction and mtime advances once every
    /// `MTIME_STEPS_PER_TICK` of them. Time deliberately runs slower than the
    /// code observing it — see the comment on `DeviceBus::tick`.
    #[test]
    fn clint_timer_counts() {
        let mut bus = DeviceBus::new(4096);
        let t0 = bus.read_u64(0x0200_BFF8);
        for _ in 0..10 {
            bus.tick();
        }
        assert_eq!(bus.read_u64(0x0200_BFF8), t0 + 1, "10 instructions == 1 mtime tick");

        for _ in 0..9 {
            bus.tick();
        }
        assert_eq!(bus.read_u64(0x0200_BFF8), t0 + 1, "a partial tick must not advance mtime");
        bus.tick();
        assert_eq!(bus.read_u64(0x0200_BFF8), t0 + 2);
    }

    /// The 8250 asserts its PLIC line as soon as the guest enables the
    /// transmit-holding-register interrupt (TX completes instantly here), and
    /// the PLIC only surfaces it to a context that has enabled the source and
    /// set a priority above its threshold.
    #[test]
    fn uart_tx_interrupt_reaches_plic() {
        const PLIC: u64 = 0x0C00_0000;
        const UART_IRQ: u64 = 10;
        const S_CTX: u64 = 1; // hart0 supervisor context

        let mut bus = DeviceBus::new(4096);
        assert!(!bus.external_interrupt_pending(), "idle UART must not assert");

        // Enable IER.THRI without configuring the PLIC: still nothing claimable.
        bus.write_u8(0x1000_0000 + 1, 0x02);
        assert!(!bus.external_interrupt_pending(), "PLIC not configured yet");

        bus.write_u32(PLIC + 4 * UART_IRQ, 7); // priority
        bus.write_u32(PLIC + 0x2000 + 0x80 * S_CTX, 1 << UART_IRQ); // enable
        bus.write_u32(PLIC + 0x20_0000 + 0x1000 * S_CTX, 0); // threshold
        assert!(bus.external_interrupt_pending(), "THRI should assert SEIP");

        // Claiming takes the source out of service until it is completed.
        let claimed = bus.read_u32(PLIC + 0x20_0000 + 0x1000 * S_CTX + 4);
        assert_eq!(claimed, UART_IRQ as u32);
        assert!(!bus.external_interrupt_pending(), "claimed irq must not re-assert");

        bus.write_u32(PLIC + 0x20_0000 + 0x1000 * S_CTX + 4, UART_IRQ as u32); // complete
        assert!(bus.external_interrupt_pending(), "level is still high after complete");

        // Disabling THRI drops the line for good.
        bus.write_u8(0x1000_0000 + 1, 0x00);
        assert!(!bus.external_interrupt_pending());
    }

    /// Console input shows up in LSR.DR and is popped byte-by-byte from the RBR.
    #[test]
    fn uart_rx_path() {
        let mut bus = DeviceBus::new(4096);
        assert_eq!(bus.read_u8(0x1000_0000 + 5) & 0x01, 0, "no data ready when idle");

        bus.uart_push_input(b"hi");
        assert_eq!(bus.read_u8(0x1000_0000 + 5) & 0x01, 1, "DR set with input queued");
        assert_eq!(bus.read_u8(0x1000_0000), b'h');
        assert_eq!(bus.read_u8(0x1000_0000), b'i');
        assert_eq!(bus.read_u8(0x1000_0000 + 5) & 0x01, 0, "DR clears when drained");
    }

    /// 0x10001000 is virtio-mmio slot 0, not a character device. Two tests here
    /// used to assert the old stub's behaviour — that bytes written to that
    /// address landed in a console string, and that MagicValue echoed back
    /// whatever you stored to it. Both are wrong for real hardware: the
    /// transport's identity registers are read-only, and an unpopulated slot
    /// must read as zero so the driver skips it.
    /// A bus with no block device must grow one rather than refuse.
    ///
    /// Only `load_state` created the device before, so a restored machine had a
    /// disk and a cold-booted one silently did not: the attach returned false,
    /// the caller had nothing useful to do with that, and the guest came up
    /// with no /dev/vda and no persistence. Nothing cold booted in the browser
    /// until the RAM selector existed, which is why it went unnoticed.
    #[test]
    fn attaching_a_backend_creates_the_block_device_when_there_is_none() {
        use alloc::boxed::Box;
        let mut bus = DeviceBus::new(4096);
        let backend = crate::virtio_blk::MemBackend::new(alloc::vec![0u8; 512]);
        assert!(bus.attach_blk_backend(Box::new(backend)), "must attach");
        assert_eq!(bus.read_u32(0x1000_1000), 0x7472_6976, "MagicValue");
        assert_eq!(bus.read_u32(0x1000_1008), 2, "DeviceID must be block");
    }

    /// An existing device is reused, not duplicated — a restore already has
    /// one, and a second would consume another slot for nothing.
    ///
    /// Same capacity on both, because rebinding a different one is refused on
    /// purpose: that check is what stops a disk changing size underneath a
    /// filesystem. Writing this test with mismatched sizes is how I confirmed
    /// the refusal still works.
    #[test]
    fn attaching_a_backend_reuses_an_existing_block_device() {
        use alloc::boxed::Box;
        let mut bus = DeviceBus::new(4096);
        bus.attach_virtio(Box::new(crate::virtio_blk::VirtioBlk::new(Box::new(
            crate::virtio_blk::MemBackend::new(alloc::vec![0u8; 512]),
        ))));
        assert!(bus.attach_blk_backend(Box::new(crate::virtio_blk::MemBackend::new(
            alloc::vec![0u8; 512]
        ))));
        assert_eq!(bus.read_u32(0x1000_2000), 0, "slot 1 must stay empty");
    }

    /// And rebinding a differently-sized disk is still refused.
    #[test]
    fn attaching_a_backend_refuses_a_capacity_change() {
        use alloc::boxed::Box;
        let mut bus = DeviceBus::new(4096);
        bus.attach_virtio(Box::new(crate::virtio_blk::VirtioBlk::new(Box::new(
            crate::virtio_blk::MemBackend::new(alloc::vec![0u8; 512]),
        ))));
        assert!(
            !bus.attach_blk_backend(Box::new(crate::virtio_blk::MemBackend::new(
                alloc::vec![0u8; 4096]
            ))),
            "a disk that changed size under the guest must be refused"
        );
    }

    #[test]
    fn virtio_identity_registers_are_read_only() {
        use alloc::boxed::Box;
        let mut bus = DeviceBus::new(4096);
        let backend = crate::virtio_blk::MemBackend::new(alloc::vec![0u8; 512]);
        bus.attach_virtio(Box::new(crate::virtio_blk::VirtioBlk::new(Box::new(backend))));

        assert_eq!(bus.read_u32(0x1000_1000), 0x7472_6976, "MagicValue");
        bus.write_u32(0x1000_1000, 0xDEAD_BEEF);
        assert_eq!(
            bus.read_u32(0x1000_1000),
            0x7472_6976,
            "MagicValue must be read-only"
        );
    }

    #[test]
    fn unpopulated_virtio_slot_reads_zero() {
        let bus = DeviceBus::new(4096);
        assert_eq!(bus.read_u32(0x1000_1000), 0, "no device attached");
        assert_eq!(bus.read_u32(0x1000_8000), 0, "last slot");
    }
}
