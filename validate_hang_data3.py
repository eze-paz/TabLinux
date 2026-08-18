#!/usr/bin/env python3
"""
Validate the hang hypothesis by examining the raw kernel binary at crash addresses.
"""

import subprocess
import struct
from pathlib import Path

def main():
    print("=" * 70)
    print("KERNEL HANG HYPOTHESIS VALIDATION")
    print("Testing: Kernel hangs due to uninitialized/corrupted data structures")
    print("=" * 70)
    
    kernel_path = Path("kernels/vmlinuz-lts-decompressed.bin")
    with open(kernel_path, "rb") as f:
        data = f.read()
    
    print(f"\nKernel binary: {kernel_path}, size={len(data)} bytes")
    
    # The kernel is loaded at 0x80200000
    # So virtual address VA maps to file offset = VA - 0x80200000
    # For addresses like 0x80a1399c (in .text/.data section)
    
    objdump = "/home/aezequiel/riscv-vm/kernels/xpack-riscv-none-elf-gcc-15.2.0-1/bin/riscv-none-elf-objdump"
    
    # First, let's disassemble the hang region to understand the loop
    print("\n=== Disassembly of hang loop at 0x80a1399c ===")
    cmd = f"{objdump} -D -b binary -m riscv:riscv64 --adjust-vma=0x80200000 kernels/vmlinuz-lts-decompressed.bin 2>&1 | grep -A 50 \"0x80a139[0-9a-f]{1}:\""
    result = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    print(result.stdout[:3000])
    
    # Now check the data at 0x81558ca0 and 0x81558cb8
    # These are in the .bss or uninitialized section
    # 0x81558ca0 - 0x80200000 = 0x1558ca0
    offset1 = 0x1558ca0  # 0x81558ca0 - 0x80200000
    offset2 = 0x1558cb8  # 0x81558cb8 - 0x80200000
    
    print(f"\n=== Checking address 0x81558ca0 (file offset 0x{offset1:x}) ===")
    if offset1 < len(data):
        chunk1 = data[offset1:offset1+32]
        print(f"Raw bytes: {chunk1.hex()}")
        print(f"Length: {len(chunk1)}")
        
        # Interpret as 64-bit values
        print("\n64-bit values:")
        for i in range(0, min(32, len(chunk1)-7), 8):
            val = int.from_bytes(chunk1[i:i+8], 'little')
            print(f"  {hex(offset1 + i)}: {val:#018x}")
    else:
        print(f"ERROR: offset {hex(offset1)} is beyond kernel size {len(data)}!")
    
    print(f"\n=== Checking address 0x81558cb8 (file offset 0x{offset2:x}) ===")
    if offset2 < len(data):
        chunk2 = data[offset2:offset2+32]
        print(f"Raw bytes: {chunk2.hex()}")
        print(f"Length: {len(chunk2)}")
        
        print("\n64-bit values:")
        for i in range(0, min(32, len(chunk2)-7), 8):
            val = int.from_bytes(chunk2[i:i+8], 'little')
            print(f"  {hex(offset2 + i)}: {val:#018x}")
    else:
        print(f"ERROR: offset {hex(offset2)} is beyond kernel size {len(data)}!")
    
    # Now check what's at the actual hang address 0x80a1399c
    offset_hang = 0xa1399c  # 0x80a1399c - 0x80200000
    print(f"\n=== Checking address 0x80a1399c (file offset 0x{offset_hang:x}) ===")
    if offset_hang < len(data):
        chunk_h = data[offset_hang:offset_hang+32]
        print(f"Raw bytes: {chunk_h.hex()}")
        
        # Interpret as 16-bit compressed instructions
        print("\n16-bit values (potential compressed instructions):")
        for i in range(0, min(32, len(chunk_h)-1), 2):
            val = int.from_bytes(chunk_h[i:i+2], 'little')
            print(f"  {hex(offset_hang + i)}: 0x{val:04x}")
        
        # Try to disassemble a few bytes around here
        print("\nDisassembly with objdump:")
        cmd = f"{objdump} -D -b binary -m riscv:riscv64 --adjust-vma=0x80200000 kernels/vmlinuz-lts-decompressed.bin 2>&1 | grep -E \"0x80a139[89a]:|0x80a139[bcd]:|0x80a139e[0-9a-f]:|0x80a13a[0-9a-f]{2}:\" | head -20"
        result = subprocess.run(cmd, shell=True, capture_output=True, text=True)
        print(result.stdout[:1000])
    else:
        print(f"ERROR: offset {hex(offset_hang)} is beyond kernel size {len(data)}!")
    
    # Analyze the data - check if 0x81558ca0 is in .text or .data or .bss
    print("\n=== ANALYSIS ===")
    print(f"Kernel size: {len(data)} bytes = 0x{len(data):x}")
    print(f"Offset 0x1558ca0 is {'WITHIN' if offset1 < len(data) else 'BEYOND'} kernel")
    print(f"Offset 0xa1399c is {'WITHIN' if offset_hang < len(data) else 'BEYOND'} kernel")
    
    # Check what kind of data is at 0x1558ca0
    if offset1 < len(data):
        # Check if it looks like valid data or zeros
        chunk1 = data[offset1:offset1+32]
        non_zero = sum(1 for b in chunk1 if b != 0)
        print(f"\nAt 0x1558ca0: {non_zero}/32 bytes are non-zero ({non_zero*100//32}% data)")
        if non_zero == 0:
            print("  -> This is .bss/uninitialized memory (all zeros)")
            print("  -> HYPOTHESIS VALIDATED: kernel is walking through zeros as if they were valid data")
        elif non_zero > 16:
            print("  -> This appears to be initialized data (.data or .text)")
        else:
            print("  -> Partially initialized - checking further...")
    
    print("\n" + "=" * 70)
    print("VALIDATION COMPLETE")
    print("=" * 70)

if __name__ == "__main__":
    main()
