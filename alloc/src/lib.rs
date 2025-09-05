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

use mutex::SpinLock;

use crate::buddy_allocator::BuddyAllocator;
use crate::range_list_allocator::MemoryBlock;

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
pub static ALLOCATOR: MemoryAllocator<'static, 4096> = MemoryAllocator {
    range_list_allocator: SpinLock::new(OnceCell::new()),
    buddy_allocator: SpinLock::new(OnceCell::new()),
};

struct MemoryAllocator<'a, const MAX_ALLOCATABLE_BYTES: usize>
where
    [(); levels!(MAX_ALLOCATABLE_BYTES)]:,
{
    range_list_allocator: SpinLock<OnceCell<MemoryBlock>>,
    buddy_allocator: SpinLock<OnceCell<BuddyAllocator<'a, MAX_ALLOCATABLE_BYTES>>>,
}

unsafe impl<const MAX_ALLOCATABLE_BYTES: usize> Sync for MemoryAllocator<'_, MAX_ALLOCATABLE_BYTES> where
    [(); levels!(MAX_ALLOCATABLE_BYTES)]:
{
}

impl<const MAX_ALLOCATABLE_BYTES: usize> MemoryAllocator<'_, MAX_ALLOCATABLE_BYTES> where
    [(); levels!(MAX_ALLOCATABLE_BYTES)]:
{
}

unsafe impl<const MAX_ALLOCATABLE_BYTES: usize> GlobalAlloc
    for MemoryAllocator<'_, MAX_ALLOCATABLE_BYTES>
where
    [(); levels!(MAX_ALLOCATABLE_BYTES)]:,
{
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        todo!()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        todo!()
    }

    unsafe fn realloc(
        &self,
        ptr: *mut u8,
        layout: core::alloc::Layout,
        new_size: usize,
    ) -> *mut u8 {
        // SAFETY: the caller must ensure that the `new_size` does not overflow.
        // `layout.align()` comes from a `Layout` and is thus guaranteed to be valid.
        let new_layout =
            unsafe { core::alloc::Layout::from_size_align_unchecked(new_size, layout.align()) };
        // SAFETY: the caller must ensure that `new_layout` is greater than zero.
        let new_ptr = unsafe { self.alloc(new_layout) };
        if !new_ptr.is_null() {
            // SAFETY: the previously allocated block cannot overlap the newly allocated block.
            // The safety contract for `dealloc` must be upheld by the caller.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    ptr,
                    new_ptr,
                    core::cmp::min(layout.size(), new_size),
                );
                self.dealloc(ptr, layout);
            }
        }
        new_ptr
    }
}

#[cfg(not(test))]
#[alloc_error_handler]
fn panic(layout: Layout) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
