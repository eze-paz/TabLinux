#!/usr/bin/env python3
"""
Validate hypothesis: kernel silently panics/wfis after MMU enable
- Run emulator with trap counting
- Check for wfi loops in disassembly
"""
import subprocess
import re
from pathlib import Path

def run_emulator():
    """Run the test and capture trap info"""
    cmd = [
        "cargo", "test", "-p", "riscv-test-harness", "--test", "boot_alpine_full",
        "--", "--ignored", "--nocapture"
    ]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    return result.stdout + result.stderr

def count_traps(output):
    """Count number of traps"""
    trap_count = len(re.findall(r'Exception\(', output))
    return trap_count

def analyze_wfi_in_kernel():
    """Disassemble kernel to check for wfi patterns near hang PC"""
    kernel_path = Path("/home/aezequiel/riscv-vm/kernels/vmlinuz-lts-decompressed.bin")
    
    if not kernel_path.exists():
        print(f"ERROR: Kernel not found at {kernel_path}")
        return
    
    # Find hang PC around 0x80a1399c
    hang_pc = 0x80a1399c
    
    # Disassemble around that area
    objdump_path = "/home/aezequiel/riscv-vm/kernels/xpack-riscv-none-elf-gcc-15.2.0-1/bin/riscv-none-elf-objdump"
    
    # Compute file offset
    load_addr = 0x80200000
    offset = hang_pc - load_addr
    
    if offset < 0:
        print(f"ERROR: hang_pc {hex(hang_pc)} is below load address {hex(load_addr)}")
        return
    
    print(f"Hang PC: {hex(hang_pc)}")
    print(f"File offset: {hex(offset)}")
    
    # Use objdump to disassemble at that offset
    # objdump -D -b binary -m riscv:rv64
    cmd = [
        objdump_path, "-D", "-b", "binary", "-m", "riscv:rv64",
        str(kernel_path)
    ]
    
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
    
    # Find lines around the offset
    lines = result.stdout.split('\n')
    for i, line in enumerate(lines):
        if f"{hang_pc:08x}:" in line or f" {hang_pc:08x}:" in line:
            print(f"\n--- Disassembly around hang PC {hex(hang_pc)} ---")
            # Print context
            start = max(0, i - 3)
            end = min(len(lines), i + 10)
            for j in range(start, end):
                marker = ">>>" if j == i else "   "
                print(f"{marker} {lines[j]}")
            break
    
    # Search for wfi pattern
    wfi_count = result.stdout.count("wfi")
    print(f"\nWFI instruction count in kernel: {wfi_count}")
    
    # Check for panic-like patterns
    panic_patterns = ["panic", "hang", "error", "fail"]
    for pattern in panic_patterns:
        if pattern in result.stdout.lower():
            print(f"WARNING: Found '{pattern}' in disassembly comments")

def main():
    print("=== Running emulator and counting traps ===\n")
    
    output = run_emulator()
    
    trap_count = count_traps(output)
    print(f"Total traps observed: {trap_count}")
    
    # Count page faults specifically
    page_faults = len(re.findall(r'InstructionPageFault', output))
    print(f"Instruction page faults: {page_faults}")
    
    # Find the first trap location
    first_trap = re.search(r'trap at step (\d+).*?sepc=(0x[0-9a-f]+)', output)
    if first_trap:
        print(f"First trap at step {first_trap.group(1)}, sepc={first_trap.group(2)}")
    
    print("\n=== Analyzing kernel for wfi/hang patterns ===\n")
    analyze_wfi_in_kernel()
    
    # Hypothesis validation
    print("\n=== HYPOTHESIS VALIDATION ===")
    if trap_count == 1:
        print("✓ Single trap at MMU enable (EXPECTED - this is correct behavior)")
        print("✗ No additional traps after MMU enable - kernel NOT silently panicking")
        print("  → Hypothesis FALSE: kernel hangs without additional exceptions")
    elif trap_count > 1:
        print(f"✗ Multiple traps ({trap_count}) - kernel experiencing exceptions")
        print("  → Hypothesis possibly TRUE: need to investigate which traps occur")
    else:
        print("  → No traps at all - kernel runs without exceptions")

if __name__ == "__main__":
    main()
