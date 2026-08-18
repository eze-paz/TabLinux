# Phase 1 Design: SBI Console Hello World

Date: 2026-07-05

## Goal
Boot Alpine Linux on the riscv-vm emulator and see it print something to the console.

Phase 1 scope: trap-and-emulate SBI calls so the kernel printk reaches stdout.
We prove the mechanism with a 20-instruction synthetic kernel before touching the
real Alpine binary.

## Approach
Option B (trap-and-emulate SBI in Rust) with Approach A (synthetic test first,
then real kernel).

## Architecture

### SBI Hook Point
Currently, ecall in S-mode raises EnvironmentCallFromS and traps to stvec/mtvec.

Change: In Supervisor.step(), after decoding ecall, check privilege level.
If S-mode, dispatch to sbi_handle_call() instead of treating it as an exception.

    ecall in S-mode
        -> check: priv == Supervisor?
        -> yes: sbi_dispatch(extension=a7, function=a6, args=a0..a5)
        -> handler executes (e.g., writes string to stdout)
        -> write return code to a0/a1
        -> advance PC past ecall
        -> continue execution

M-mode ecall still traps normally (used by firmware, not SBI).

Unknown SBI extensions return SBI_ERR_NOT_SUPPORTED (-2) and advance PC.

## SBI Dispatch

New file: crates/riscv-supervisor/src/sbi.rs

    pub(crate) fn handle_sbi_call(
        cpu: &mut Cpu,
        bus: &mut dyn Bus,
        extension: u64,
        function: u64,
        args: [u64; 6],
    ) -> SbiResult;

Return convention:
- a0 = error code (0 = success)
- a1 = return value

The handler writes a0/a1 directly to CPU registers.

## Debug Console Extension (Phase 1 only)

Extension ID: 0x4442434E (sbi_debug_console)

Function 0 -- sbi_debug_console_write(byte_len, base_addr_in, base_addr_out):
1. Read byte_len bytes from guest memory at base_addr_in via Bus::read_u8
2. Print to host stdout
3. Write (0, written_len) to a0/a1

This single extension is sufficient for observing Linux printk output.

## Synthetic Hello World Test

New test (in mini_boot.rs or dedicated test file):

1. Assemble S-mode program that:
   - Loads pointer to "Hello SBI\n" into a0
   - Loads length 12 into a1
   - Loads extension ID 0x4442434E into a7
   - Loads function 0 into a6
   - Executes ecall
   - Executes ebreak

2. Run Supervisor.step() loop
3. Assert output contains "Hello SBI"

Output capture: Use a callback-based writer so the test can assert without relying
on println side effects.

## Real Alpine Kernel Boot (after synthetic test passes)

### Download
Fetch Alpine RISC-V vmlinuz from Alpine CDN.
Save to: kernels/alpine-vmlinuz-riscv64

### Boot Harness
New/updated in crates/riscv-harness/src/boot.rs:
1. Load kernel blob into DRAM
2. Construct minimal DTB in memory (memory@80000000, clint, plic stubs)
3. Set registers:
   - a0 = 0 (hartid)
   - a1 = dtb_address
   - pc = kernel_entry
4. Run Supervisor.step() loop with CLINT ticking
5. Capture SBI console output

### Memory Layout (QEMU virt compatible)

| Address       | Size  | Device              |
|---------------|-------|---------------------|
| 0x0200_0000   | --    | CLINT               |
| 0x0C00_0000   | --    | PLIC (stub)         |
| 0x1000_1000   | --    | VirtIO console stub |
| 0x8000_0000   | 128MB | DRAM (kernel)       |

Kernel entry point typically 0x8020_0000 (offset into DRAM).
DTB placed after kernel image.

## Error Handling

| Scenario                      | Behavior                                  |
|-------------------------------|-------------------------------------------|
| Unknown SBI extension         | Return -2 (SBI_ERR_NOT_SUPPORTED)         |
| Invalid guest address         | Return -3 (SBI_ERR_INVALID_PARAM)         |
| Null pointer for console write| Return -3, advance PC                     |

No panics. Kernel must handle SBI errors gracefully.

## File Changes

| File                                      | Action |
|-------------------------------------------|--------|
| crates/riscv-supervisor/src/sbi.rs        | New    |
| crates/riscv-supervisor/src/supervisor.rs | Modify |
| crates/riscv-supervisor/src/lib.rs        | Modify |
| crates/riscv-harness/src/mini_boot.rs     | Modify |
| crates/riscv-harness/src/boot.rs          | Modify |
| kernels/alpine-vmlinuz-riscv64            | New    |

## Testing Plan

1. Unit test: cargo test sbi_console_hello
   Assert synthetic program prints "Hello SBI"
2. Integration test: cargo test alpine_boot_prints
   Boot downloaded kernel, assert any output within N steps

## Success Criteria

- [ ] Synthetic SBI hello test passes
- [ ] Real Alpine kernel downloads and loads into memory
- [ ] Alpine kernel executes at least one SBI console write
- [ ] Something from the kernel appears on stdout

## Out of Scope (Phase 2+)

- Full SBI extension set (timer, IPI, hart state)
- MMU page table walking (SV39)
- Functional VirtIO console/block backends
- Functional PLIC interrupt delivery
- User mode (U-mode) trap delegation
- Initramfs / rootfs mounting
- Userspace shell
