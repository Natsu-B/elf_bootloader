//! AArch64 stage-1 and stage-2 paging types and translation helpers.

#![no_std]
#![recursion_limit = "1024"]
#![feature(generic_const_exprs)]
#![feature(sync_unsafe_cell)]

#[cfg(all(test, not(target_arch = "aarch64")))]
extern crate std;

extern crate alloc;

use core::alloc::Layout;
use core::slice;

mod registers;
pub mod stage1;
pub mod stage2;
pub use stage1::*;
pub use stage2::*;

const PAGE_TABLE_SIZE: usize = common::mem::PAGE_SIZE_4K;

fn new_table() -> Result<&'static mut [u64], PagingErr> {
    // SAFETY: the page size is a nonzero power of two and is also the requested alignment.
    let address = unsafe {
        alloc::alloc::alloc(Layout::from_size_align_unchecked(
            PAGE_TABLE_SIZE,
            PAGE_TABLE_SIZE,
        ))
    };
    if address.is_null() {
        return Err(PagingErr::OutOfMemory);
    }
    // SAFETY: the fresh allocation is exclusively owned and retained for the paging lifetime.
    let table = unsafe { slice::from_raw_parts_mut(address.cast(), 512) };
    table.fill(0);
    Ok(table)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagingErr {
    Corrupted,
    UnalignedPage,
    ZeroSizedPage,
    UnsupportedPARange,
    OutOfMemory,
    Stage2Fault,
}
