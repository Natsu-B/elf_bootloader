//! Common architecture-neutral primitives shared across AArch64 HAL crates.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod addr;
pub mod irq;
pub mod mem;
pub mod mmio;
pub mod mmio_hook;

pub use addr::IpaAddr;
pub use addr::PhysAddr;
pub use addr::VirtAddr;
pub use irq::IrqSense;
pub use irq::PirqHookError;
pub use irq::PirqHookOp;
pub use irq::PirqLifecycleHook;
pub use irq::TriggerMode;
pub use mem::PAGE_SIZE_4K;
pub use mem::PAGE_SIZE_4K_U64;
pub use mem::align_down_u64;
pub use mem::align_down_usize;
pub use mem::align_up_u64;
pub use mem::align_up_usize;
pub use mem::is_aligned_u64;
pub use mem::is_aligned_usize;
pub use mmio::MmioRegion;
pub use mmio_hook::AccessClass;
pub use mmio_hook::MmioError;
pub use mmio_hook::MmioHandler;
pub use mmio_hook::SplitPolicy;
