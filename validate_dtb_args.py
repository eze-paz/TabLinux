#!/usr/bin/env python3
"""Validate DTB initrd address hypothesis."""
import re
from pathlib import Path

# Read boot_alpine_full.rs to find gen_dtb_v2.py call
with open('/home/aezequiel/riscv-vm/crates/riscv-test-harness/tests/boot_alpine_full.rs', 'r') as f:
    content = f.read()

# Find gen_dtb_v2.py invocation
print("=" * 60)
print("LOCATING gen_dtb_v2.py INVOCATION IN boot_alpine_full.rs")
print("=" * 60)

for i, line in enumerate(content.split('\n'), 1):
    if 'gen_dtb_v2' in line or 'initrd_start' in line or 'initrd_end' in line or 'initrd_load' in line:
        print(f"Line {i}: {line}")

print("\n" + "=" * 60)
print("CALCULATING INITRD LOAD ADDRESS FROM CODE")
print("=" * 60)

DRAM_BASE = 0x80000000
DRAM_SIZE = 0x40000000

initrd_path_match = re.search(r"initrd.*=.*r#\"([^\"]+)\"", content)
if initrd_path_match:
    initrd_path = initrd_path_match.group(1)
    print(f"Initrd path: {initrd_path}")
    initrd_file = Path(initrd_path)
    if initrd_file.exists():
        initrd_size = initrd_file.stat().st_size
        print(f"Initrd size: {initrd_size} bytes (0x{initrd_size:x})")
        initrd_start_calc = DRAM_BASE + DRAM_SIZE - initrd_size - 0x1000000
        initrd_end_calc = initrd_start_calc + initrd_size
        
        print(f"\nActual harness calculation:")
        print(f"  Calculated start: 0x{initrd_start_calc:x}")
        print(f"  Calculated end:   0x{initrd_end_calc:x}")
        print(f"  DTB start:        0xbe9e0000")
        print(f"  DTB end:          0xbefff58a")
        print(f"\n  MISMATCH DETECTED!")
        print(f"  Difference: {initrd_start_calc - 0xbe9e0000:#x} bytes ({abs(initrd_start_calc - 0xbe9e0000)//(1024*1024)} MB)")
    else:
        print(f"Initrd file not found at {initrd_path}")
else:
    print("Could not find initrd path in file")
    
# Show actual gen_dtb_v2.py call
print("\n" + "=" * 60)
print("EXTRACTING gen_dtb_v2.py ACTUAL ARGS FROM CODE")
print("=" * 60)

# Find the section that calls gen_dtb_v2.py
lines = content.split('\n')
for i, line in enumerate(lines):
    if 'gen_dtb_v2.py' in line:
        # Print context
        start = max(0, i-3)
        end = min(len(lines), i+10)
        for j in range(start, end):
            marker = ">>>" if j == i else "   "
            print(f"{marker} Line {j+1}: {lines[j]}")
        break

