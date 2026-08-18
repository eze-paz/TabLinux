//! Integration-test harness for the RISC-V emulator.
//!
//! This crate exists so that heavy integration tests (e.g. full Alpine
//! Linux boot) live in a dedicated library crate and are auto-discovered
//! by `cargo test`.
