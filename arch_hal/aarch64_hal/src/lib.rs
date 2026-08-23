//! Top-level AArch64 HAL crate that re-exports architecture subsystems.

#![no_std]
#![cfg_attr(feature = "emergency-stack", feature(sync_unsafe_cell))]

#[cfg(feature = "emergency-stack")]
mod stack_overflow;

#[cfg(feature = "uefi-test")]
pub use aarch64_test::*;

pub use aarch64_gdb;
pub use aarch64_mutex;
pub use common;
pub use cpu;
pub use exceptions;
pub use gic;
#[cfg(feature = "paging")]
pub use paging;
pub use print::*;
pub use psci;
pub use soc;
#[cfg(feature = "emergency-stack")]
pub use stack_overflow::init_emergency_stack;
pub use timer;
pub use tls;
