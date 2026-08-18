#!/usr/bin/env python3
"""Generate a minimal FDT/DTB for RISC-V virt machine."""
import struct, sys

def pad4(n):
    return (n + 3) & ~3

class FdtBuilder:
    def __init__(self):
        self.struct = b''
        self.strings = b''
        self.strtab = {}
        self.rsvmap = [(0, 0)]  # terminator

    def _add_string(self, s):
        if s not in self.strtab:
            self.strtab[s] = len(self.strings)
            self.strings += s.encode('ascii') + b'\x00'
        return self.strtab[s]

    def begin_node(self, name):
        self.struct += struct.pack('>I', 1) + name.encode('ascii') + b'\x00'
        self.struct += b'\x00' * (pad4(len(name) + 1) - (len(name) + 1))

    def end_node(self):
        self.struct += struct.pack('>I', 2)

    def prop(self, name, data):
        nameoff = self._add_string(name)
        self.struct += struct.pack('>III', 3, len(data), nameoff) + data
        self.struct += b'\x00' * (pad4(len(data)) - len(data))

    def end(self):
        self.struct += struct.pack('>I', 9)

    def build(self):
        # Header is 40 bytes
        header_size = 40
        rsvmap_offset = header_size
        rsvmap_size = len(self.rsvmap) * 16
        struct_offset = rsvmap_offset + rsvmap_size
        # Pad struct_offset to 4-byte aligned (already aligned since 40+16=56)
        struct_offset = pad4(struct_offset)
        strings_offset = struct_offset + pad4(len(self.struct))
        totalsize = strings_offset + pad4(len(self.strings))

        header = struct.pack('>IIIIIIIIII',
            0xd00dfeed,
            totalsize,
            struct_offset,
            strings_offset,
            rsvmap_offset,
            17,              # version
            16,              # last_comp_version
            0,               # boot_cpuid_phys
            len(self.strings),  # size_dt_strings
            len(self.struct),   # size_dt_struct
        )
        rsvmap_data = b''.join(struct.pack('>QQ', addr, size) for addr, size in self.rsvmap)
        return header + rsvmap_data + self.struct + b'\x00' * (pad4(len(self.struct)) - len(self.struct)) + self.strings + b'\x00' * (pad4(len(self.strings)) - len(self.strings))

if __name__ == '__main__':
    fdt = FdtBuilder()
    fdt.begin_node('')
    fdt.prop('model', b'riscv-virtio')
    fdt.prop('#address-cells', struct.pack('>I', 2))
    fdt.prop('#size-cells', struct.pack('>I', 2))

    # cpus
    fdt.begin_node('cpus')
    fdt.prop('#address-cells', struct.pack('>I', 1))
    fdt.prop('#size-cells', struct.pack('>I', 0))
    fdt.prop('timebase-frequency', struct.pack('>I', 10000000))

    fdt.begin_node('cpu@0')
    fdt.prop('device_type', b'cpu')
    fdt.prop('reg', struct.pack('>I', 0))
    fdt.prop('status', b'okay')
    fdt.prop('compatible', b'riscv')
    fdt.prop('riscv,isa', b'rv64imafdc_zicsr_zifencei')
    fdt.prop('mmu-type', b'sv39')
    fdt.begin_node('interrupt-controller')
    fdt.prop('#interrupt-cells', struct.pack('>I', 1))
    fdt.prop('interrupt-controller', b'')
    fdt.prop('compatible', b'riscv,cpu-intc')
    fdt.end_node()
    fdt.end_node()
    fdt.end_node()

    # memory
    fdt.begin_node('memory@80000000')
    fdt.prop('device_type', b'memory')
    fdt.prop('reg', struct.pack('>II', 0x80000000, 0x10000000))  # 256MB @ 2GB
    fdt.end_node()

    # soc / clint
    fdt.begin_node('soc')
    fdt.prop('#address-cells', struct.pack('>I', 2))
    fdt.prop('#size-cells', struct.pack('>I', 2))
    fdt.prop('compatible', b'simple-bus')
    fdt.prop('ranges', b'')

    fdt.begin_node('clint@2000000')
    fdt.prop('compatible', b'riscv,clint0')
    fdt.prop('reg', struct.pack('>IIII', 0, 0x02000000, 0, 0x00010000))
    fdt.prop('interrupts-extended', struct.pack('>II', 1, 3) + struct.pack('>II', 1, 7))  # phandle, int
    fdt.end_node()
    fdt.end_node()

    fdt.end_node()  # root
    fdt.end()

    data = fdt.build()
    out = sys.argv[1] if len(sys.argv) > 1 else 'virt.dtb'
    with open(out, 'wb') as f:
        f.write(data)
    print(f"Wrote {len(data)} bytes -> {out}")
