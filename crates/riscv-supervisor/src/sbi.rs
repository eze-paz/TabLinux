use riscv_core::execute::Bus;
extern crate alloc;

const SBI_EXT_BASE: u64 = 0x10;
const SBI_EXT_TIME: u64 = 0x54494D45;
const SBI_EXT_IPI: u64 = 0x735049;
const SBI_EXT_HSM: u64 = 0x48534D;
const SBI_EXT_SRST: u64 = 0x53525354;
const SBI_EXT_DBCN: u64 = 0x4442434E;
const SBI_EXT_RFENCE: u64 = 0x52464E43;
const SBI_SUCCESS: i64 = 0;
const SBI_ERR_NOT_SUPPORTED: i64 = -2;
const SBI_ERR_INVALID_PARAM: i64 = -3;

// Legacy SBI extension IDs
const SBI_LEGACY_SET_TIMER: u64 = 0x00;
const SBI_LEGACY_CONSOLE_PUTCHAR: u64 = 0x01;
const SBI_LEGACY_CONSOLE_GETCHAR: u64 = 0x02;
const SBI_LEGACY_CLEAR_IPI: u64 = 0x03;
const SBI_LEGACY_SEND_IPI: u64 = 0x04;
const SBI_LEGACY_REMOTE_FENCE_I: u64 = 0x05;
const SBI_LEGACY_REMOTE_SFENCE_VMA: u64 = 0x06;
const SBI_LEGACY_REMOTE_SFENCE_VMA_ASID: u64 = 0x07;
const SBI_LEGACY_SHUTDOWN: u64 = 0x08;

pub trait HostConsole {
    fn write(&mut self, buf: &[u8]);
}

pub struct SbiRet {
    pub error: i64,
    pub value: i64,
}

pub struct SbiRetExt {
    pub ext: u64,
    pub error: i64,
    pub value: i64,
}

pub fn handle_ecall(
    bus: &mut dyn Bus,
    a0: u64, a1: u64, _a2: u64, _a3: u64, _a4: u64, _a5: u64,
    a6: u64, a7: u64,
    console: &mut dyn HostConsole,
) -> SbiRetExt {
    let ret = match a7 {
        // Modern SBI extensions
        SBI_EXT_DBCN => handle_dbcn(a0, a1, bus, console),
        SBI_EXT_BASE => handle_base(a0, a1, a6),
        SBI_EXT_TIME => handle_time(a0, a1, bus),
        SBI_EXT_HSM => handle_hsm(a0, a6),
        SBI_EXT_SRST => handle_srst(a0, a6),
        SBI_EXT_IPI => handle_ipi(a0, a6),
        SBI_EXT_RFENCE => handle_rfence(a0, a6),

        // Legacy SBI extensions (EID 0x01-0x09)
        SBI_LEGACY_SET_TIMER => {
            bus.write_u64(0x0200_4000, a0);
            SbiRet { error: 0, value: 0 }
        }
        SBI_LEGACY_CONSOLE_PUTCHAR => {
            let ch = (a0 & 0xFF) as u8;
            console.write(&[ch]);
            SbiRet { error: 0, value: 0 }
        }
        SBI_LEGACY_CONSOLE_GETCHAR => {
            // No input device, return -1
            SbiRet { error: 0, value: -1 }
        }
        SBI_LEGACY_CLEAR_IPI => {
            // Single-hart, no IPI to clear
            SbiRet { error: 0, value: 0 }
        }
        SBI_LEGACY_SEND_IPI => {
            // Single-hart, IPI targeting self is a no-op
            SbiRet { error: 0, value: 0 }
        }
        SBI_LEGACY_REMOTE_FENCE_I => {
            SbiRet { error: 0, value: 0 }
        }
        SBI_LEGACY_REMOTE_SFENCE_VMA => {
            // If the remote hart is hart 0 (us), flush TLB
            SbiRet { error: 0, value: 0 }
        }
        SBI_LEGACY_REMOTE_SFENCE_VMA_ASID => {
            SbiRet { error: 0, value: 0 }
        }
        SBI_LEGACY_SHUTDOWN => {
            // Signal shutdown by returning a special value
            // The supervisor will detect this and stop execution
            SbiRet { error: 0, value: -1 }
        }

        _ => SbiRet { error: SBI_ERR_NOT_SUPPORTED, value: 0 },
    };
    SbiRetExt { ext: a7, error: ret.error, value: ret.value }
}

fn handle_base(a0: u64, _a1: u64, function: u64) -> SbiRet {
    match function {
        0 => SbiRet { error: SBI_SUCCESS, value: 0x02000000 },
        1 => SbiRet { error: SBI_SUCCESS, value: 0x1 },
        2 => SbiRet { error: SBI_SUCCESS, value: 0x02000000 },
        3 => {
            let available = matches!(a0, SBI_EXT_BASE | SBI_EXT_TIME | SBI_EXT_DBCN |
                SBI_EXT_IPI | SBI_EXT_HSM | SBI_EXT_SRST | SBI_EXT_RFENCE |
                SBI_LEGACY_SET_TIMER | SBI_LEGACY_CONSOLE_PUTCHAR | SBI_LEGACY_CONSOLE_GETCHAR |
                SBI_LEGACY_CLEAR_IPI | SBI_LEGACY_SEND_IPI | SBI_LEGACY_REMOTE_FENCE_I |
                SBI_LEGACY_REMOTE_SFENCE_VMA | SBI_LEGACY_REMOTE_SFENCE_VMA_ASID | SBI_LEGACY_SHUTDOWN);
            SbiRet { error: SBI_SUCCESS, value: if available { 1 } else { 0 } }
        }
        // get_mvendorid / get_marchid / get_mimpid. Answering NOT_SUPPORTED made
        // the kernel map the error to -ENOTSUPP and print *that* as the ID, so
        // /proc/cpuinfo read `mvendorid : 0xfffffffffffffdf4`. Zero is the
        // architectural "non-commercial implementation" value and is what
        // OpenSBI reports on the QEMU virt machine.
        4 | 5 | 6 => SbiRet { error: SBI_SUCCESS, value: 0 },
        _ => SbiRet { error: SBI_ERR_NOT_SUPPORTED, value: 0 },
    }
}

fn handle_hsm(_a0: u64, function: u64) -> SbiRet {
    match function {
        // hart_start (0): single-hart emulator - no secondary harts exist.
        // Returning SBI_SUCCESS made the kernel believe hart 1 came online and jump
        // the boot CPU into the SMP secondary trampoline (VA 0x80201048), which faults.
        // Rejecting makes cpu_up(1) fail gracefully; kernel boots with one CPU.
        0 => SbiRet { error: SBI_ERR_INVALID_PARAM, value: 0 },
        _ => SbiRet { error: SBI_ERR_NOT_SUPPORTED, value: 0 },
    }
}

fn handle_srst(_a0: u64, function: u64) -> SbiRet {
    match function {
        0 => SbiRet { error: SBI_SUCCESS, value: 0 },
        _ => SbiRet { error: SBI_ERR_NOT_SUPPORTED, value: 0 },
    }
}

fn handle_ipi(_a0: u64, function: u64) -> SbiRet {
    match function {
        0 => SbiRet { error: SBI_SUCCESS, value: 0 },
        _ => SbiRet { error: SBI_ERR_NOT_SUPPORTED, value: 0 },
    }
}

fn handle_rfence(_a0: u64, function: u64) -> SbiRet {
    match function {
        0 => SbiRet { error: SBI_SUCCESS, value: 0 },
        _ => SbiRet { error: SBI_ERR_NOT_SUPPORTED, value: 0 },
    }
}

const PAGE_OFFSET: u64 = 0xffffffff80000000;
const PHYS_OFFSET: u64 = 0x80000000;

fn va_to_pa(va: u64) -> u64 {
    if va >= PAGE_OFFSET {
        // Kernel direct-mapped VA: PA = VA - PAGE_OFFSET + PHYS_OFFSET
        va.wrapping_sub(PAGE_OFFSET).wrapping_add(PHYS_OFFSET)
    } else {
        va // Already a physical address or user-space VA (pass through)
    }
}

fn handle_dbcn(
    byte_len: u64, base_addr_in: u64,
    bus: &mut dyn Bus, console: &mut dyn HostConsole,
) -> SbiRet {
    if byte_len == 0 {
        return SbiRet { error: SBI_SUCCESS, value: 0 };
    }
    let len = byte_len as usize;
    let mut chunk = [0u8; 256];
    let mut written = 0usize;
    let base_pa = va_to_pa(base_addr_in);
    while written < len {
        let to_read = (len - written).min(256);
        for i in 0..to_read {
            chunk[i] = bus.read_u8(base_pa.wrapping_add((written + i) as u64));
        }
        console.write(&chunk[..to_read]);
        written += to_read;
    }
    SbiRet { error: SBI_SUCCESS, value: byte_len as i64 }
}

fn handle_time(a0: u64, a1: u64, bus: &mut dyn Bus) -> SbiRet {
    let stime = ((a1 as u64) << 32) | (a0 as u64);
    bus.write_u64(0x0200_4000, stime);
    SbiRet { error: SBI_SUCCESS, value: 0 }
}
