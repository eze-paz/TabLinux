#!/usr/bin/env python3
"""
Validate the hang hypothesis: Kernel hangs in infinite loop in .data section 
due to uninitialized/incorrect data structures in memory allocator/slab 
initialization.

This script:
1. Extracts .data section from kernel binary
2. Dumps data at suspicious addresses (0xffffffff81558ca0, 0xffffffff81558cb8)
3. Analyzes the data structures to find bounds/termination issues
"""

import subprocess
import sys
from pathlib import Path

def run_cmd(cmd):
    """Run shell command and return output"""
    result = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    return result.stdout, result.stderr, result.returncode

def read_kernel_binary():
    """Read kernel binary"""
    kernel_path = Path("kernels/vmlinuz-lts-decompressed.bin")
    if not kernel_path.exists():
        print(f"ERROR: Kernel not found at {kernel_path}")
        sys.exit(1)
    
    with open(kernel_path, "rb") as f:
        data = f.read()
    
    print(f"Kernel binary: {kernel_path}")
    print(f"Size: {len(data)} bytes")
    return data

def find_section_vma_sections(data):
    """
    Parse ELF to find section VMA/RVA mappings.
    ELF64 header at offset 0.
    """
    if data[:4] != b'\x7fELF':
        print("ERROR: Not an ELF file")
        sys.exit(1)
    
    ei_class = data[4]  # 1=32-bit, 2=64-bit
    if ei_class != 2:
        print(f"ERROR: Expected 64-bit ELF, got {ei_class}")
        sys.exit(1)
    
    ei_data = data[5]  # 1=LE, 2=BE
    if ei_data != 1:
        print(f"ERROR: Expected little-endian, got {ei_data}")
        sys.exit(1)
    
    e_type = int.from_bytes(data[16:18], 'little')
    if e_type != 2:  # ET_EXEC
        print(f"WARNING: ELF type {e_type}, expected 2 (EXEC)")
    
    e_shoff = int.from_bytes(data[40:48], 'little')  # Section header offset
    e_shentsize = int.from_bytes(data[58:60], 'little')  # Section header entry size
    e_shnum = int.from_bytes(data[60:62], 'little')  # Number of section headers
    e_shstrndx = int.from_bytes(data[62:64], 'little')  # Section name string table index
    
    print(f"ELF: class=64, endian=LE, type={e_type}, shoff={e_shoff}, shentsize={e_shentsize}, shnum={e_shnum}, shstrndx={e_shstrndx}")
    
    # Read all section headers
    sections = []
    for i in range(e_shnum):
        sh_offset = e_shoff + i * e_shentsize
        sh_name = int.from_bytes(data[sh_offset:sh_offset+4], 'little')
        sh_type = int.from_bytes(data[sh_offset+4:sh_offset+8], 'little')
        sh_flags = int.from_bytes(data[sh_offset+8:sh_offset+16], 'little')
        sh_addr = int.from_bytes(data[sh_offset+16:sh_offset+24], 'little')  # VMA
        sh_offset2 = int.from_bytes(data[sh_offset+24:sh_offset+32], 'little')  # File offset
        sh_size = int.from_bytes(data[sh_offset+32:sh_offset+40], 'little')
        sh_link = int.from_bytes(data[sh_offset+40:sh_offset+44], 'little')
        sh_info = int.from_bytes(data[sh_offset+44:sh_offset+48], 'little')
        sh_addralign = int.from_bytes(data[sh_offset+48:sh_offset+56], 'little')
        sh_entsize = int.from_bytes(data[sh_offset+56:sh_offset+64], 'little')
        
        sections.append({
            'sh_name': sh_name,
            'sh_type': sh_type,
            'sh_flags': sh_flags,
            'sh_addr': sh_addr,  # VMA
            'sh_offset': sh_offset2,  # File offset
            'sh_size': sh_size,
            'sh_link': sh_link,
            'sh_info': sh_info,
            'sh_addralign': sh_addralign,
            'sh_entsize': sh_entsize,
        })
    
    # Read section names from string table
    shstrtab = sections[e_shstrndx]
    shstrtab_data = data[shstrtab['sh_offset']:shstrtab['sh_offset']+shstrtab['sh_size']]
    
    for sec in sections:
        name_start = shstrtab['sh_offset'] + sec['sh_name']
        name_end = shstrtab_data.find(b'\x00', name_start - shstrtab['sh_offset'])
        if name_end >= 0:
            sec['name'] = shstrtab_data[name_start:name_end].decode('utf-8', errors='replace')
        else:
            sec['name'] = f"<unknown_{sec['sh_name']}>"
    
    return sections

def dump_memory(data, sections, vaddr):
    """
    Dump memory at virtual address.
    For .data section, the kernel loads at 0x80200000, so:
    phys_addr = vaddr - 0xffffffff80000000 + 0x80200000
               = vaddr - 0x7FFFFFFF80000000
    Actually, let's use the ELF program headers to determine load address.
    """
    # Kernel entry is at 0x80200000, and .data section is part of the kernel
    # VMA of .data is in the ELF, and it's loaded at its VMA
    # So 0xffffffff80a1399c is in .text/.data section which is at ~0x80a1399c
    # The kernel is loaded at 0x80200000, so virtual addresses in kernel are at VMA
    
    # Find which section contains this address
    print(f"\nDumping addresses around {hex(vaddr)}:")
    
    for sec in sections:
        if 'data' in sec['name'].lower() or 'text' in sec['name'].lower() or 'bss' in sec['name'].lower():
            if sec['sh_addr'] <= vaddr < sec['sh_addr'] + sec['sh_size']:
                offset_in_section = vaddr - sec['sh_addr']
                file_offset = sec['sh_offset'] + offset_in_section
                print(f"  Found in section: {sec['name']} (VMA={hex(sec['sh_addr'])}, size={sec['sh_size']})")
                print(f"  Offset in section: {offset_in_section}, file offset: {file_offset}")
                
                # Dump 64 bytes around the address
                start = max(0, file_offset - 32)
                end = min(len(data), file_offset + 64)
                
                print(f"  Raw bytes (hex):")
                hex_str = ""
                for i in range(start, end):
                    if i % 16 == 0:
                        hex_str += f"\n    {i:08x}: "
                    hex_str += f"{data[i]:02x} "
                print(hex_str)
                
                # Try to interpret as 64-bit values
                print(f"\n  64-bit values around {hex(vaddr)}:")
                for offset in range(-64, 64, 8):
                    check_addr = file_offset + offset
                    if 0 <= check_addr + 7 < len(data):
                        val = int.from_bytes(data[check_addr:check_addr+8], 'little')
                        print(f"    {hex(check_addr)}: {val:#018x} (0x{val:016x})")
                
                return sec
    
    print(f"  Address {hex(vaddr)} not found in any section!")
    return None

def analyze_infinite_loop():
    """
    Main analysis: find what data structure causes the infinite loop.
    
    From the observation:
    - Hang at 0xffffffff80a1399c–0xffffffff80a13ec2
    - s6 base addresses: 0xffffffff81558ca0, 0xffffffff81558cb8
    - Loop logic: load value -> shift -> add -> compare -> branch back
    - Termination condition s5 >= a4 never met
    """
    print("=" * 70)
    print("KERNEL HANG HYPOTHESIS VALIDATION")
    print("=" * 70)
    
    # Read kernel
    data = read_kernel_binary()
    
    # Parse ELF
    sections = find_section_vma_sections(data)
    
    print("\n" + "=" * 70)
    print("SECTION TABLE (interesting sections):")
    print("=" * 70)
    for sec in sections:
        if any(x in sec['name'].lower() for x in ['data', 'text', 'bss', 'rodata', 'init', '.data']):
            print(f"{sec['name']:20s} VMA={sec['sh_addr']:016x} size={sec['sh_size']:10d} file_off={sec['sh_offset']:08x}")
    
    # Address to analyze: s6 base addresses from hang loop
    s6_addr1 = 0xffffffff81558ca0
    s6_addr2 = 0xffffffff81558cb8
    hang_addr = 0xffffffff80a1399c
    
    print("\n" + "=" * 70)
    print(f"ANALYZING HANG LOOP DATA STRUCTURES")
    print("=" * 70)
    
    print(f"\n1. Hang loop address: {hex(hang_addr)}")
    dump_memory(data, sections, hang_addr)
    
    print(f"\n2. First s6 base: {hex(s6_addr1)}")
    dump_memory(data, sections, s6_addr1)
    
    print(f"\n3. Second s6 base: {hex(s6_addr2)}")
    dump_memory(data, sections, s6_addr2)
    
    # Check if these addresses are within kernel sections
    print("\n" + "=" * 70)
    print("VERIFICATION: Are these addresses in valid kernel sections?")
    print("=" * 70)
    
    for addr, name in [(s6_addr1, "s6_1"), (s6_addr2, "s6_2"), (hang_addr, "hang")]:
        in_section = False
        for sec in sections:
            if sec['sh_addr'] <= addr < sec['sh_addr'] + sec['sh_size']:
                print(f"{name} ({hex(addr)}): in section {sec['name']}")
                in_section = True
                break
        if not in_section:
            print(f"{name} ({hex(addr)}): NOT in any section! HYPOTHESIS VALIDATED - uninitialized memory access")
    
    # Also dump objdisassembly of the loop
    print("\n" + "=" * 70)
    print("DISASSEMBLY OF HANG LOOP (objdump)")
    print("=" * 70)
    
    # Find physical offset of hang_addr
    # The kernel is loaded at 0x80200000, so we need to subtract the base
    # But first let's find which section contains it
    objdump_path = "/home/aezequiel/riscv-vm/kernels/xpack-riscv-none-elf-gcc-15.2.0-1/bin/riscv-none-elf-objdump"
    
    # Find the section containing hang_addr
    target_sec = None
    for sec in sections:
        if sec['sh_addr'] <= hang_addr < sec['sh_addr'] + sec['sh_size']:
            target_sec = sec
            break
    
    if target_sec:
        # Calculate relative offset in section
        rel_offset = hang_addr - target_sec['sh_addr']
        
        # Use objdump to disassemble starting from this offset
        cmd = f"{objdump_path} -D -b binary -m riscv:riscv32 --adjust-vma={target_sec['sh_addr']:x} kernels/vmlinuz-lts-decompressed.bin | grep -A 30 \"^{hex(target_sec['sh_addr']+rel_offset):x}:\""
        stdout, stderr, rc = run_cmd(cmd)
        
        if stdout:
            print(stdout)
        else:
            # Try a broader range
            print("Trying broader disassembly...")
            start_addr = target_sec['sh_addr'] + max(0, rel_offset - 0x100)
            end_addr = target_sec['sh_addr'] + min(target_sec['sh_size'], rel_offset + 0x200)
            cmd = f"{objdump_path} -D -b binary -m riscv:riscv32 --adjust-vma={target_sec['sh_addr']:x} kernels/vmlinuz-lts-decompressed.bin | grep -E \"^{hex(start_addr):x}:|^{hex(end_addr):x}:|\" | head -100"
            stdout, stderr, rc = run_cmd(cmd)
            print(stdout)
    
    print("\n" + "=" * 70)
    print("ANALYSIS COMPLETE")
    print("=" * 70)
    
    # Summary
    print("\nSUMMARY:")
    print("-" * 70)
    
    s6_1_in_section = any(sec['sh_addr'] <= s6_addr1 < sec['sh_addr'] + sec['sh_size'] for sec in sections)
    s6_2_in_section = any(sec['sh_addr'] <= s6_addr2 < sec['sh_addr'] + sec['sh_size'] for sec in sections)
    
    if not s6_1_in_section or not s6_2_in_section:
        print("✓ HYPOTHESIS VALIDATED: Data addresses point to uninitialized memory!")
        print(f"  - s6_1={hex(s6_addr1)}: {'✓ in section' if s6_1_in_section else '✗ NOT in section'}")
        print(f"  - s6_2={hex(s6_addr2)}: {'✓ in section' if s6_2_in_section else '✗ NOT in section'}")
        print("  The kernel is walking through uninitialized/bss memory as if it were")
        print("  a valid data structure, causing the infinite loop.")
    else:
        print("Data structures are in valid sections.")
        print("Hypothesis may be FALSE - investigating further...")
        print("\nThis suggests the problem is NOT uninitialized data, but rather:")
        print("- Corrupted pointers within valid structures")
        print("- Incorrect bounds/termination conditions")
        print("- Wrong data structure layout interpretation")

if __name__ == "__main__":
    analyze_infinite_loop()
