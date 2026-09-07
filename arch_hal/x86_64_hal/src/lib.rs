//! Minimal x86-64 architecture boundary for the thin monitor.

#![cfg_attr(not(test), no_std)]

#[cfg(not(target_arch = "x86_64"))]
compile_error!("x86_64_hal requires an x86_64 target");

#[cfg(feature = "vmx")]
pub mod addr;
pub mod cpu;
#[cfg(feature = "vmx")]
pub mod ept;
#[cfg(feature = "vmx")]
pub mod paging;
#[cfg(feature = "vmx")]
pub mod platform_memory;
#[cfg(feature = "vmx")]
pub mod vmcs;
#[cfg(feature = "vmx")]
pub mod vmx;
