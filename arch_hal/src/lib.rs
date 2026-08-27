//! Architecture-selected HAL re-exports.

#![no_std]

#[cfg(target_arch = "aarch64")]
pub use aarch64_hal::*;

#[cfg(target_arch = "x86_64")]
pub use x86_64_hal::*;
