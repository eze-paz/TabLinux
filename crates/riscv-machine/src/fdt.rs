//! Just enough flattened-devicetree handling to retarget the initrd.
//!
//! Where the initramfs lands depends on its size, so `linux,initrd-start` and
//! `linux,initrd-end` cannot be baked into a pre-built blob. Both are 8-byte
//! properties, so they can be overwritten in place — no relocation, no
//! re-serialisation, and nothing else about the tree needs understanding.

const MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Overwrite the initrd start/end properties. Returns false if the blob is not
/// a devicetree or the properties are missing, in which case the caller is
/// booting something that does not want an initrd anyway.
pub fn patch_initrd(dtb: &mut [u8], start: u64, end: u64) -> bool {
    if dtb.len() < 40 || be32(dtb, 0) != MAGIC {
        return false;
    }
    let off_struct = be32(dtb, 8) as usize;
    let off_strings = be32(dtb, 12) as usize;
    let size_strings = be32(dtb, 32) as usize;
    if off_struct >= dtb.len() || off_strings + size_strings > dtb.len() {
        return false;
    }

    // Property names live in a separate string block, referenced by offset.
    let name_at = |off: usize| -> &[u8] {
        let s = off_strings + off;
        if s >= dtb.len() {
            return &[];
        }
        let end = dtb[s..].iter().position(|&c| c == 0).map(|n| s + n).unwrap_or(dtb.len());
        &dtb[s..end]
    };

    let mut patched = 0;
    let mut pos = off_struct;
    // Collected first, then written, because `name_at` borrows the blob.
    let mut writes: [(usize, u64); 2] = [(0, 0); 2];

    while pos + 4 <= dtb.len() {
        match be32(dtb, pos) {
            FDT_BEGIN_NODE => {
                pos += 4;
                let e = dtb[pos..].iter().position(|&c| c == 0).map(|n| pos + n).unwrap_or(dtb.len());
                pos = align4(e + 1);
            }
            FDT_END_NODE | FDT_NOP => pos += 4,
            FDT_PROP => {
                let len = be32(dtb, pos + 4) as usize;
                let nameoff = be32(dtb, pos + 8) as usize;
                let data = pos + 12;
                let n = name_at(nameoff);
                // Both are 64-bit in the QEMU virt tree we clone. A 32-bit
                // variant exists in other trees; refusing to touch it is safer
                // than writing eight bytes over a four-byte property.
                if len == 8 {
                    if n == b"linux,initrd-start" {
                        writes[0] = (data, start);
                        patched += 1;
                    } else if n == b"linux,initrd-end" {
                        writes[1] = (data, end);
                        patched += 1;
                    }
                }
                pos = align4(data + len);
            }
            FDT_END => break,
            _ => return false, // unknown token: refuse rather than corrupt
        }
    }

    for (at, val) in writes {
        if at != 0 {
            dtb[at..at + 8].copy_from_slice(&val.to_be_bytes());
        }
    }
    patched == 2
}

/// Rewrite the `/memory` node's `reg` so the tree describes the RAM that
/// actually exists.
///
/// The devicetree ships prebuilt, with the size it was generated at baked in.
/// Booting a machine with less RAM than that and leaving this alone does not
/// fail loudly — Linux believes the tree, and the first thing it does with the
/// belief is place reservations near the top of what it thinks it has. With
/// 512 MiB of real RAM and a 1 GiB tree the boot stops dead at
///
///     cma: Reserved 16 MiB at 0x00000000bf000000
///
/// which is 1008 MiB up: past the end of memory, so the write goes nowhere and
/// the kernel wedges with no diagnostic. Nothing in that message says "your
/// devicetree is wrong".
///
/// Returns false and changes nothing if the tree is malformed or its `reg` is
/// not the 2-cell/2-cell shape the QEMU virt tree uses.
pub fn patch_memory(dtb: &mut [u8], base: u64, size: u64) -> bool {
    if dtb.len() < 40 || be32(dtb, 0) != MAGIC {
        return false;
    }
    let off_struct = be32(dtb, 8) as usize;
    let off_strings = be32(dtb, 12) as usize;
    let size_strings = be32(dtb, 32) as usize;
    if off_struct >= dtb.len() || off_strings + size_strings > dtb.len() {
        return false;
    }

    let name_at = |off: usize| -> &[u8] {
        let s = off_strings + off;
        if s >= dtb.len() {
            return &[];
        }
        let end = dtb[s..].iter().position(|&c| c == 0).map(|n| s + n).unwrap_or(dtb.len());
        &dtb[s..end]
    };

    // The node is named "memory@<addr>", so match on the prefix. Only `reg`
    // inside it is touched — every other node has a `reg` too.
    let mut in_memory = false;
    let mut depth = 0i32;
    let mut memory_depth = -1i32;
    let mut at = 0usize;
    let mut pos = off_struct;

    while pos + 4 <= dtb.len() {
        match be32(dtb, pos) {
            FDT_BEGIN_NODE => {
                pos += 4;
                let e = dtb[pos..].iter().position(|&c| c == 0).map(|n| pos + n).unwrap_or(dtb.len());
                let name = &dtb[pos..e];
                depth += 1;
                if !in_memory && (name == b"memory" || name.starts_with(b"memory@")) {
                    in_memory = true;
                    memory_depth = depth;
                }
                pos = align4(e + 1);
            }
            FDT_END_NODE => {
                if in_memory && depth == memory_depth {
                    in_memory = false;
                }
                depth -= 1;
                pos += 4;
            }
            FDT_NOP => pos += 4,
            FDT_PROP => {
                let len = be32(dtb, pos + 4) as usize;
                let nameoff = be32(dtb, pos + 8) as usize;
                let data = pos + 12;
                // 16 bytes = <#address-cells 2><#size-cells 2>, one bank. A
                // different shape means a tree this was not written for, and
                // writing 16 bytes into it would corrupt the blob.
                if in_memory && len == 16 && name_at(nameoff) == b"reg" {
                    at = data;
                }
                pos = align4(data + len);
            }
            FDT_END => break,
            _ => return false, // unknown token: refuse rather than corrupt
        }
    }

    if at == 0 {
        return false;
    }
    dtb[at..at + 8].copy_from_slice(&base.to_be_bytes());
    dtb[at + 8..at + 16].copy_from_slice(&size.to_be_bytes());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    /// Hand-build a minimal tree with the two properties we care about.
    fn tiny_dtb() -> Vec<u8> {
        let strings = b"linux,initrd-start\0linux,initrd-end\0";
        let mut st: Vec<u8> = Vec::new();
        st.extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
        st.extend_from_slice(&[0, 0, 0, 0]); // empty root name, padded
        for (nameoff, val) in [(0u32, 0u64), (19u32, 0u64)] {
            st.extend_from_slice(&FDT_PROP.to_be_bytes());
            st.extend_from_slice(&8u32.to_be_bytes());
            st.extend_from_slice(&nameoff.to_be_bytes());
            st.extend_from_slice(&val.to_be_bytes());
        }
        st.extend_from_slice(&FDT_END_NODE.to_be_bytes());
        st.extend_from_slice(&FDT_END.to_be_bytes());

        let off_struct = 40usize;
        let off_strings = off_struct + st.len();
        let mut d: Vec<u8> = Vec::new();
        d.extend_from_slice(&MAGIC.to_be_bytes());
        d.extend_from_slice(&((off_strings + strings.len()) as u32).to_be_bytes());
        d.extend_from_slice(&(off_struct as u32).to_be_bytes());
        d.extend_from_slice(&(off_strings as u32).to_be_bytes());
        d.extend_from_slice(&40u32.to_be_bytes()); // off_rsvmap
        d.extend_from_slice(&17u32.to_be_bytes()); // version
        d.extend_from_slice(&16u32.to_be_bytes()); // last_comp_version
        d.extend_from_slice(&0u32.to_be_bytes()); // boot_cpuid
        d.extend_from_slice(&(strings.len() as u32).to_be_bytes());
        d.extend_from_slice(&(st.len() as u32).to_be_bytes());
        d.extend_from_slice(&st);
        d.extend_from_slice(strings);
        d
    }

    /// Root containing `memory@80000000 { reg = <base size>; }` and a sibling
    /// node that also has a `reg`, so the test proves the right one is picked.
    fn dtb_with_memory() -> Vec<u8> {
        let strings = b"reg\0";
        let mut st: Vec<u8> = Vec::new();
        let node = |st: &mut Vec<u8>, name: &[u8], a: u64, b: u64| {
            st.extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
            st.extend_from_slice(name);
            st.push(0);
            while st.len() % 4 != 0 {
                st.push(0);
            }
            st.extend_from_slice(&FDT_PROP.to_be_bytes());
            st.extend_from_slice(&16u32.to_be_bytes());
            st.extend_from_slice(&0u32.to_be_bytes()); // "reg"
            st.extend_from_slice(&a.to_be_bytes());
            st.extend_from_slice(&b.to_be_bytes());
            st.extend_from_slice(&FDT_END_NODE.to_be_bytes());
        };

        st.extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
        st.extend_from_slice(&[0, 0, 0, 0]); // root
        node(&mut st, b"uart@10000000", 0x1000_0000, 0x100);
        node(&mut st, b"memory@80000000", 0x8000_0000, 1024 * 1024 * 1024);
        st.extend_from_slice(&FDT_END_NODE.to_be_bytes());
        st.extend_from_slice(&FDT_END.to_be_bytes());

        let off_struct = 40usize;
        let off_strings = off_struct + st.len();
        let mut d: Vec<u8> = Vec::new();
        d.extend_from_slice(&MAGIC.to_be_bytes());
        d.extend_from_slice(&((off_strings + strings.len()) as u32).to_be_bytes());
        d.extend_from_slice(&(off_struct as u32).to_be_bytes());
        d.extend_from_slice(&(off_strings as u32).to_be_bytes());
        d.extend_from_slice(&40u32.to_be_bytes());
        d.extend_from_slice(&17u32.to_be_bytes());
        d.extend_from_slice(&16u32.to_be_bytes());
        d.extend_from_slice(&0u32.to_be_bytes());
        d.extend_from_slice(&(strings.len() as u32).to_be_bytes());
        d.extend_from_slice(&(st.len() as u32).to_be_bytes());
        d.extend_from_slice(&st);
        d.extend_from_slice(strings);
        d
    }

    #[test]
    fn patch_memory_rewrites_the_memory_node_only() {
        let mut d = dtb_with_memory();
        let before = d.clone();
        assert!(patch_memory(&mut d, 0x8000_0000, 512 * 1024 * 1024));

        // The memory bank now describes 512 MiB...
        let at = d.windows(8)
            .position(|w| w == 0x8000_0000u64.to_be_bytes())
            .expect("memory base");
        assert_eq!(&d[at + 8..at + 16], &(512u64 * 1024 * 1024).to_be_bytes());

        // ...and the uart's reg is untouched. Getting this wrong would rewrite
        // whichever node happened to come first.
        let uart = before.windows(8)
            .position(|w| w == 0x1000_0000u64.to_be_bytes())
            .expect("uart base");
        assert_eq!(&d[uart..uart + 16], &before[uart..uart + 16]);
    }

    #[test]
    fn patch_memory_refuses_a_tree_with_no_memory_node() {
        // tiny_dtb has no memory node; it must be left exactly as it was
        // rather than having 16 bytes written somewhere hopeful.
        let mut d = tiny_dtb();
        let before = d.clone();
        assert!(!patch_memory(&mut d, 0x8000_0000, 1 << 20));
        assert_eq!(d, before);
    }

    #[test]
    fn rewrites_both_initrd_properties() {
        let mut d = tiny_dtb();
        assert!(patch_initrd(&mut d, 0xBE9E_0000, 0xBEFF_F58A));
        // The values sit right after each 12-byte property header.
        let s = 40 + 8 + 12;
        assert_eq!(&d[s..s + 8], &0xBE9E_0000u64.to_be_bytes());
        let e = s + 8 + 12;
        assert_eq!(&d[e..e + 8], &0xBEFF_F58Au64.to_be_bytes());
    }

    #[test]
    fn rejects_a_blob_that_is_not_a_devicetree() {
        let mut junk = [0xAAu8; 64];
        assert!(!patch_initrd(&mut junk, 1, 2));
    }
}
