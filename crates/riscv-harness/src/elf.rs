use alloc::vec::Vec;

pub fn load_elf(_data: &[u8]) -> (u64, Vec<u8>) {
    (0x8000_0000, _data.to_vec())
}
