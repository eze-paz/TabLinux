#!/usr/bin/env python3
"""
Differential decoder sweep: compare our Rust decoder against objdump
for every instruction in the kernel text section.
"""
import subprocess
import sys
import struct
from pathlib import Path

OBJDUMP = "/home/aezequiel/riscv-vm/kernels/xpack-riscv-none-elf-gcc-15.2.0-1/bin/riscv-none-elf-objdump"
TEXT_SECTION = "kernels/text_section.raw"

def run_objdump():
    """Run objdump on the text section and parse output."""
    cmd = [OBJDUMP, "-D", "-b", "binary", "-m", "riscv:rv64", "-M", "no-aliases", TEXT_SECTION]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    if result.returncode != 0:
        print("objdump failed:", result.stderr)
        sys.exit(1)
    return result.stdout

def parse_objdump(output):
    """Parse objdump output into list of (offset, raw_bytes, mnemonic, operands)."""
    instructions = []
    for line in output.split('\n'):
        # Skip header lines
        if not line.strip() or line.startswith(' ') or 'file format' in line:
            continue
        # Format: "   0:\t6f 00 00 00 \tjal\tzero,0x0"
        parts = line.split('\t')
        if len(parts) < 3:
            continue
        offset_str = parts[0].strip().rstrip(':')
        try:
            offset = int(offset_str, 16)
        except ValueError:
            continue
        raw_bytes = bytes.fromhex(parts[1].strip().replace(' ', ''))
        mnemonic = parts[2].strip()
        operands = parts[3].strip() if len(parts) > 3 else ""
        instructions.append((offset, raw_bytes, mnemonic, operands))
    return instructions

def get_raw_at(data, offset):
    """Get the raw instruction word at given offset."""
    if offset + 2 > len(data):
        return None
    lo = struct.unpack('<H', data[offset:offset+2])[0]
    if (lo & 0x3) != 0x3:
        # Compressed instruction
        return lo
    if offset + 4 > len(data):
        return None
    hi = struct.unpack('<H', data[offset+2:offset+4])[0]
    return (hi << 16) | lo

def decode_our(raw):
    """Call our Rust decoder via a small test program."""
    # We'll create a temporary Rust test that decodes this instruction
    # For now, let's just use the existing decode test infrastructure
    pass

def main():
    with open(TEXT_SECTION, 'rb') as f:
        data = f.read()
    
    print("Running objdump...")
    objdump_out = run_objdump()
    instructions = parse_objdump(objdump_out)
    print(f"Parsed {len(instructions)} instructions from objdump")
    
    # For now, just verify we can parse the output correctly
    # and check for any suspicious patterns
    compressed_count = 0
    for offset, raw_bytes, mnemonic, operands in instructions:
        if len(raw_bytes) == 2:
            compressed_count += 1
    
    print(f"Compressed instructions: {compressed_count}")
    print(f"First 10 instructions:")
    for i in range(min(10, len(instructions))):
        offset, raw_bytes, mnemonic, operands = instructions[i]
        print(f"  {offset:06x}: {raw_bytes.hex():>8}  {mnemonic} {operands}")

if __name__ == "__main__":
    main()
