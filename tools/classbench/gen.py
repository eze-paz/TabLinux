#!/usr/bin/env python3
"""Generate per-instruction-class guest benchmarks as static RISC-V ELFs.

Stage 2 of the speed plan needs the compiled path's ~10ns/instruction split by
class -- ALU, load, store, branch, jump, indirect jump, FP -- because that
ranking decides whether the next lever is the memory path or a trace optimizer.
The honest way to get it is to run pure-class loops inside the real VM: the
emulator counts retired instructions exactly, so ns/class = wall/steps with no
model in between.

No RISC-V binutils on this box, so: clang's built-in cross assembler makes the
.o, llvm-objcopy extracts the raw text, and this script wraps it in a minimal
static ELF64 by hand. The loops use only local branches with hand-resolved
offsets and immediates, so the text carries no relocations -- gen checks that
claim with llvm-objdump and refuses to ship a binary where it is false.

    python3 tools/classbench/gen.py    # writes /tmp/classbench/cb_*.elf
"""

import os
import struct
import subprocess
import sys

OUT = '/tmp/classbench'
CLANG = 'clang'
OBJCOPY = 'llvm-objcopy-18'
OBJDUMP = 'llvm-objdump-18'

# 16 body instructions per iteration (24 for ind), plus the 2-instruction loop
# counter. Interleaved registers so nothing artificially serialises.


def body_alu():
    regs = ['a1', 'a2', 'a3', 'a4']
    return '\n'.join(f'    addi {regs[i % 4]}, {regs[i % 4]}, 1' for i in range(16))


def body_load():
    return '\n'.join('    ld a1, 0(sp)' for _ in range(16))


def body_store():
    return '\n'.join('    sd a1, 0(sp)' for _ in range(16))


def body_fp():
    return '\n'.join('    fadd.d fa0, fa0, fa1' for _ in range(16))


def body_fpl():
    return '\n'.join('    fld fa0, 0(sp)' for _ in range(16))


def body_fps():
    return '\n'.join('    fsd fa0, 0(sp)' for _ in range(16))


def body_br():
    # Never-taken branches: t1 != t2, so each falls through. The loop's own
    # back-edge is the taken-branch case and is present in every class alike.
    return '\n'.join(f'    beq t1, t2, 1f\n1:' for _ in range(16))


def body_jmp():
    # Direct jumps to the next instruction.
    return '\n'.join('    j 1f\n1:' for _ in range(16))


def body_ind():
    # Indirect jumps: auipc at X, jalr lands at X+12, immediately after it.
    # Hand-resolved so the assembler emits no relocation. Each jalr ends a
    # compiled block, so this prices the block-boundary/chain path.
    return '\n'.join('    auipc t1, 0\n    addi t1, t1, 12\n    jalr zero, 0(t1)'
                     for _ in range(8))


CLASSES = {
    # name: (iterations, setup, body, instructions per iteration)
    'alu':   (10_000_000, '', body_alu(), 18),
    'load':  (10_000_000, '', body_load(), 18),
    'store': (10_000_000, '', body_store(), 18),
    'br':    (10_000_000, '    li t1, 1\n    li t2, 2', body_br(), 18),
    'jmp':   (10_000_000, '', body_jmp(), 18),
    'ind':   (2_000_000, '', body_ind(), 26),
    'fp':    (1_000_000, '    li a1, 1\n    fcvt.d.l fa0, a1\n    fcvt.d.l fa1, a1',
              body_fp(), 18),
    # FP loads/stores: the flag-free traffic stage 1.5 inlines. The fcvt in the
    # setup makes the task's FP state dirty, so the loop runs with FS == Dirty
    # -- the state the inline path is gated on.
    'fpl':   (2_000_000, '    li a1, 1\n    fcvt.d.l fa0, a1', body_fpl(), 18),
    'fps':   (2_000_000, '    li a1, 1\n    fcvt.d.l fa0, a1', body_fps(), 18),
}

TEMPLATE = '''    .text
    .global _start
_start:
{setup}
    li t0, {n}
loop:
{body}
    addi t0, t0, -1
    bnez t0, loop
    li a0, 0
    li a7, 93
    ecall
'''


def wrap_elf(text: bytes) -> bytes:
    """A minimal static ELF64: one RX PT_LOAD at 0x10000, entry at its start."""
    vaddr = 0x10000
    off = 0x1000  # congruent with vaddr mod page size, as the loader requires
    ehdr = struct.pack(
        '<16sHHIQQQIHHHHHH',
        b'\x7fELF\x02\x01\x01\x00' + b'\x00' * 8,  # 64-bit LSB SysV
        2, 243, 1,                                  # ET_EXEC, EM_RISCV, v1
        vaddr,                                      # entry
        64, 0,                                      # phoff, shoff
        0x4,                                        # e_flags: double-float ABI
        64, 56, 1,                                  # ehsize, phentsize, phnum
        0, 0, 0)                                    # no sections
    phdr = struct.pack(
        '<IIQQQQQQ',
        1, 5,                                       # PT_LOAD, R+X
        off, vaddr, vaddr,
        len(text), len(text), 0x1000)
    img = bytearray(ehdr + phdr)
    img += b'\x00' * (off - len(img))
    img += text
    return bytes(img)


def main():
    os.makedirs(OUT, exist_ok=True)
    for name, (n, setup, body, ipi) in CLASSES.items():
        asm = os.path.join(OUT, f'cb_{name}.S')
        obj = os.path.join(OUT, f'cb_{name}.o')
        elf = os.path.join(OUT, f'cb_{name}.elf')
        with open(asm, 'w') as f:
            f.write(TEMPLATE.format(setup=setup, n=n, body=body))
        subprocess.run(
            [CLANG, '--target=riscv64-linux-gnu', '-march=rv64g', '-mno-relax',
             '-c', asm, '-o', obj],
            check=True)
        # The whole scheme rests on the text having no relocations; verify
        # rather than assume.
        rel = subprocess.run([OBJDUMP, '-r', obj], capture_output=True, text=True).stdout
        if 'R_RISCV' in rel:
            sys.exit(f'{name}: unresolved relocations, the binary would be garbage:\n{rel}')
        subprocess.run([OBJCOPY, '-O', 'binary', '--only-section=.text', obj, elf + '.bin'],
                       check=True)
        with open(elf + '.bin', 'rb') as f:
            text = f.read()
        with open(elf, 'wb') as f:
            f.write(wrap_elf(text))
        os.remove(elf + '.bin')
        total = n * ipi
        print(f'{name:6} {len(text):5} B text  {n:>10,} iters  ~{total / 1e6:,.0f}M instrs')
    print(f'\nwrote {len(CLASSES)} benchmarks to {OUT}/')


if __name__ == '__main__':
    main()
