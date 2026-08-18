#!/usr/bin/env python3
"""
Validate the hang hypothesis by examining data at crash addresses.
"""

import subprocess
from pathlib import Path

def main():
    print("=" * 70)
    print("KERNEL HANG HYPOTHESIS VALIDATION")
    print("=" * 70)
    
    # Check if vmlinux exists
    vmlinux_path = Path("kernels/vmlinux")
    if not vmlinux_path.exists():
        print("vmlinux not found - trying with vmlinuz-lts-decompressed.bin")
        return
    
    objdump = "/home/aezequiel/riscv-vm/kernels/xpack-riscv-none-elf-gcc-15.2.0-1/bin/riscv-none-elf-objdump"
    
    # Get .data section contents
    print("\n=== .data section (first 512 bytes) ===")
    cmd = f"{objdump} -s -j .data {vmlinux_path} 2>&1"
    result = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    print(result.stdout[:4000])
    
    # Check if address 0xffffffff81558ca0 is in .data
    print("\n=== Check if 0x81558ca0 is in .data section ===")
    cmd = f"{objdump} -h {vmlinux_path} 2>&1 | grep .data"
    result = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    print(result.stdout)
    
    # Get the .data section offset and size
    # Then check what's at the specific offset
    print("\n=== Disassembly of hang function ===")
    cmd = f"{objdump} -d {vmlinux_path} --start-address=0x80a13900 --stop-address=0x80a14000 2>&1 | head -150"
    result = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    print(result.stdout)

if __name__ == "__main__":
    main()
