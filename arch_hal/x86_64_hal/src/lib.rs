//! Minimal x86-64 architecture boundary for the thin monitor.

#![cfg_attr(not(test), no_std)]

#[cfg(not(target_arch = "x86_64"))]
compile_error!("x86_64_hal requires an x86_64 target");

pub mod addr;
pub mod cpu;
pub mod ept;
pub mod vmcs;
pub mod vmx;
