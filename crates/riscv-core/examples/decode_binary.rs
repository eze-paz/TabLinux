use riscv_core::decode::decode;
use riscv_core::compressed::decompress;
use std::fs;

fn main() {
    let data = fs::read("/home/aezequiel/riscv-vm/kernels/text_section.raw").expect("Failed to read binary");
    let mut offset = 0usize;
    while offset < data.len() {
        if offset + 2 > data.len() {
            break;
        }
        let lo = u16::from_le_bytes([data[offset], data[offset+1]]);
        if (lo & 0x3) != 0x3 {
            // Compressed
            let instr = decompress(lo);
            let raw = lo as u32;
            match instr {
                Some(i) => println!("{:06x} {:04x} {:?}", offset, raw, i),
                None => println!("{:06x} {:04x} ILLEGAL", offset, raw),
            }
            offset += 2;
        } else {
            if offset + 4 > data.len() {
                break;
            }
            let hi = u16::from_le_bytes([data[offset+2], data[offset+3]]);
            let raw = ((hi as u32) << 16) | (lo as u32);
            let instr = decode(raw);
            println!("{:06x} {:08x} {:?}", offset, raw, instr);
            offset += 4;
        }
    }
}
