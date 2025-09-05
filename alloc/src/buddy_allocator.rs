use crate::levels;
use core::alloc::Layout;
use core::cmp::max;
use core::cmp::min;
use core::fmt;

use crate::intrusive_linked_list::IntrusiveLinkedList;
use crate::intrusive_linked_list::MINIMUM_ALLOCATABLE_BYTES;
use crate::pr_debug;

// Assumes that MAX_ALLOCATABLE_BYTES is a power of 2.
pub(crate) struct BuddyAllocator<const MAX_ALLOCATABLE_BYTES: usize>
where
    [(); levels!(MAX_ALLOCATABLE_BYTES)]: ,
{
    free_list: [IntrusiveLinkedList; levels!(MAX_ALLOCATABLE_BYTES)],
    // A function that can fetch memory of the size and alignment of MAX_ALLOCATABLE_BYTES.
    alloc_heap: Option<&'static (dyn Fn() -> Option<usize> + 'static)>,
    total_size: usize,
    allocated: usize,
}

impl<const MAX_ALLOCATABLE_BYTES: usize> fmt::Debug for BuddyAllocator<MAX_ALLOCATABLE_BYTES>
where
    [(); levels!(MAX_ALLOCATABLE_BYTES)]:,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuddyAllocator")
            .field("free_list", &self.free_list)
            .field("total_size", &self.total_size)
            .field("allocated", &self.allocated)
            .finish()
    }
}

impl<const MAX_ALLOCATABLE_BYTES: usize> BuddyAllocator<MAX_ALLOCATABLE_BYTES>
where
    [(); levels!(MAX_ALLOCATABLE_BYTES)]:,
{
    const MINIMUM_ALLOCATABLE_BYTES_LEVELS: usize =
        MINIMUM_ALLOCATABLE_BYTES.trailing_zeros() as usize;

    const LEVELS: usize = MAX_ALLOCATABLE_BYTES.trailing_zeros() as usize
        - Self::MINIMUM_ALLOCATABLE_BYTES_LEVELS
        + 1;

    fn level2size(level: usize) -> usize {
        1 << (level + Self::MINIMUM_ALLOCATABLE_BYTES_LEVELS)
    }

    fn size2level_next_power(size: usize) -> usize {
        Self::size2level(size.next_power_of_two())
    }

    // Assumes that size is a power of 2.
    fn size2level(size: usize) -> usize {
        size.trailing_zeros() as usize - Self::MINIMUM_ALLOCATABLE_BYTES_LEVELS
    }

    pub(crate) fn new(heap_allocator: Option<&'static (dyn Fn() -> Option<usize> + 'static)>) -> Self {
        pr_debug!("buddy_allocator: init");
        Self {
            free_list: core::array::from_fn(|_| IntrusiveLinkedList::new()),
            alloc_heap: heap_allocator,
            total_size: 0,
            allocated: 0,
        }
    }

    // Assumes that the size is a multiple of MAX_ALLOCATABLE_BYTES and the alignment is MAX_ALLOCATABLE_BYTES.
    pub(crate) fn set_memory(&mut self, ptr: usize, size: usize) {
        pr_debug!(
            "buddy_allocator: set memory: 0x{:x} size: 0x{:x}",
            ptr,
            size
        );
        self.total_size += size;
        let mut ptr = ptr;
        for _ in 0..size >> MAX_ALLOCATABLE_BYTES.trailing_zeros() {
            unsafe { self.free_list[Self::LEVELS - 1].push_back(ptr) };
            ptr += MAX_ALLOCATABLE_BYTES;
        }
    }

    pub(crate) fn change_allocator(
        &mut self,
        heap_allocator: Option<&'static (dyn Fn() -> Option<usize> + 'static)>,
    ) {
        self.alloc_heap = heap_allocator;
    }

    // Assumes that sizes and alignments greater than or equal to MAX_ALLOCATABLE_BYTES will not be requested.
    // According to the Layout specification, align must be greater than 0 and a power of 2.
    pub(crate) fn alloc(&mut self, layout: Layout) -> Result<usize, &'static str> {
        pr_debug!("buddy_allocator: alloc before: {:#?}", self);
        let required_size = max(layout.size(), layout.align()).max(MINIMUM_ALLOCATABLE_BYTES);
        let level = Self::size2level_next_power(required_size);

        let mut free_level = level;
        while free_level < Self::LEVELS && self.free_list[free_level].is_none() {
            free_level += 1;
        }

        if free_level == Self::LEVELS {
            if let Some(allocator) = self.alloc_heap.as_mut() {
                let new_heap = allocator().ok_or("failed to allocate memory from heap")?;
                self.set_memory(new_heap, MAX_ALLOCATABLE_BYTES);
                // After adding memory, we should have a block at the highest level
                free_level = Self::LEVELS - 1;
            } else {
                return Err("out of memory");
            }
        }

        for i in (level + 1..=free_level).rev() {
            let block = self.free_list[i].pop().unwrap();
            let lower_level_size = Self::level2size(i - 1);
            unsafe {
                self.free_list[i - 1].push(block + lower_level_size);
                self.free_list[i - 1].push(block);
            }
            pr_debug!("buddy_allocator: split: {:#?}", self);
        }

        let ptr = self.free_list[level].pop().unwrap();
        self.allocated += required_size.next_power_of_two();
        pr_debug!("buddy_allocator: alloc after: {:#?}", self);
        Ok(ptr)
    }

    // If a block of MAX_ALLOCATABLE_BYTES size is created, it is added to the end of the linked list.
    // Otherwise, it is merged using add_with_sort.
    pub(crate) fn dealloc(&mut self, ptr: usize, layout: Layout) {
        pr_debug!("buddy_allocator: dealloc before: {:#?}", self);
        pr_debug!(
            "buddy_allocator: dealloc ptr: 0x{:x}, size: 0x{:x}",
            ptr,
            layout.size()
        );
        let mut ptr = ptr;
        let required_size = layout.size().max(MINIMUM_ALLOCATABLE_BYTES);
        let mut level = Self::size2level_next_power(required_size);

        self.allocated -= required_size.next_power_of_two();

        while level + 1 < Self::LEVELS {
            let block_size = Self::level2size(level);
            let buddy_addr = if ptr.is_multiple_of(Self::level2size(level + 1)) {
                ptr + block_size
            } else {
                ptr - block_size
            };
            pr_debug!("buddy_allocator: dealloc buddy: 0x{:x}", buddy_addr);
            if self.free_list[level].remove_if(buddy_addr) {
                ptr = min(ptr, buddy_addr);
                level += 1;
            } else {
                unsafe { self.free_list[level].add_with_sort(ptr) };
                pr_debug!("buddy_allocator: dealloc after: {:#?}", self);
                return;
            }
        }
        unsafe { self.free_list[level].push_back(ptr) };
        pr_debug!("buddy_allocator: dealloc after: {:#?}", self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::alloc::Layout;
    use core::ptr;
    use std::cell::RefCell;

    const MAX_ALLOC: usize = 4096; // 4KB
    const HEAP_SIZE: usize = MAX_ALLOC * 4; // 16KB
    #[repr(align(4096))]
    struct AlignedHeap([u8; HEAP_SIZE]);

    #[test]
    fn test_new() {
        let allocator = BuddyAllocator::<MAX_ALLOC>::new(None);
        for i in 0..BuddyAllocator::<MAX_ALLOC>::LEVELS {
            assert!(allocator.free_list[i].is_none());
        }
        assert_eq!(allocator.total_size, 0);
        assert_eq!(allocator.allocated, 0);
    }

    #[test]
    fn test_set_memory_detailed() {
        const HEAP_SIZE: usize = MAX_ALLOC * 4; // 16KB
        #[repr(align(4096))]
        struct AlignedHeap([u8; HEAP_SIZE]);
        let mut heap = AlignedHeap([0; HEAP_SIZE]);
        let heap_addr = &mut heap.0 as *mut _ as usize;

        let mut allocator = BuddyAllocator::<MAX_ALLOC>::new(None);
        allocator.set_memory(heap_addr, HEAP_SIZE);
        let top_level = BuddyAllocator::<MAX_ALLOC>::LEVELS - 1;
        for i in 0..top_level {
            assert_eq!(allocator.free_list[i].size(), 0);
        }
        assert_eq!(allocator.free_list[top_level].size(), HEAP_SIZE / MAX_ALLOC);
        let mut current = allocator.free_list[top_level].next;
        for i in 0..(HEAP_SIZE / MAX_ALLOC) {
            let node = current.expect("Node should exist");
            assert_eq!(node.as_ptr() as usize, heap_addr + i * MAX_ALLOC);
            current = unsafe { node.as_ref().next };
        }
    }

    #[test]
    fn test_alloc_dealloc_simple() {
        const HEAP_SIZE: usize = MAX_ALLOC * 4;
        #[repr(align(4096))]
        struct AlignedHeap([u8; HEAP_SIZE]);
        let mut heap = AlignedHeap([0; HEAP_SIZE]);
        let heap_addr = &mut heap.0 as *mut _ as usize;

        let mut allocator = BuddyAllocator::<MAX_ALLOC>::new(None);
        allocator.set_memory(heap_addr, HEAP_SIZE);

        let layout = Layout::from_size_align(128, 8).unwrap();
        let ptr = allocator.alloc(layout).unwrap();

        assert!(ptr != 0);
        let level = BuddyAllocator::<MAX_ALLOC>::size2level_next_power(128);
        assert_eq!(
            allocator.allocated,
            BuddyAllocator::<MAX_ALLOC>::level2size(level)
        );

        allocator.dealloc(ptr, layout);
        assert_eq!(allocator.allocated, 0);
    }

    #[test]
    fn test_buddy_merge() {
        const SMALL_HEAP_SIZE: usize = 256;
        const MAX_SMALL_ALLOC: usize = 128;
        #[repr(align(128))]
        struct AlignedHeap([u8; SMALL_HEAP_SIZE]);
        let mut heap = AlignedHeap([0; SMALL_HEAP_SIZE]);
        let heap_addr = &mut heap.0 as *mut _ as usize;

        let mut allocator = BuddyAllocator::<MAX_SMALL_ALLOC>::new(None);
        allocator.set_memory(heap_addr, SMALL_HEAP_SIZE);

        let layout1 = Layout::from_size_align(32, 32).unwrap();
        let ptr1 = allocator.alloc(layout1).unwrap();

        let layout2 = Layout::from_size_align(32, 32).unwrap();
        let ptr2 = allocator.alloc(layout2).unwrap();

        let layout3 = Layout::from_size_align(64, 64).unwrap();
        let ptr3 = allocator.alloc(layout3).unwrap();

        allocator.dealloc(ptr1, layout1);
        allocator.dealloc(ptr2, layout2);
        allocator.dealloc(ptr3, layout3);

        assert_eq!(allocator.allocated, 0);
        let top_level = BuddyAllocator::<MAX_SMALL_ALLOC>::LEVELS - 1;
        let mut count = 0;
        let mut current = allocator.free_list[top_level].next;
        while let Some(node) = current {
            count += 1;
            current = unsafe { node.as_ref().next };
        }
        assert_eq!(count, 2);
    }

    #[test]
    fn test_buddy_merge_step_by_step() {
        const SMALL_HEAP_SIZE: usize = 256;
        const MAX_SMALL_ALLOC: usize = 128;
        #[repr(align(128))]
        struct AlignedHeap([u8; SMALL_HEAP_SIZE]);
        let mut heap = AlignedHeap([0; SMALL_HEAP_SIZE]);
        let heap_start = &mut heap.0 as *mut _ as usize;

        let mut allocator = BuddyAllocator::<MAX_SMALL_ALLOC>::new(None);
        allocator.set_memory(heap_start, SMALL_HEAP_SIZE);

        let top_level = BuddyAllocator::<MAX_SMALL_ALLOC>::LEVELS - 1;
        let level_64 = top_level - 1;
        let level_32 = top_level - 2;

        assert_eq!(allocator.free_list[top_level].size(), 2);

        let layout32 = Layout::from_size_align(32, 32).unwrap();
        let layout64 = Layout::from_size_align(64, 64).unwrap();

        let ptr1 = allocator.alloc(layout32).unwrap();
        let ptr2 = allocator.alloc(layout32).unwrap();
        let ptr3 = allocator.alloc(layout64).unwrap();

        assert_eq!(ptr1, heap_start);
        assert_eq!(ptr2, heap_start + 32);
        assert_eq!(ptr3, heap_start + 64);

        allocator.dealloc(ptr1, layout32);
        allocator.dealloc(ptr2, layout32);
        assert_eq!(
            allocator.free_list[level_64].size(),
            1,
            "After merging 2x32, a 64 block should exist"
        );

        allocator.dealloc(ptr3, layout64);

        assert_eq!(allocator.allocated, 0);
        assert_eq!(allocator.free_list[level_32].size(), 0);
        assert_eq!(
            allocator.free_list[level_64].size(),
            0,
            "After final merge, level 64 should be empty"
        );
        assert_eq!(allocator.free_list[top_level].size(), 2);
    }

    #[test]
    fn test_alloc_dealloc_split_and_merge() {
        #[repr(align(4096))]
        struct AlignedHeap([u8; MAX_ALLOC]);
        let mut heap = AlignedHeap([0; MAX_ALLOC]);
        let heap_addr = &mut heap.0 as *mut _ as usize;

        let mut allocator = BuddyAllocator::<MAX_ALLOC>::new(None);
        pr_debug!("heap address: 0x{:x}", heap_addr);
        allocator.set_memory(heap_addr, MAX_ALLOC);
        pr_debug!("tmp{:#?}", allocator);

        let top_level = BuddyAllocator::<MAX_ALLOC>::LEVELS - 1;
        let level_2048 = top_level - 1;
        let level_1024 = top_level - 2;
        let level_512 = top_level - 3;

        let layout = Layout::from_size_align(512, 8).unwrap();
        let ptr = allocator.alloc(layout).unwrap();

        assert_eq!(ptr, heap_addr);
        assert_eq!(allocator.allocated, 512);
        assert_eq!(allocator.free_list[top_level].size(), 0);
        assert_eq!(allocator.free_list[level_2048].size(), 1);
        assert_eq!(allocator.free_list[level_1024].size(), 1);
        assert_eq!(allocator.free_list[level_512].size(), 1);

        allocator.dealloc(ptr, layout);

        assert_eq!(allocator.allocated, 0);
        assert_eq!(allocator.free_list[level_512].size(), 0);
        assert_eq!(allocator.free_list[level_1024].size(), 0);
        assert_eq!(allocator.free_list[level_2048].size(), 0);
        assert_eq!(allocator.free_list[top_level].size(), 1);
        for i in 0..top_level {
            assert_eq!(allocator.free_list[i].size(), 0);
        }
    }
    #[test]
    fn test_out_of_memory() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // Test the case where the external allocator is called but returns None.
        static EXTERNAL_HEAP_CALLED: AtomicBool = AtomicBool::new(false);
        static HEAP_ALLOCATOR: fn() -> Option<usize> = || {
            EXTERNAL_HEAP_CALLED.store(true, Ordering::SeqCst);
            None // Simulate always returning None.
        };

        let mut allocator = BuddyAllocator::<MAX_ALLOC>::new(Some(&HEAP_ALLOCATOR));

        // Set up the heap.
        #[repr(align(4096))]
        struct AlignedHeap([u8; MAX_ALLOC]);
        let mut heap = AlignedHeap([0; MAX_ALLOC]);
        let heap_addr = &mut heap.0 as *mut _ as usize;
        allocator.set_memory(heap_addr, MAX_ALLOC);

        // The first allocation should succeed.
        let layout = Layout::from_size_align(MAX_ALLOC, 8).unwrap();
        let ptr = allocator.alloc(layout);
        assert!(ptr.is_ok());
        assert!(!EXTERNAL_HEAP_CALLED.load(Ordering::SeqCst)); // Not called at this point.

        // The second allocation will fail because there is no memory and the external allocator will return None.
        let layout2 = Layout::from_size_align(8, 8).unwrap();
        let ptr2 = allocator.alloc(layout2);
        assert!(ptr2.is_err());
        assert!(EXTERNAL_HEAP_CALLED.load(Ordering::SeqCst)); // Verify that the external allocator was called.
    }

    #[test]
    fn test_too_large_allocation() {
        const HEAP_SIZE: usize = MAX_ALLOC * 4;
        #[repr(align(4096))]
        struct AlignedHeap([u8; HEAP_SIZE]);
        let mut heap = AlignedHeap([0; HEAP_SIZE]);
        let heap_addr = &mut heap.0 as *mut _ as usize;

        let mut allocator = BuddyAllocator::<MAX_ALLOC>::new(None);
        allocator.set_memory(heap_addr, HEAP_SIZE);

        let layout = Layout::from_size_align(MAX_ALLOC + 1, 8).unwrap();
        let result = allocator.alloc(layout);
        assert!(result.is_err());
    }

    #[test]
    fn test_alloc_max_size() {
        const HEAP_SIZE: usize = MAX_ALLOC * 4;
        #[repr(align(4096))]
        struct AlignedHeap([u8; HEAP_SIZE]);
        let mut heap = AlignedHeap([0; HEAP_SIZE]);
        let heap_addr = &mut heap.0 as *mut _ as usize;

        let mut allocator = BuddyAllocator::<MAX_ALLOC>::new(None);
        allocator.set_memory(heap_addr, HEAP_SIZE);

        let layout = Layout::from_size_align(MAX_ALLOC, MAX_ALLOC).unwrap();
        let ptr = allocator.alloc(layout).unwrap();
        assert!(ptr != 0);
        assert_eq!(allocator.allocated, MAX_ALLOC);
        allocator.dealloc(ptr, layout);
        assert_eq!(allocator.allocated, 0);
    }

    #[test]
    fn test_alloc_zero_size() {
        const HEAP_SIZE: usize = MAX_ALLOC * 4;
        #[repr(align(4096))]
        struct AlignedHeap([u8; HEAP_SIZE]);
        let mut heap = AlignedHeap([0; HEAP_SIZE]);
        let heap_addr = &mut heap.0 as *mut _ as usize;

        let mut allocator = BuddyAllocator::<MAX_ALLOC>::new(None);
        allocator.set_memory(heap_addr, HEAP_SIZE);

        let layout = Layout::from_size_align(0, 1).unwrap();
        let ptr = allocator.alloc(layout).unwrap();
        assert!(ptr != 0);
        assert_eq!(allocator.allocated, MINIMUM_ALLOCATABLE_BYTES);
        allocator.dealloc(ptr, layout);
        assert_eq!(allocator.allocated, 0);
    }
}
