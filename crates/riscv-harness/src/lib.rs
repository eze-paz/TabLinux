#![cfg_attr(not(test), no_std)]
extern crate alloc;

pub mod elf;
pub mod boot;
pub mod pe_loader;

pub use elf::load_elf;
pub use boot::load_kernel;
pub use pe_loader::{prepare_pe_kernel, parse_kernel_header};
#[cfg(test)]
mod mini_boot;
