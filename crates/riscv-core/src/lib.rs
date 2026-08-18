#![no_std]
pub mod types;
pub mod decode;
pub mod execute;
pub mod fpu;
pub mod compressed;
pub mod encode;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod smoke_test;
pub mod state;
