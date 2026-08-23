//! Memory allocator for bare-metal environments.
//!
//! This crate provides a two-tier allocator: a buddy allocator for small allocations
//! and a range-list allocator for large/aligned allocations.

#![cfg_attr(not(any(test, doctest)), no_std)]
#![feature(generic_const_exprs)]
#![cfg_attr(not(test), feature(alloc_error_handler))]

extern crate alloc;
mod aligned_box;
mod buddy_allocator;
mod range_list_allocator;
pub use aligned_box::AlignedSliceBox;
use alloc::vec::Vec;
use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::cell::OnceCell;
use core::cmp::max;
use core::ptr::null_mut;

use mutex::RawSpinLock;

use crate::buddy_allocator::BuddyAllocator;
use crate::range_list_allocator::MemoryBlock;
use crate::range_list_allocator::MemoryRegions;

/// Debug print macro (no-op in release builds).
#[cfg(all(not(feature = "debug-assertions"), not(test)))]
#[macro_export]
macro_rules! pr_debug {
    ($($arg:tt)*) => {};
}

#[cfg(test)]
#[macro_export]
macro_rules! pr_debug {
    ($($arg:tt)*) => (std::println!("[info] (alloc) {} ({}:{})", format_args!($($arg)*), file!(), line!()));
}

#[cfg(all(feature = "debug-assertions", not(test)))]
#[macro_export]
macro_rules! pr_debug {
    ($($arg:tt)*) => {};
}

/// Minimum allocatable block size in bytes.
pub const MINIMUM_ALLOCATABLE_BYTES: usize = range_list_allocator::MINIMUM_ALLOCATABLE_BYTES;

/// Computes the number of buddy levels needed for the given maximum block size.
#[must_use]
pub const fn levels_value(max: usize) -> usize {
    max.trailing_zeros() as usize - MINIMUM_ALLOCATABLE_BYTES.trailing_zeros() as usize + 1
}

/// Macro to compute buddy levels at compile time.
#[macro_export]
macro_rules! levels {
    ($max:expr) => {
        $crate::levels_value($max as usize)
    };
}

/// Default allocator with 4096-byte maximum block size.
pub type DefaultAllocator = MemoryAllocator<4096, { levels!(4096) }>;

/// A two-tier memory allocator combining buddy and range-list algorithms.
pub struct MemoryAllocator<const MAX_ALLOCATABLE_BYTES: usize, const LEVELS: usize> {
    range_list_allocator: RawSpinLock<OnceCell<MemoryBlock>>,
    buddy_allocator: RawSpinLock<OnceCell<BuddyAllocator<MAX_ALLOCATABLE_BYTES, LEVELS>>>,
}

unsafe impl<const MAX_ALLOCATABLE_BYTES: usize, const LEVELS: usize> Sync
    for MemoryAllocator<MAX_ALLOCATABLE_BYTES, LEVELS>
{
}

impl<const MAX_ALLOCATABLE_BYTES: usize, const LEVELS: usize> Default
    for MemoryAllocator<MAX_ALLOCATABLE_BYTES, LEVELS>
{
    fn default() -> Self {
        Self::new()
    }
}

impl<const MAX_ALLOCATABLE_BYTES: usize, const LEVELS: usize>
    MemoryAllocator<MAX_ALLOCATABLE_BYTES, LEVELS>
{
    /// Creates a new uninitialized allocator.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            range_list_allocator: RawSpinLock::new(OnceCell::new()),
            buddy_allocator: RawSpinLock::new(OnceCell::new()),
        }
    }

    /// Initializes the allocator.
    ///
    /// # Panics
    /// Panics if initialization fails unexpectedly (should not happen with correct usage).
    pub fn init(&'static self) {
        // Initialize the range list allocator.
        let range_list_allocator_guard = self.range_list_allocator.lock();
        if range_list_allocator_guard.get().is_none() {
            let range_list_allocator = range_list_allocator::MemoryBlock::init();
            // NOTE: Memory regions should be added here before initializing the buddy allocator.
            range_list_allocator_guard
                .set(range_list_allocator)
                .unwrap();
        }

        // Initialize the buddy allocator.
        let buddy_allocator_guard = self.buddy_allocator.lock();
        if buddy_allocator_guard.get().is_none() {
            let new_buddy_allocator = BuddyAllocator::<MAX_ALLOCATABLE_BYTES, LEVELS>::new();
            buddy_allocator_guard.set(new_buddy_allocator).unwrap();
        }
    }

    /// Allocates a block for the buddy allocator from the range-list allocator.
    pub fn alloc_for_buddy_allocator(&self) -> Option<usize> {
        let mut range_list_allocator_guard = self.range_list_allocator.lock();
        let range_list_allocator = range_list_allocator_guard.get_mut()?;
        let layout = Layout::from_size_align(MAX_ALLOCATABLE_BYTES, MAX_ALLOCATABLE_BYTES).ok()?;
        range_list_allocator.allocate_region(layout.size(), layout.align())
    }
}

unsafe impl<const MAX_ALLOCATABLE_BYTES: usize, const LEVELS: usize> GlobalAlloc
    for MemoryAllocator<MAX_ALLOCATABLE_BYTES, LEVELS>
{
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        if max(layout.size(), layout.align()) > MAX_ALLOCATABLE_BYTES {
            let mut range_list_allocator_guard = self.range_list_allocator.lock();
            if let Some(range_list_allocator) = range_list_allocator_guard.get_mut()
                && let Some(heap_mem) =
                    range_list_allocator.allocate_region(layout.size(), layout.align())
            {
                return heap_mem as *mut u8;
            }
        } else {
            let mut buddy_allocator_guard = self.buddy_allocator.lock();
            let alloc_for_buddy = || self.alloc_for_buddy_allocator();
            if let Some(buddy_allocator) = buddy_allocator_guard.get_mut()
                && let Ok(heap_mem) = buddy_allocator.alloc(layout, Some(&alloc_for_buddy))
            {
                return heap_mem as *mut u8;
            }
        }
        null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        if max(layout.size(), layout.align()) > MAX_ALLOCATABLE_BYTES {
            let mut range_list_allocator_guard = self.range_list_allocator.lock();
            if let Some(range_list_allocator) = range_list_allocator_guard.get_mut() {
                range_list_allocator.deallocate_region(ptr as usize, layout);
            }
        } else {
            let mut buddy_allocator_guard = self.buddy_allocator.lock();
            if let Some(buddy_allocator) = buddy_allocator_guard.get_mut() {
                buddy_allocator.dealloc(ptr as usize, layout);
            }
        }
    }
}

impl<const MAX_ALLOCATABLE_BYTES: usize, const LEVELS: usize>
    MemoryAllocator<MAX_ALLOCATABLE_BYTES, LEVELS>
{
    /// Add an available memory region before finalization.
    ///
    /// # Errors
    /// Returns `Err` if called after finalization or if allocator is not initialized.
    pub fn add_available_region(&self, address: usize, size: usize) -> Result<(), &'static str> {
        let mut guard = self.range_list_allocator.lock();
        let block = guard.get_mut().ok_or("allocator not initialized")?;
        if block.is_finalized() {
            return Err("allocator already finalized");
        }
        block.add_region(&MemoryRegions::from_parts(address, size))
    }

    /// Add a reserved memory region before finalization.
    ///
    /// # Errors
    /// Returns `Err` if called after finalization or if allocator is not initialized.
    pub fn add_reserved_region(&self, address: usize, size: usize) -> Result<(), &'static str> {
        let mut guard = self.range_list_allocator.lock();
        let block = guard.get_mut().ok_or("allocator not initialized")?;
        if block.is_finalized() {
            return Err("allocator already finalized");
        }
        block.add_reserved_region(&MemoryRegions::from_parts(address, size))
    }

    /// Allocates a reserved region dynamically.
    ///
    /// # Errors
    /// Returns `Err` if the allocator is not initialized.
    pub fn allocate_dynamic_reserved_region(
        &self,
        size: usize,
        align: Option<usize>,
        alloc_range: Option<(usize, usize)>,
    ) -> Result<Option<usize>, &'static str> {
        let mut guard = self.range_list_allocator.lock();
        let block = guard.get_mut().ok_or("allocator not initialized")?;
        block.add_reserved_region_dynamic(size, align, alloc_range)
    }

    /// Finalize the allocator by subtracting reserved regions and enabling allocation.
    ///
    /// # Errors
    /// Returns `Err` if allocator is not initialized.
    /// Safe to call multiple times; after the first success, it’s a no-op.
    pub fn finalize(&self) -> Result<(), &'static str> {
        let mut guard = self.range_list_allocator.lock();
        let block = guard.get_mut().ok_or("allocator not initialized")?;
        if block.is_finalized() {
            return Ok(());
        }
        // Do not force-align free regions; just reconcile regions.
        block.check_regions()
    }

    /// Allocates memory with specified size and alignment.
    ///
    /// # Errors
    /// Returns `Err` if allocator is not initialized, not finalized, or allocation fails.
    pub fn allocate_with_size_and_align(
        &self,
        size: usize,
        align: usize,
    ) -> Result<usize, &'static str> {
        let mut guard = self.range_list_allocator.lock();
        let block = guard.get_mut().ok_or("allocator not initialized")?;
        if !block.is_finalized() {
            return Err("allocator not finalized");
        }

        block
            .allocate_region(size, align)
            .ok_or("failed to allocate")
    }

    /// Trims an already-finalized allocator before boot handoff.
    ///
    /// This function should be called right before transferring execution
    /// to another kernel or ELF payload (e.g., Linux), after `finalize()`
    /// has succeeded.
    ///
    /// # Safety
    /// - Do not call this after `enable_raw_atomics` has been invoked.
    /// - This function does not support atomic access and may cause a deadlock.
    /// - Internally calls `MemoryBlock::trim_for_boot`, which allocates a `Vec`
    ///   while the range-list allocator guard is held and can re-enter the
    ///   global allocator.
    /// - Only valid before `enable_raw_atomics()` when `RawSpinLock::lock()` is a
    ///   no-op and during single-threaded bring-up; after atomic locking is
    ///   enabled this path can deadlock or spin indefinitely.
    /// # Errors
    /// Returns `Err` if allocator is not initialized or not finalized.
    pub fn trim_for_boot(&self, reserve_bytes: usize) -> Result<Vec<(usize, usize)>, &'static str> {
        let mut guard = self.range_list_allocator.lock();
        let block = guard.get_mut().ok_or("allocator not initialized")?;
        if !block.is_finalized() {
            return Err("allocator not finalized");
        }
        block.trim_for_boot(reserve_bytes)
    }

    /// Iterates over each free memory region.
    ///
    /// # Errors
    /// Returns `Err` if allocator is not initialized.
    pub fn for_each_free_region<F: FnMut(usize, usize)>(&self, f: F) -> Result<(), &'static str> {
        let guard = self.range_list_allocator.lock();
        let block = guard.get().ok_or("allocator not initialized")?;
        block.for_each_free_region(f);
        Ok(())
    }

    /// Iterates over each reserved memory region.
    ///
    /// # Errors
    /// Returns `Err` if allocator is not initialized.
    pub fn for_each_reserved_region<F: FnMut(usize, usize)>(
        &self,
        f: F,
    ) -> Result<(), &'static str> {
        let guard = self.range_list_allocator.lock();
        let block = guard.get().ok_or("allocator not initialized")?;
        block.for_each_reserved_region(f);
        Ok(())
    }
}

#[cfg(all(not(test), not(feature = "no-alloc-error-handler")))]
#[alloc_error_handler]
fn panic(_layout: Layout) -> ! {
    pr_debug!("allocator panicked!!: {:?}", _layout);
    loop {}
}

/// Macro to define a global allocator with the specified page size.
#[macro_export]
macro_rules! define_global_allocator {
    ($name:ident, $page_size:expr) => {
        #[global_allocator]
        static $name: $crate::MemoryAllocator<{ $page_size }, { $crate::levels!($page_size) }> =
            $crate::MemoryAllocator::new();
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[global_allocator]
    static TEST_ALLOCATOR: std::alloc::System = std::alloc::System;

    static GLOBAL_ALLOCATOR: MemoryAllocator<4096, { levels!(4096) }> = MemoryAllocator::new();

    #[test]
    fn allocate_4k_page_after_finalize_with_unaligned_reserved_region() {
        use core::alloc::Layout;

        const PRIMARY_SIZE: usize = 0x80_000;
        const SECONDARY_SIZE: usize = 0x20_000;

        #[repr(align(4096))]
        struct PrimaryHeap([u8; PRIMARY_SIZE]);
        #[repr(align(4096))]
        struct SecondaryHeap([u8; SECONDARY_SIZE]);

        let mut primary = PrimaryHeap([0; PRIMARY_SIZE]);
        let mut secondary = SecondaryHeap([0; SECONDARY_SIZE]);

        let primary_base = primary.0.as_mut_ptr() as usize;
        let secondary_base = secondary.0.as_mut_ptr() as usize;

        GLOBAL_ALLOCATOR.init();
        GLOBAL_ALLOCATOR
            .add_available_region(primary_base, PRIMARY_SIZE)
            .unwrap();
        GLOBAL_ALLOCATOR
            .add_available_region(secondary_base, SECONDARY_SIZE)
            .unwrap();

        GLOBAL_ALLOCATOR
            .add_reserved_region(primary_base, 0x2_000)
            .unwrap();
        let dynamic_addr = GLOBAL_ALLOCATOR
            .allocate_dynamic_reserved_region(0x8_000, None, Some((primary_base, 0x20_000)))
            .unwrap()
            .expect("dynamic reserved region allocation failed");
        assert_eq!(
            dynamic_addr & 0xFFF,
            0,
            "dynamic reserved region is not 4KiB aligned"
        );
        GLOBAL_ALLOCATOR
            .add_reserved_region(primary_base + 0x3C_000, 0xA0)
            .unwrap();
        GLOBAL_ALLOCATOR
            .add_reserved_region(primary_base + 0x2_000, 0x12_000)
            .unwrap();
        GLOBAL_ALLOCATOR
            .add_reserved_region(primary_base + 0x20_000, 0x329)
            .unwrap();

        GLOBAL_ALLOCATOR.finalize().unwrap();

        let page = GLOBAL_ALLOCATOR
            .allocate_with_size_and_align(0x1_000, 0x1_000)
            .expect("allocate_with_size_and_align failed");
        assert_eq!(page & 0xFFF, 0, "range allocator returned unaligned page");

        let layout = Layout::from_size_align(0x1_000, 0x1_000).unwrap();
        let mut allocated = alloc::vec::Vec::new();
        for _ in 0..8 {
            let ptr = unsafe { GLOBAL_ALLOCATOR.alloc(layout) };
            assert!(
                !ptr.is_null(),
                "GlobalAlloc::alloc failed to hand out a 4KiB page"
            );
            assert_eq!(
                ptr as usize & 0xFFF,
                0,
                "GlobalAlloc handed out a non-4KiB-aligned page"
            );
            allocated.push(ptr);
        }

        for ptr in allocated {
            unsafe {
                GLOBAL_ALLOCATOR.dealloc(ptr, layout);
            }
        }
    }
}
