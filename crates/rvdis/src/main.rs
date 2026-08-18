//! Disassemble riscv64 out of an ELF using the emulator's OWN decoder.
//!
//! This box has no riscv64 binutils and no sudo to install any, but the decoder
//! the VM already trusts is right here, so decode with that. It also means a
//! disagreement between this listing and what the VM actually executed is
//! impossible by construction, which is the property you want when the thing
//! under debug is the VM.
//!
//!   rvdis <elf> <vaddr-hex> [count]
use riscv_core::{compressed, decode};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: rvdis <elf> <vaddr-hex> [count]");
        std::process::exit(2);
    }
    let d = std::fs::read(&a[1]).expect("read elf");
    let va = u64::from_str_radix(a[2].trim_start_matches("0x"), 16).expect("vaddr");
    let n: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(24);

    let g16 = |o: usize| u16::from_le_bytes(d[o..o + 2].try_into().unwrap()) as u64;
    let g32 = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap()) as u64;
    let g64 = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    let (phoff, phent, phnum) = (g64(32), g16(54), g16(56));
    let mut segs = vec![];
    for i in 0..phnum {
        let p = (phoff + i * phent) as usize;
        if g32(p) == 1 {
            segs.push((g64(p + 16), g64(p + 8), g64(p + 32))); // vaddr, off, filesz
        }
    }
    let foff = |v: u64| {
        segs.iter()
            .find(|(sv, _, sz)| *sv <= v && v < sv + sz)
            .map(|(sv, o, _)| o + (v - sv))
    };

    let mut v = va;
    for _ in 0..n {
        let Some(o) = foff(v) else {
            println!("{v:#x}: <not mapped>");
            break;
        };
        let half = g16(o as usize) as u16;
        if half & 3 != 3 {
            let txt = match compressed::decompress(half) {
                Some(i) => format!("{i:?}"),
                None => "<bad compressed>".into(),
            };
            println!("{:#x}:     {:04x}  {}", v, half, txt);
            v += 2;
        } else {
            let raw = g32(o as usize) as u32;
            println!("{:#x}: {:08x}  {:?}", v, raw, decode::decode(raw));
            v += 4;
        }
    }
}
