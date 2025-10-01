#![no_std]

#[cfg(target_arch = "aarch64")]
pub use aarch64_hal::*;
