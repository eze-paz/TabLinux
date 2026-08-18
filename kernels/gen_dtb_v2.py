#!/usr/bin/env python3
"""Generate a proper FDT/DTB for the RISC-V virt machine to boot Linux/Alpine.

Strategy: start from QEMU's reference DTB (kernels/virt_template.dtb, produced by
`qemu-system-riscv64 -machine virt,dumpdtb=...`), which is a devicetree the kernel
accepts (QEMU boots Alpine 6.18.35 to PID1 with it). We then:
  * switch mmu-type riscv,sv57 -> riscv,sv39  (our emulator only implements Sv39)
  * set bootargs for the emulator (ttyS0 + earlycon=sbi + lpj)
  * set the initrd start/end to the addresses the harness loads the initramfs at
The full node set (cpus/cpu, pmu, soc with uart/clint/plic/virtio/pci, chosen, ...)
is preserved verbatim so the kernel's early DT scan finds everything it needs.
"""
import struct, sys, os

HERE = os.path.dirname(os.path.abspath(__file__))
TEMPLATE = os.path.join(HERE, 'virt_template.dtb')

# NOTE on rdinit/root: Alpine's initramfs-lts ships its init as **/init** (a busybox
# shell script); /sbin is only a symlink to usr/sbin and holds no `init`. Asking for
# `rdinit=/sbin/init` made the kernel log
#   check access for rdinit=/sbin/init failed: -2, ignoring
# fall through to prepare_namespace, and panic on `root=/dev/ram0` (we attach no block
# device). Dropping both leaves the default rdinit=/init, which exists.
# cryptomgr.notests: skip the crypto self-tests every algorithm registration
# runs at boot. On real hardware they are milliseconds; at interpreter speed
# they were a visible slice of the boot-time PC histogram (keccakf_round,
# sha256 self-checks). Nothing in this VM depends on the manager test rig.
_BASE_BOOTARGS = (
    "earlycon=uart8250,mmio32,0x10000000 console=ttyS0,115200 "
    "loglevel={loglevel} log_buf_len=16M lpj=1000000 maxcpus=1 riscv_isa_fallback "
    "cryptomgr.notests"
).format(loglevel=os.environ.get('BOOT_LOGLEVEL', '7'))

# Booting off a disk instead of stopping in the initramfs.
#
# ext4 and the virtio drivers are MODULES in this kernel, not built in (checked
# against usr/lib/modules/.../modules.builtin), so the kernel alone cannot mount
# a root filesystem. Alpine's initramfs is the thing that can: given `root=`, its
# init modprobes everything in `modules=`, runs nlplug-findfs to wait for the
# device node to appear, mounts it on /sysroot and switch_roots into it.
# `rootflags=rw` matters — the initramfs defaults to `ro`, and a read-only root
# would defeat the point of having persistent storage.
ROOT_DEVICE = os.environ.get('ROOT_DEVICE', '')
if ROOT_DEVICE:
    DEFAULT_BOOTARGS = (
        _BASE_BOOTARGS
        + " root=" + ROOT_DEVICE
        + " rootfstype=ext4 rootflags=rw"
        + " modules=virtio_mmio,virtio_blk,virtio_net,ext4"
    )
else:
    DEFAULT_BOOTARGS = _BASE_BOOTARGS


def align4(n):
    return (n + 3) & ~3


def parse(path):
    data = open(path, 'rb').read()
    (magic, totalsize, off_struct, off_strings, off_rsv, version,
     last_comp, boot_cpu, size_strings, size_struct) = struct.unpack('>IIIIIIIIII', data[:40])
    assert magic == 0xd00dfeed, hex(magic)
    strings = data[off_strings:off_strings + size_strings]

    def get_str(off):
        end = strings.index(b'\x00', off)
        return strings[off:end]

    pos = off_struct

    def ru32():
        nonlocal pos
        v = struct.unpack('>I', data[pos:pos + 4])[0]
        pos += 4
        return v

    def rstr():
        nonlocal pos
        end = data.index(b'\x00', pos)
        s = data[pos:end]
        pos = align4(end + 1)
        return s

    root = None
    stack = []
    while True:
        tok = ru32()
        if tok == 1:
            node = {'name': rstr().decode('ascii', 'replace'), 'props': [], 'children': []}
            if root is None:
                root = node
            else:
                stack[-1]['children'].append(node)
            stack.append(node)
        elif tok == 2:
            stack.pop()
        elif tok == 3:
            plen = ru32()
            nameoff = ru32()
            pdata = data[pos:pos + plen]
            pos = align4(pos + plen)
            stack[-1]['props'].append({'name': get_str(nameoff).decode('ascii', 'replace'), 'data': pdata})
        elif tok == 9:
            break
    return root


def serialize(root):
    strtab = {}
    strdata = b''

    def str_off(s):
        nonlocal strdata
        if s in strtab:
            return strtab[s]
        off = len(strdata)
        strtab[s] = off
        strdata += s.encode() + b'\x00'
        return off

    struct_b = b''

    def emit(node):
        nonlocal struct_b
        nm = node['name'].encode()
        struct_b += struct.pack('>I', 1) + nm + b'\x00'
        struct_b += b'\x00' * (align4(len(nm) + 1) - (len(nm) + 1))
        for p in node['props']:
            nameoff = str_off(p['name'])
            plen = len(p['data'])
            struct_b += struct.pack('>III', 3, plen, nameoff) + p['data']
            struct_b += b'\x00' * (align4(plen) - plen)
        for c in node['children']:
            emit(c)
        struct_b += struct.pack('>I', 2)

    emit(root)
    struct_b += struct.pack('>I', 9)

    header_size = 40
    rsvmap_size = 16
    struct_offset = align4(header_size + rsvmap_size)
    struct_padded = align4(len(struct_b))
    strings_offset = struct_offset + struct_padded
    strings_padded = align4(len(strdata))
    totalsize = strings_offset + strings_padded

    header = struct.pack('>IIIIIIIIII',
        0xd00dfeed, totalsize, struct_offset, strings_offset,
        header_size, 17, 16, 0, len(strdata), len(struct_b))
    out = header + struct.pack('>QQ', 0, 0)
    out += b'\x00' * (struct_offset - header_size - rsvmap_size)
    out += struct_b + b'\x00' * (struct_padded - len(struct_b))
    out += strdata + b'\x00' * (strings_padded - len(strdata))
    return out


def find_node(node, path):
    if not path:
        return node
    for c in node['children']:
        if c['name'] == path[0]:
            return find_node(c, path[1:])
    return None


def set_prop(node, name, data):
    for p in node['props']:
        if p['name'] == name:
            p['data'] = data
            return
    node['props'].append({'name': name, 'data': data})


def main():
    initrd_start = int(sys.argv[1], 0) if len(sys.argv) > 1 else 0
    initrd_end = int(sys.argv[2], 0) if len(sys.argv) > 2 else 0
    root = parse(TEMPLATE)

    cpu = find_node(root, ['cpus', 'cpu@0'])
    assert cpu is not None, "cpu@0 not found in template"
    set_prop(cpu, 'mmu-type', b'riscv,sv39\x00')
    # 64-bit hartid reg for cpu@0 (matches cpus #address-cells=2)
    set_prop(cpu, 'reg', struct.pack('>II', 0, 0))

    # Keep ALL CPU nodes in the DTB. The kernel only creates the early identity
    # map in swapper_pg_dir (required by relocate_enable_mmu re-relocation during
    # apply_boot_alternatives) when it sees >1 hart in the DTB. Actual bring-up is
    # capped to a single hart via the maxcpus=1 boot argument, so no sbi_hart_start
    # is issued but the identity map is preserved.

    # ---- riscv,timer node ----
    # The kernel (timer-riscv.c, CONFIG_RISCV_TIMER) registers the clockevent via
    # TIMER_OF_DECLARE("riscv,timer", ...). riscv_timer_init_dt() calls
    # riscv_of_processor_hartid(), which expects the timer node to be a CHILD of a
    # cpu node (so it can read the hartid from the parent cpu's reg). If the timer
    # node is a direct child of /cpus, the probe returns -ENODEV and no clockevent
    # is registered, so the scheduler can never arm the tick -> kernel spins in idle.
    # Attach it under cpu@0 (the only hart we actually bring up; maxcpus=1).
    base_cpu = find_node(root, ['cpus', 'cpu@0'])
    assert base_cpu is not None, "cpu@0 not found in template"
    # Find cpu@0's local interrupt-controller phandle so the timer node can point
    # its interrupt at the per-hart intc (S-mode timer interrupt == 5 in cpu-intc).
    base_intc_ph = 2  # QEMU default; overridden below if the intc child is found
    for _ic in base_cpu['children']:
        if _ic['name'] == 'interrupt-controller':
            for _p in _ic['props']:
                if _p['name'] == 'phandle':
                    base_intc_ph = struct.unpack('>I', _p['data'][:4])[0]
    if not any(c['name'] == 'timer' for c in base_cpu['children']):
        # reg=<0 0> is a 64-bit hartid: riscv_of_processor_hartid() reads it via
        # of_property_read_u64, which fails on the parent cpu's 32-bit `reg = <0>`
        # (cpus #address-cells is 1). Giving the timer node its own 64-bit reg makes
        # the hartid read succeed. interrupts-extended points at cpu@0's intc with
        # the S-mode timer interrupt id (5), so the clockevent IRQ maps and the
        # scheduler can arm the tick.
        base_cpu['children'].append({
            'name': 'timer',
            'props': [
                {'name': 'compatible', 'data': b'riscv,timer\x00'},
                {'name': 'reg', 'data': struct.pack('>II', 0, 0)},
                {'name': 'interrupts-extended',
                 'data': struct.pack('>II', base_intc_ph, 5)},
            ],
            'children': [],
        })

    # ---- riscv,isa must describe OUR cpu, not QEMU's ----
    # The template comes from qemu-system-riscv64, whose virt CPU advertises
    # zba/zbb/zbc/zbs/zicbom/zicboz/zawrs/zfa/svadu/h. The kernel BELIEVES the DTB:
    # it runtime-patches its ALTERNATIVE sites (e.g. the Zbb strcmp/strlen/strncmp
    # variants, which use orc.b) and then executes instructions our decoder does not
    # implement -> IllegalInstruction storm -> panic long before userspace.
    # So the ISA string is derived from what riscv-core actually executes.
    # KEEP IN SYNC WITH crates/riscv-core/src/decode.rs.
    #   sstc is real here: the emulator implements the stimecmp CSR (0x14D) and
    #   raises STIP when mtime >= stimecmp, so the kernel can drive its own tick
    #   without OpenSBI.
    OUR_ISA = 'rv64imafdc_zicntr_zicsr_zifencei_sstc'
    for _cpu in find_node(root, ['cpus'])['children']:
        if not _cpu['name'].startswith('cpu@'):
            continue
        for _pr in _cpu['props']:
            if _pr['name'] == 'riscv,isa':
                _pr['data'] = OUR_ISA.encode() + b'\x00'
        # Drop the new-style extension list as well: when present it takes
        # precedence over riscv,isa and would re-introduce what we just removed.
        # The cbo*-block-size props go too — they only mean anything with
        # Zicbom/Zicboz, which we do not implement.
        _cpu['props'] = [_pr for _pr in _cpu['props']
                         if _pr['name'] not in ('riscv,isa-extensions',
                                                'riscv,isa-base',
                                                'riscv,cbom-block-size',
                                                'riscv,cboz-block-size')]


    # --- Multi-hart clone so the kernel keeps the early identity map ---
    # The kernel only builds the early identity map (required by
    # relocate_enable_mmu re-relocation during apply_boot_alternatives) when it
    # sees >1 hart in the DTB. Our emulator is genuinely single-hart, so we
    # declare N harts in the DTB but cap actual bring-up with maxcpus=1 (see
    # DEFAULT_BOOTARGS); that keeps the identity map without ever trying to
    # start the non-existent secondaries.
    NHARTS = 8
    cpus = find_node(root, ['cpus'])
    set_prop(cpus, "timebase-frequency", struct.pack(">I", 10000000))
    # Force 64-bit hartid cells: kernel riscv_of_processor_hartid() reads the cpu
    # reg with of_property_read_u64, which fails when cpus #address-cells is 1
    # (cpu reg is then a single 32-bit cell) -> 'Invalid hartid for node'.
    set_prop(cpus, "#address-cells", struct.pack(">I", 2))
    assert cpus is not None, "cpus not found in template"
    base_cpu = find_node(root, ['cpus', 'cpu@0'])
    assert base_cpu is not None, "cpu@0 not found in template"
    base_ph = 1
    next_ph = 5  # 2=intc@0, 3=plic, 4=test already used
    cmap = find_node(root, ['cpus', 'cpu-map'])
    cluster0 = find_node(root, ['cpus', 'cpu-map', 'cluster0'])
    assert cluster0 is not None, "cpu-map/cluster0 not found"
    for i in range(1, NHARTS):
        new_ph = next_ph; next_ph += 1
        new_intc_ph = next_ph; next_ph += 1
        new_props = []
        for pr in base_cpu['props']:
            if pr['name'] == 'phandle':
                new_props.append({'name': 'phandle', 'data': struct.pack('>I', new_ph)})
            elif pr['name'] == 'reg':
                # cpus/#address-cells is 2, so `reg` is ONE 64-bit hartid split
                # across two big-endian cells: <hi lo>. Packing (i, 0) put the
                # hartid in the HIGH cell, giving cpu@i a hartid of i<<32.
                new_props.append({'name': 'reg', 'data': struct.pack('>II', 0, i)})
            elif pr['name'] == 'mmu-type':
                new_props.append({'name': 'mmu-type', 'data': b'riscv,sv39\x00'})
            else:
                new_props.append({'name': pr['name'], 'data': pr['data']})
        new_intc = {
            'name': 'interrupt-controller',
            'props': [
                {'name': '#interrupt-cells', 'data': struct.pack('>I', 1)},
                {'name': 'interrupt-controller', 'data': b''},
                {'name': 'compatible', 'data': b'riscv,cpu-intc\x00'},
                {'name': 'phandle', 'data': struct.pack('>I', new_intc_ph)},
            ],
            'children': [],
        }
        new_cpu = {'name': f'cpu@{i}', 'props': new_props, 'children': [new_intc]}
        cpus['children'].append(new_cpu)
        cluster0['children'].append({
            'name': f'core{i}',
            'props': [{'name': 'cpu', 'data': struct.pack('>I', new_ph)}],
            'children': [],
        })

    # ---- riscv,sbi node (SBI earlycon, matched by OF_EARLYCON_DECLARE "riscv,sbi") ----
    # The RISC-V SBI earlycon (drivers/tty/serial/earlycon-riscv-sbi.c) binds from
    # /chosen/stdout-path when the bare "earlycon" cmdline is present. Without this
    # node the earlycon never binds and the kernel emits nothing.
    if not any(c['name'] == 'riscv-sbi' for c in root['children']):
        root['children'].append({
            'name': 'riscv-sbi',
            'props': [{'name': 'compatible', 'data': b'riscv,sbi\x00'}],
            'children': [],
        })

    # ---- Add reg-io-width/reg-shift to UART so the 8250 OF driver can autoconfig ----
    # Without reg-io-width the OF serial driver defaults to 8-bit (UPIO_MEM) and the
    # 8250 autoconfig divisor-latch (DLAB) probe fails, so ttyS0 never registers.
    uart = find_node(root, ['soc', 'serial@10000000'])
    if uart is not None:
        set_prop(uart, 'reg-io-width', struct.pack('>I', 4))
        set_prop(uart, 'reg-shift', struct.pack('>I', 0))
    else:
        print("WARNING: serial@10000000 not found in template")

    chosen = find_node(root, ['chosen'])
    assert chosen is not None, "chosen not found in template"
    set_prop(chosen, 'bootargs', DEFAULT_BOOTARGS.encode() + b'\x00')
    set_prop(chosen, 'stdout-path', b'/riscv-sbi\x00')
    if initrd_start and initrd_end:
        set_prop(chosen, 'linux,initrd-start',
                 struct.pack('>II', (initrd_start >> 32) & 0xffffffff, initrd_start & 0xffffffff))
        set_prop(chosen, 'linux,initrd-end',
                 struct.pack('>II', (initrd_end >> 32) & 0xffffffff, initrd_end & 0xffffffff))

    out = serialize(root)
    with open('virt.dtb', 'wb') as f:
        f.write(out)
    print(f"Wrote {len(out)} bytes -> virt.dtb")


if __name__ == '__main__':
    main()
