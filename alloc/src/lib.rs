#![cfg_attr(not(any(test, doctest)), no_std)]
#![feature(generic_const_exprs)]
#![cfg_attr(not(test), feature(alloc_error_handler))]

extern crate alloc;
mod buddy_allocator;
mod intrusive_linked_list;
mod range_list_allocator;
use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::cell::OnceCell;
use core::cmp::max;
use core::ptr::null_mut;

use mutex::SpinLock;

use crate::buddy_allocator::BuddyAllocator;
use crate::range_list_allocator::MemoryBlock;
use crate::range_list_allocator::MemoryRegions;

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

#[macro_export]
macro_rules! levels {
    ($max:expr) => {
        ($max.trailing_zeros() as usize
            - $crate::intrusive_linked_list::MINIMUM_ALLOCATABLE_BYTES.trailing_zeros() as usize
            + 1)
    };
}

#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: MemoryAllocator<4096> = MemoryAllocator {
    range_list_allocator: SpinLock::new(OnceCell::new()),
    buddy_allocator: SpinLock::new(OnceCell::new()),
};

struct MemoryAllocator<const MAX_ALLOCATABLE_BYTES: usize>
where
    [(); levels!(MAX_ALLOCATABLE_BYTES)]:,
{
    range_list_allocator: SpinLock<OnceCell<MemoryBlock>>,
    buddy_allocator: SpinLock<OnceCell<BuddyAllocator<MAX_ALLOCATABLE_BYTES>>>,
}

unsafe impl<const MAX_ALLOCATABLE_BYTES: usize> Sync for MemoryAllocator<MAX_ALLOCATABLE_BYTES> where
    [(); levels!(MAX_ALLOCATABLE_BYTES)]:
{
}

#[cfg(not(test))]
fn static_alloc_for_buddy() -> Option<usize> {
    GLOBAL_ALLOCATOR.alloc_for_buddy_allocator()
}

impl<const MAX_ALLOCATABLE_BYTES: usize> MemoryAllocator<MAX_ALLOCATABLE_BYTES>
where
    [(); levels!(MAX_ALLOCATABLE_BYTES)]:,
{
    #[cfg(not(test))]
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
            let new_buddy_allocator =
                BuddyAllocator::<MAX_ALLOCATABLE_BYTES>::new(Some(&static_alloc_for_buddy));
            buddy_allocator_guard.set(new_buddy_allocator).unwrap();
        }
    }

    pub fn alloc_for_buddy_allocator(&self) -> Option<usize> {
        let mut range_list_allocator_guard = self.range_list_allocator.lock();
        if let Some(range_list_allocator) = range_list_allocator_guard.get_mut() {
            let layout =
                Layout::from_size_align(MAX_ALLOCATABLE_BYTES, MAX_ALLOCATABLE_BYTES).ok()?;
            range_list_allocator.allocate_region(layout)
        } else {
            None
        }
    }
}

unsafe impl<const MAX_ALLOCATABLE_BYTES: usize> GlobalAlloc
    for MemoryAllocator<MAX_ALLOCATABLE_BYTES>
where
    [(); levels!(MAX_ALLOCATABLE_BYTES)]:,
{
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        if max(layout.size(), layout.align()) > MAX_ALLOCATABLE_BYTES {
            let mut range_list_allocator_guard = self.range_list_allocator.lock();
            if let Some(range_list_allocator) = range_list_allocator_guard.get_mut()
                && let Some(heap_mem) = range_list_allocator.allocate_region(layout)
            {
                return heap_mem as *mut u8;
            }
        } else {
            let mut buddy_allocator_guard = self.buddy_allocator.lock();
            if let Some(buddy_allocator) = buddy_allocator_guard.get_mut()
                && let Ok(heap_mem) = buddy_allocator.alloc(layout)
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

#[cfg(not(test))]
#[alloc_error_handler]
fn panic(layout: Layout) -> ! {
    pr_debug!("allocator panicked!!: {:?}", layout);
    loop {}
}

// -----------------------
// Public API (non-test)
// -----------------------
#[cfg(not(test))]
/// Initialize the global allocator state. Safe to call multiple times.
pub fn init() {
    GLOBAL_ALLOCATOR.init();
}

#[cfg(not(test))]
/// Add an available memory region before finalization.
/// Returns Err if called after finalization.
pub fn add_available_region(address: usize, size: usize) -> Result<(), &'static str> {
    let mut guard = GLOBAL_ALLOCATOR.range_list_allocator.lock();
    if let Some(block) = guard.get_mut() {
        if block.is_finalized() {
            return Err("allocator already finalized");
        }
        block.add_region(&MemoryRegions::from_parts(address, size))
    } else {
        Err("allocator not initialized")
    }
}

#[cfg(not(test))]
/// Add a reserved memory region before finalization.
/// Returns Err if called after finalization.
pub fn add_reserved_region(address: usize, size: usize) -> Result<(), &'static str> {
    let mut guard = GLOBAL_ALLOCATOR.range_list_allocator.lock();
    if let Some(block) = guard.get_mut() {
        if block.is_finalized() {
            return Err("allocator already finalized");
        }
        block.add_reserved_region(&MemoryRegions::from_parts(address, size))
    } else {
        Err("allocator not initialized")
    }
}

#[cfg(not(test))]
pub fn allocate_dynamic_reserved_region(
    size: usize,
    align: Option<usize>,
    alloc_range: Option<(usize, usize)>,
) -> Result<Option<usize>, &'static str> {
    let mut guard = GLOBAL_ALLOCATOR.range_list_allocator.lock();
    if let Some(block) = guard.get_mut() {
        block.add_reserved_region_dynamic(size, align, alloc_range)
    } else {
        Err("allocator not initialized")
    }
}

#[cfg(not(test))]
/// Finalize the allocator by subtracting reserved regions and enabling allocation.
/// Safe to call multiple times; after the first success, it’s a no-op.
pub fn finalize() -> Result<(), &'static str> {
    let mut guard = GLOBAL_ALLOCATOR.range_list_allocator.lock();
    if let Some(block) = guard.get_mut() {
        if block.is_finalized() {
            return Ok(());
        }
        block.check_regions()
    } else {
        Err("allocator not initialized")
    }
}
