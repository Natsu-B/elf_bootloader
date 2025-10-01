#![no_std]

extern crate alloc;

use core::arch::asm;

mod allocator {
    use core::alloc::GlobalAlloc;
    use core::alloc::Layout;
    use core::ptr::null_mut;
    use core::sync::atomic::AtomicUsize;
    use core::sync::atomic::Ordering;

    const HEAP_SIZE: usize = 64 * 1024 * 1024; // 64 MiB scratch allocator space

    static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

    pub struct BumpAllocator {
        next: AtomicUsize,
    }

    impl BumpAllocator {
        pub const fn new() -> Self {
            Self {
                next: AtomicUsize::new(0),
            }
        }
    }

    unsafe impl GlobalAlloc for BumpAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let heap_start = core::ptr::addr_of_mut!(HEAP) as usize;
            let heap_end = heap_start + HEAP_SIZE;

            let mut current_offset = self.next.load(Ordering::Relaxed);

            loop {
                let current_addr = heap_start + current_offset;
                let aligned_addr = current_addr.next_multiple_of(layout.align());
                let aligned_offset = aligned_addr - heap_start;

                let allocation_end = match aligned_offset.checked_add(layout.size()) {
                    Some(end) => heap_start + end,
                    None => return null_mut(),
                };

                if allocation_end > heap_end {
                    return null_mut();
                }

                let new_offset = allocation_end - heap_start;

                match self.next.compare_exchange(
                    current_offset,
                    new_offset,
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return aligned_addr as *mut u8,
                    Err(offset) => current_offset = offset,
                }
            }
        }

        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
            // Bump allocator doesn't support deallocation
        }
    }

    unsafe impl Sync for BumpAllocator {}

    #[global_allocator]
    pub static ALLOCATOR: BumpAllocator = BumpAllocator::new();
}

pub fn exit_success() -> ! {
    exit_with_code(0)
}

pub fn exit_failure() -> ! {
    exit_with_code(1)
}

pub extern "C" fn exit_with_code(code: u32) -> ! {
    const SYS_EXIT_EXTENDED: u64 = 0x20; // semihosting op
    const ADP_APP_EXIT: u32 = 0x20026; // ADP_Stopped_ApplicationExit

    #[repr(C)]
    struct ExitArgs {
        reason: u32,
        value: u32,
    }

    let mut args = ExitArgs {
        reason: ADP_APP_EXIT,
        value: code,
    };
    let ptr = &mut args as *mut ExitArgs as usize;

    unsafe {
        asm!(
            "hlt #0xf000",                 // AArch64 semihosting trap
            in("x0") SYS_EXIT_EXTENDED,    // op
            in("x1") ptr,                  // &ExitArgs { reason, value }
            options(noreturn)
        );
    }
}