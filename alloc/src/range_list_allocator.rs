use core::alloc::Layout;
use core::fmt;
use core::ops::Deref;
use core::ops::DerefMut;
use core::ptr::copy_nonoverlapping;
use core::slice;

#[cfg(not(doctest))]
use alloc::boxed::Box;
#[cfg(doctest)]
use std::boxed::Box;

enum RegionData {
    Global([MemoryRegions; 128]),
    Heap(&'static mut [MemoryRegions]),
}

struct RegionContainer(RegionData);

impl Deref for RegionContainer {
    type Target = [MemoryRegions];

    fn deref(&self) -> &Self::Target {
        match &self.0 {
            RegionData::Global(slice) => slice,
            RegionData::Heap(heap) => heap,
        }
    }
}

impl DerefMut for RegionContainer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match &mut self.0 {
            RegionData::Global(slice) => slice,
            RegionData::Heap(heap) => heap,
        }
    }
}

pub(crate) struct MemoryBlock {
    regions: RegionContainer,
    reserved_regions: RegionContainer,
    region_size: u32,
    reserved_region_size: u32,
    region_capacity: u32,
    reserved_region_capacity: u32,
    allocatable: bool,
}

impl fmt::Debug for MemoryBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryBlock")
            .field("regions", &&self.regions[..self.region_size as usize])
            .field(
                "reserved_regions",
                &&self.reserved_regions[..self.reserved_region_size as usize],
            )
            .field("allocatable", &self.allocatable)
            .finish()
    }
}

impl MemoryBlock {
    pub fn init() -> MemoryBlock {
        MemoryBlock {
            regions: RegionContainer(RegionData::Global(
                [MemoryRegions {
                    address: 0,
                    size: 0,
                }; 128],
            )),
            reserved_regions: RegionContainer(RegionData::Global(
                [MemoryRegions {
                    address: 0,
                    size: 0,
                }; 128],
            )),
            region_size: 0,
            reserved_region_size: 0,
            region_capacity: 128,
            reserved_region_capacity: 128,
            allocatable: false,
        }
    }

    // Indicates whether allocation from regions is enabled (i.e., finalized).
    pub(crate) fn is_finalized(&self) -> bool {
        self.allocatable
    }

    pub fn add_region(&mut self, region: &MemoryRegions) -> Result<(), &'static str> {
        self.add_region_internal(false, region)
    }

    pub fn add_reserved_region(&mut self, region: &MemoryRegions) -> Result<(), &'static str> {
        self.add_region_internal(true, region)
    }

    fn insert_region(
        regions_slice: &mut [MemoryRegions],
        size_ref: &mut u32,
        insertion_point: usize,
        region_to_insert: &MemoryRegions,
    ) {
        #[cfg(debug_assertions)]
        {
            if insertion_point > 0 {
                let pre_region = &regions_slice[insertion_point - 1];
                assert!(pre_region.end() <= region_to_insert.address);
            }
            if insertion_point < *size_ref as usize {
                let next_region = &regions_slice[insertion_point];
                assert!(region_to_insert.end() <= next_region.address);
            }
        }

        regions_slice.copy_within(insertion_point..*size_ref as usize, insertion_point + 1);
        regions_slice[insertion_point] = *region_to_insert;
        *size_ref += 1;
    }

    fn add_and_merge_region(
        regions_slice: &mut [MemoryRegions],
        size_ref: &mut u32,
        x: usize, // insertion_point
        region: &MemoryRegions,
    ) {
        let mut pre_region_overlaps = false;
        let mut next_region_overlaps = false;

        // Check for overlap with pre_region
        if x > 0 {
            let pre_region = &regions_slice[x - 1];
            if region.address <= pre_region.end() {
                pre_region_overlaps = true;
            }
        }

        // Check for overlap with next_region
        if x < *size_ref as usize {
            let next_region = &regions_slice[x];
            if region.end() >= next_region.address {
                next_region_overlaps = true;
            }
        }

        match (pre_region_overlaps, next_region_overlaps) {
            (false, false) => {
                // No overlap, just insert the new region
                MemoryBlock::insert_region(regions_slice, size_ref, x, region);
            }
            (true, false) => {
                // Overlap with pre_region only
                let pre_region = &mut regions_slice[x - 1];
                let new_end = region.end();
                let pre_region_end = pre_region.end();

                if new_end > pre_region_end {
                    pre_region.size = new_end - pre_region.address;
                }
            }
            (false, true) => {
                // Overlap with next_region only
                let next_region = &mut regions_slice[x];
                let old_end = next_region.end();
                let new_end = region.end();

                next_region.address = region.address;
                next_region.size = old_end.max(new_end) - region.address;
            }
            (true, true) => {
                // Overlap with both pre_region and next_region
                let next_region = regions_slice[x];
                let pre_region = &mut regions_slice[x - 1];

                let new_end = pre_region.end().max(region.end()).max(next_region.end());

                pre_region.size = new_end - pre_region.address;

                regions_slice.copy_within(x + 1..*size_ref as usize, x);
                regions_slice[*size_ref as usize - 1] = MemoryRegions {
                    address: 0,
                    size: 0,
                };
                *size_ref -= 1;
            }
        }
    }

    fn add_region_internal(
        &mut self,
        is_reserved: bool,
        region: &MemoryRegions,
    ) -> Result<(), &'static str> {
        // Get mutable slices and references to sizes
        let (regions_slice, size_ref, capacity) = if is_reserved {
            (
                &mut self.reserved_regions[..], // Take a mutable slice
                &mut self.reserved_region_size,
                self.reserved_region_capacity,
            )
        } else {
            (
                &mut self.regions[..], // Take a mutable slice
                &mut self.region_size,
                self.region_capacity,
            )
        };

        if *size_ref + 1 > capacity {
            return Err("region size overflow");
        }

        let valid_regions = &mut regions_slice[0..*size_ref as usize];

        // The following must be guaranteed for `valid_regions`:
        // - It is sorted in ascending order of address.
        // - There are no overlapping memory ranges.
        let search_result = valid_regions.binary_search_by_key(&region.address, |r| r.address);

        // High-level logic for adding a region:
        // - If a region with the same address exists:
        //     - If the new region is larger, update the existing one.
        //         - If the updated region now overlaps with the *next* region, merge them.
        //     - If the new region is smaller or equal, do nothing.
        // - If no region with the same address exists (an `Err` from binary_search):
        //     - Find the insertion point.
        //     - Check for overlaps with the previous and next regions.
        //     - Based on the overlap, either:
        //         - Insert the new region without merging.
        //         - Merge with the previous region.
        //         - Merge with the next region.
        //         - Merge with both previous and next regions.
        match search_result {
            Ok(x) => {
                // Exact address match found.
                let new_size = region.size;
                let old_size = regions_slice[x].size;

                if new_size > old_size {
                    // The new region is larger. Check for merging with the next region.
                    let next_region_data = if x + 1 < *size_ref as usize {
                        Some(regions_slice[x + 1]) // Copy the data, not a borrow
                    } else {
                        None
                    };

                    if let Some(next_region) = next_region_data {
                        if region.address + new_size >= next_region.address {
                            // Overlaps with next, so merge.
                            let merged_end = (region.address + new_size)
                                .max(next_region.address + next_region.size);
                            regions_slice[x].size = merged_end - regions_slice[x].address;

                            // Remove the next_region
                            regions_slice.copy_within(x + 2..*size_ref as usize, x + 1);
                            regions_slice[*size_ref as usize - 1] = MemoryRegions {
                                address: 0,
                                size: 0,
                            };
                            *size_ref -= 1;
                        } else {
                            // No overlap, just update size.
                            regions_slice[x].size = new_size;
                        }
                    } else {
                        // No next region, just update size.
                        regions_slice[x].size = new_size;
                    }
                }
                // If new_size <= old_size, do nothing.
                Ok(())
            }
            Err(x) => {
                MemoryBlock::add_and_merge_region(regions_slice, size_ref, x, region);
                Ok(())
            }
        }
    }

    /// Subtracts the reserved memory regions from the available memory regions.
    ///
    /// This function operates under the following assumption:
    /// 1.  **Caller-Guaranteed Sort**: The `regions` and `reserved_regions` slices
    ///     are guaranteed by the caller to be pre-sorted by their base address.
    /// 2.  Each `reserved_region` must be fully contained within a single `region`.
    pub fn check_regions(&mut self) -> Result<(), &'static str> {
        const MAX_REGIONS: usize = 120; // A safe upper limit to allow for splits.
        if self.region_size as usize > MAX_REGIONS
            || self.reserved_region_size as usize > MAX_REGIONS
        {
            return Err("memory regions and reserved regions are too big");
        }
        if self.reserved_region_size == 0 {
            self.allocatable = true;
            return Ok(()); // Nothing to do if there are no reserved regions.
        }

        let regions = &mut self.regions;
        let reserved_regions = &self.reserved_regions[..self.reserved_region_size as usize];

        let mut region_idx: usize = 0;

        for reserved_region in reserved_regions {
            // Advance `region_idx` to find the region that could contain the reserved_region.
            while region_idx < self.region_size as usize
                && regions[region_idx].end() <= reserved_region.address
            {
                region_idx += 1;
            }

            if region_idx == self.region_size as usize {
                return Err("invalid reserved region: located outside of all available regions");
            }

            if reserved_region.address < regions[region_idx].address
                || reserved_region.end() > regions[region_idx].end()
            {
                return Err("the memory region is smaller than the reserved region");
            }

            let starts_at_same_address = regions[region_idx].address == reserved_region.address;
            let ends_at_same_address = regions[region_idx].end() == reserved_region.end();

            match (starts_at_same_address, ends_at_same_address) {
                // Case 1: The reserved region perfectly matches the available region.
                (true, true) => {
                    regions.copy_within((region_idx + 1)..(self.region_size as usize), region_idx);
                    self.region_size -= 1;
                }
                // Case 2: The reserved region is at the beginning of the available region.
                (true, false) => {
                    let subtracted_size = reserved_region.size;
                    regions[region_idx].address += subtracted_size;
                    regions[region_idx].size -= subtracted_size;
                }
                // Case 3: The reserved region is at the end of the available region.
                (false, true) => {
                    regions[region_idx].size -= reserved_region.size;
                }
                // Case 4: The reserved region is in the middle, splitting the available region.
                (false, false) => {
                    let new_region_count = self.region_size as usize + 1;
                    if new_region_count > self.region_capacity as usize {
                        return Err("region buffer overflow after splitting");
                    }

                    let original_region_end = regions[region_idx].end();

                    regions[region_idx].size =
                        reserved_region.address - regions[region_idx].address;

                    let new_region = MemoryRegions {
                        address: reserved_region.end(),
                        size: original_region_end - reserved_region.end(),
                    };

                    let insert_idx = region_idx + 1;
                    regions.copy_within(insert_idx..self.region_size as usize, insert_idx + 1);
                    regions[insert_idx] = new_region;

                    self.region_size += 1;
                    region_idx += 1;
                }
            }
        }

        // clean reserved memory region
        self.reserved_regions = RegionContainer(RegionData::Global(
            [MemoryRegions {
                address: 0,
                size: 0,
            }; 128],
        ));
        self.reserved_region_size = 0;

        self.allocatable = true;
        Ok(())
    }

    fn allocate_region_internal(&mut self, size: usize, alignment: usize) -> Option<usize> {
        let regions = &mut self.regions;
        for mut i in 0..self.region_size as usize {
            let address = regions[i].address;
            let address_multiple_of = address.next_multiple_of(alignment);
            let end_addr = regions[i].end();
            if address_multiple_of + size <= regions[i].end() {
                if !address.is_multiple_of(alignment) {
                    let size = address_multiple_of - address;
                    regions.copy_within(i..self.region_size as usize, i + 1);
                    regions[i] = MemoryRegions { address, size };
                    regions[i + 1].address = address_multiple_of;
                    regions[i + 1].size -= size;
                    i += 1;
                    self.region_size += 1;
                }
                let new_addr = address_multiple_of + size;
                if new_addr != regions[i].end() {
                    regions[i].address = new_addr;
                    regions[i].size = end_addr - new_addr;
                } else {
                    // The region is consumed completely.
                    regions.copy_within(i + 1..self.region_size as usize, i);
                    self.region_size -= 1;
                }
                self.add_reserved_alloc_record(address_multiple_of, size);
                return Some(address_multiple_of);
            }
        }
        None
    }

    fn ensure_overflow_headroom(&mut self) {
        if self.region_size + 10 > self.region_capacity
            || self.reserved_region_size + 10 > self.reserved_region_capacity
        {
            self.overflow_wrapping();
        }
    }

    // Record allocation into reserved list: reserved := reserved ∪ [addr, addr+size)
    fn add_reserved_alloc_record(&mut self, address: usize, size: usize) {
        let insertion_point = self.reserved_regions[0..self.reserved_region_size as usize]
            .binary_search_by_key(&address, |r| r.address)
            .unwrap_or_else(|x| x);
        MemoryBlock::add_and_merge_region(
            &mut self.reserved_regions,
            &mut self.reserved_region_size,
            insertion_point,
            &MemoryRegions { address, size },
        );
    }

    // Remove allocation range from reserved list: reserved := reserved \ [addr, addr+size)
    fn remove_reserved_alloc_record(&mut self, addr: usize, size: usize) {
        self.ensure_overflow_headroom();
        if size == 0 {
            return;
        }
        let reserved = &mut self.reserved_regions;
        let rsize = &mut self.reserved_region_size;
        let valid_reserved = &mut reserved[0..*rsize as usize];
        let search = valid_reserved.binary_search_by_key(&addr, |r| r.address);
        match search {
            Ok(i) => {
                let region = &mut reserved[i];
                if size == region.size {
                    reserved.copy_within(i + 1..*rsize as usize, i);
                    reserved[*rsize as usize - 1] = MemoryRegions {
                        address: 0,
                        size: 0,
                    };
                    *rsize -= 1;
                } else if size < region.size {
                    region.address += size;
                    region.size -= size;
                } else {
                    reserved.copy_within(i + 1..*rsize as usize, i);
                    reserved[*rsize as usize - 1] = MemoryRegions {
                        address: 0,
                        size: 0,
                    };
                    *rsize -= 1;
                }
            }
            Err(x) => {
                if x == 0 {
                    return;
                }
                let i = x - 1;
                let region = &mut reserved[i];
                let region_end = region.end();
                let dealloc_end = addr + size;
                if addr >= region.address && dealloc_end <= region_end {
                    let starts_at_same = addr == region.address;
                    let ends_at_same = dealloc_end == region_end;
                    match (starts_at_same, ends_at_same) {
                        (true, true) => {
                            reserved.copy_within(i + 1..*rsize as usize, i);
                            reserved[*rsize as usize - 1] = MemoryRegions {
                                address: 0,
                                size: 0,
                            };
                            *rsize -= 1;
                        }
                        (true, false) => {
                            region.address += size;
                            region.size -= size;
                        }
                        (false, true) => {
                            region.size = addr - region.address;
                        }
                        (false, false) => {
                            let original_end = region_end;
                            region.size = addr - region.address;
                            let insert_idx = i + 1;
                            reserved.copy_within(insert_idx..*rsize as usize, insert_idx + 1);
                            reserved[insert_idx] = MemoryRegions {
                                address: dealloc_end,
                                size: original_end - dealloc_end,
                            };
                            *rsize += 1;
                        }
                    }
                }
            }
        }
    }

    fn add_free_region_merge(&mut self, address: usize, size: usize) {
        let insertion_point = self.regions[0..self.region_size as usize]
            .binary_search_by_key(&address, |r| r.address)
            .unwrap_or_else(|x| x);
        MemoryBlock::add_and_merge_region(
            &mut self.regions,
            &mut self.region_size,
            insertion_point,
            &MemoryRegions { address, size },
        );
    }

    fn overflow_wrapping(&mut self) {
        let allocate_size = (self.region_capacity + self.reserved_region_capacity) as usize
            * 2
            * core::mem::size_of::<MemoryRegions>();
        let new_region = self
            .allocate_region_internal(allocate_size, 4096)
            .expect("out of memory");

        let new_regions_capacity = self.region_capacity * 2;
        let new_reserved_capacity = self.reserved_region_capacity * 2;

        let new_regions_ptr = new_region as *mut MemoryRegions;
        let new_reserved_ptr = (new_region
            + new_regions_capacity as usize * core::mem::size_of::<MemoryRegions>())
            as *mut MemoryRegions;

        unsafe {
            copy_nonoverlapping(
                self.regions.as_ptr(),
                new_regions_ptr,
                self.region_size as usize,
            );
            self.regions = RegionContainer(RegionData::Heap(slice::from_raw_parts_mut(
                new_regions_ptr,
                new_regions_capacity as usize,
            )));

            copy_nonoverlapping(
                self.reserved_regions.as_ptr(),
                new_reserved_ptr,
                self.reserved_region_size as usize,
            );
            self.reserved_regions = RegionContainer(RegionData::Heap(slice::from_raw_parts_mut(
                new_reserved_ptr,
                new_reserved_capacity as usize,
            )));
        }
        self.region_capacity = new_regions_capacity;
        self.reserved_region_capacity = new_reserved_capacity;
    }

    pub fn allocate_region(&mut self, layout: Layout) -> Option<usize> {
        if !self.allocatable {
            return None;
        }
        self.ensure_overflow_headroom();
        self.allocate_region_internal(layout.size(), layout.align())
    }

    pub fn deallocate_region(&mut self, ptr: usize, layout: Layout) {
        if !self.allocatable {
            return;
        }
        let addr = ptr;
        let size = layout.size();
        if size == 0 {
            return; // Nothing to do
        }
        // Ensure headroom for metadata operations
        self.ensure_overflow_headroom();

        // Remove from reserved list and return to free list
        self.remove_reserved_alloc_record(addr, size);
        self.add_free_region_merge(addr, size);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct MemoryRegions {
    address: usize,
    size: usize,
}

impl MemoryRegions {
    // Internal constructor for building regions from raw parts.
    pub(crate) fn from_parts(address: usize, size: usize) -> Self {
        Self { address, size }
    }

    fn end(&self) -> usize {
        self.address + self.size
    }
}

#[cfg(test)]
#[macro_export]
macro_rules! debug_assert {
    ($($arg:tt)*) => (
        assert!($($arg)*);
    )
}

#[cfg(not(test))]
#[macro_export]
macro_rules! debug_assert {
    ($($arg:tt)*) => {};
}

#[cfg(test)]
mod tests {
    use core::panic;
    use std::alloc::alloc;

    use crate::pr_debug;

    use super::*;

    #[test]
    fn add_single_region() {
        let mut allocator = MemoryBlock::init();
        let region = MemoryRegions {
            address: 0x1000,
            size: 0x100,
        };
        assert_eq!(allocator.add_region(&region), Ok(()));
        assert_eq!(allocator.region_size, 1);
        assert_eq!(allocator.regions[0], region);
    }

    #[test]
    fn add_two_separate_regions() {
        let mut allocator = MemoryBlock::init();
        let region1 = MemoryRegions {
            address: 0x1000,
            size: 0x100,
        };
        let region2 = MemoryRegions {
            address: 0x2000,
            size: 0x100,
        };
        assert_eq!(allocator.add_region(&region1), Ok(()));
        assert_eq!(allocator.add_region(&region2), Ok(()));
        assert_eq!(allocator.region_size, 2);
        assert_eq!(allocator.regions[0], region1);
        assert_eq!(allocator.regions[1], region2);
    }

    #[test]
    fn add_adjacent_regions_merge() {
        let mut allocator = MemoryBlock::init();
        let region1 = MemoryRegions {
            address: 0x1000,
            size: 0x100,
        };
        let region2 = MemoryRegions {
            address: 0x1100,
            size: 0x100,
        };
        assert_eq!(allocator.add_region(&region1), Ok(()));
        assert_eq!(allocator.add_region(&region2), Ok(()));

        assert_eq!(allocator.region_size, 1);
        let expected_region = MemoryRegions {
            address: 0x1000,
            size: 0x200,
        };
        assert_eq!(allocator.regions[0], expected_region);
    }

    #[test]
    fn add_overlapping_regions_merge() {
        let mut allocator = MemoryBlock::init();
        let region1 = MemoryRegions {
            address: 0x1000,
            size: 0x200,
        };
        let region2 = MemoryRegions {
            address: 0x1100,
            size: 0x200,
        };
        assert_eq!(allocator.add_region(&region1), Ok(()));
        assert_eq!(allocator.add_region(&region2), Ok(()));

        assert_eq!(allocator.region_size, 1);
        let expected_region = MemoryRegions {
            address: 0x1000,
            size: 0x300,
        };
        assert_eq!(allocator.regions[0], expected_region);
    }

    #[test]
    fn add_region_that_spans_two_existing_regions() {
        let mut allocator = MemoryBlock::init();
        let region1 = MemoryRegions {
            address: 0x1000,
            size: 0x100,
        };
        let region3 = MemoryRegions {
            address: 0x2000,
            size: 0x100,
        };
        assert_eq!(allocator.add_region(&region1), Ok(()));
        assert_eq!(allocator.add_region(&region3), Ok(()));
        assert_eq!(allocator.region_size, 2);

        // Add a region that connects region1 and region3
        let region2 = MemoryRegions {
            address: 0x1000,
            size: 0x1100,
        };
        assert_eq!(allocator.add_region(&region2), Ok(()));

        assert_eq!(allocator.region_size, 1);
        let expected_region = MemoryRegions {
            address: 0x1000,
            size: 0x1100,
        };
        assert_eq!(allocator.regions[0], expected_region);
    }

    #[test]
    fn add_reserved_region() {
        let mut allocator = MemoryBlock::init();
        let region = MemoryRegions {
            address: 0x1000,
            size: 0x100,
        };
        assert_eq!(allocator.add_reserved_region(&region), Ok(()));
        assert_eq!(allocator.reserved_region_size, 1);
        assert_eq!(allocator.reserved_regions[0], region);
    }

    #[test]
    fn add_region_that_is_contained_in_existing_region() {
        let mut allocator = MemoryBlock::init();
        let outer_region = MemoryRegions {
            address: 0x1000,
            size: 0x1000,
        };
        assert_eq!(allocator.add_region(&outer_region), Ok(()));
        assert_eq!(allocator.region_size, 1);

        let inner_region = MemoryRegions {
            address: 0x1100,
            size: 0x100,
        };
        assert_eq!(allocator.add_region(&inner_region), Ok(()));

        // The size should not change, and the region should remain the same
        assert_eq!(allocator.region_size, 1);
        assert_eq!(allocator.regions[0], outer_region);
    }

    #[test]
    fn check_regions_no_reserved() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        assert_eq!(allocator.check_regions(), Ok(()));
        assert_eq!(allocator.region_size, 1);
        assert!(allocator.allocatable);
    }

    #[test]
    fn check_regions_perfect_match() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();

        assert_eq!(allocator.check_regions(), Ok(()));
        assert_eq!(allocator.region_size, 0);
        assert!(allocator.allocatable);
    }

    #[test]
    fn check_regions_starts_at_same_address() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x1000,
                size: 0x100,
            })
            .unwrap();

        assert_eq!(allocator.check_regions(), Ok(()));
        assert_eq!(allocator.region_size, 1);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1100,
                size: 0xF00
            }
        );
        assert!(allocator.allocatable);
    }

    #[test]
    fn check_regions_ends_at_same_address() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x1F00,
                size: 0x100,
            })
            .unwrap();

        assert_eq!(allocator.check_regions(), Ok(()));
        assert_eq!(allocator.region_size, 1);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1000,
                size: 0xF00
            }
        );
        assert!(allocator.allocatable);
    }

    #[test]
    fn check_regions_split() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x1100,
                size: 0x100,
            })
            .unwrap();

        assert_eq!(allocator.check_regions(), Ok(()));
        assert_eq!(allocator.region_size, 2);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1000,
                size: 0x100
            }
        );
        assert_eq!(
            allocator.regions[1],
            MemoryRegions {
                address: 0x1200,
                size: 0xE00
            }
        );
        assert!(allocator.allocatable);
    }

    #[test]
    fn check_regions_multiple_reserved() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator
            .add_region(&MemoryRegions {
                address: 0x3000,
                size: 0x1000,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x1100,
                size: 0x100,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x3200,
                size: 0x100,
            })
            .unwrap();

        assert_eq!(allocator.check_regions(), Ok(()));
        assert_eq!(allocator.region_size, 4);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1000,
                size: 0x100
            }
        );
        assert_eq!(
            allocator.regions[1],
            MemoryRegions {
                address: 0x1200,
                size: 0xE00
            }
        );
        assert_eq!(
            allocator.regions[2],
            MemoryRegions {
                address: 0x3000,
                size: 0x200
            }
        );
        assert_eq!(
            allocator.regions[3],
            MemoryRegions {
                address: 0x3300,
                size: 0xD00
            }
        );
    }

    #[test]
    fn check_regions_error_outside() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x2000,
                size: 0x100,
            })
            .unwrap();
        assert_eq!(
            allocator.check_regions(),
            Err("invalid reserved region: located outside of all available regions")
        );
    }

    #[test]
    fn check_regions_error_not_contained() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x100,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x1000,
                size: 0x200,
            })
            .unwrap();
        assert_eq!(
            allocator.check_regions(),
            Err("the memory region is smaller than the reserved region")
        );
    }

    #[test]
    fn check_regions_multiple_reserved_in_one_region() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x1100,
                size: 0x100,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x1300,
                size: 0x100,
            })
            .unwrap();

        assert_eq!(allocator.check_regions(), Ok(()));
        assert_eq!(allocator.region_size, 3);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1000,
                size: 0x100
            }
        );
        assert_eq!(
            allocator.regions[1],
            MemoryRegions {
                address: 0x1200,
                size: 0x100
            }
        );
        assert_eq!(
            allocator.regions[2],
            MemoryRegions {
                address: 0x1400,
                size: 0xC00
            }
        );
    }

    #[test]
    fn test_allocate_region_before_check_regions() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        let layout = Layout::from_size_align(0x100, 0x10).unwrap();
        assert_eq!(allocator.allocate_region(layout), None);
    }

    #[test]
    fn test_allocate_region_simple() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator.check_regions().unwrap();

        let layout = Layout::from_size_align(0x100, 0x10).unwrap();
        let ptr = allocator.allocate_region(layout);
        assert_eq!(ptr, Some(0x1000));
        assert_eq!(allocator.region_size, 1);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1100,
                size: 0xF00
            }
        );
    }

    #[test]
    fn test_allocate_region_no_sufficient_space() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x100,
            })
            .unwrap();
        allocator.check_regions().unwrap();

        let layout = Layout::from_size_align(0x200, 0x10).unwrap();
        assert_eq!(allocator.allocate_region(layout), None);
    }

    #[test]
    fn test_allocate_region_respects_reserved_region() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x1000,
                size: 0x100,
            })
            .unwrap();
        allocator.check_regions().unwrap();

        let layout = Layout::from_size_align(0x100, 0x10).unwrap();
        let ptr = allocator.allocate_region(layout);
        assert_eq!(ptr, Some(0x1100));
        assert_eq!(allocator.region_size, 1);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1200,
                size: 0xE00
            }
        );
    }

    #[test]
    fn test_allocate_region_with_alignment() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1001,
                size: 0x1000,
            })
            .unwrap();
        allocator.check_regions().unwrap();

        let layout = Layout::from_size_align(0x100, 0x100).unwrap();
        let ptr = allocator.allocate_region(layout);
        assert_eq!(ptr, Some(0x1100));

        // Check that the original region is split correctly
        assert_eq!(allocator.region_size, 2);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1001,
                size: 0xFF
            }
        );
        assert_eq!(
            allocator.regions[1],
            MemoryRegions {
                address: 0x1200,
                size: 0xE01
            }
        );
    }

    #[test]
    fn test_overflow_wrapping() {
        let heap = unsafe { alloc(Layout::from_size_align_unchecked(0x200000, 0x1000)) };
        let mut allocator = MemoryBlock::init();
        // Add a large region with an unaligned address.
        allocator
            .add_region(&MemoryRegions {
                address: heap as usize,
                size: 0x200000, // 2MB
            })
            .unwrap();
        allocator.check_regions().unwrap();

        let initial_region_capacity = allocator.region_capacity;
        let initial_reserved_capacity = allocator.reserved_region_capacity;
        assert_eq!(initial_region_capacity, 128);

        // We need to increase region_size. Each allocation with a specific alignment
        // on an unaligned region will split it, increasing region_size by 1.
        // The overflow_wrapping is triggered when region_size + 10 > region_capacity.
        // So we need to reach region_size = 119 to trigger it on the next allocation.
        // Initial region_size is 1. We need 118 splits.
        for _ in 0..119 {
            let layout = Layout::from_size_align(0x10, 0x1000).unwrap();
            assert!(allocator.allocate_region(layout).is_some());
            // Each allocation creates a split, increasing region_size.
        }
        // After 118 allocations, region_size should be 119.
        assert_eq!(allocator.region_size, 119);
        assert_eq!(allocator.region_capacity, initial_region_capacity);
        // This allocation should trigger overflow_wrapping.
        let layout = Layout::from_size_align(0x10, 0x1000).unwrap();
        assert!(allocator.allocate_region(layout).is_some());
        // Verify that the capacities have been doubled.
        assert_eq!(allocator.region_capacity, initial_region_capacity * 2);
        assert_eq!(
            allocator.reserved_region_capacity,
            initial_reserved_capacity * 2
        );

        // region_size should be 120 now.
        assert_eq!(allocator.region_size, 120);

        // Verify that we can still allocate after wrapping.
        let layout = Layout::from_size_align(0x10, 0x1000).unwrap();
        assert!(allocator.allocate_region(layout).is_some());
        assert_eq!(allocator.region_size, 121);
    }

    #[test]
    fn test_deallocate_region_simple_roundtrip() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator.check_regions().unwrap();

        let layout = Layout::from_size_align(0x100, 0x10).unwrap();
        let ptr = allocator.allocate_region(layout).expect("alloc failed");
        assert_eq!(ptr, 0x1000);
        // After alloc: regions becomes [0x1100, 0xF00]
        assert_eq!(allocator.region_size, 1);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1100,
                size: 0xF00
            }
        );

        // Deallocate and expect full region restored
        allocator.deallocate_region(ptr, layout);
        assert_eq!(allocator.region_size, 1);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1000,
                size: 0x1000
            }
        );
    }

    #[test]
    fn test_deallocate_region_adjacent_allocations_merge_back() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator.check_regions().unwrap();

        let l = Layout::from_size_align(0x100, 0x100).unwrap();
        let p1 = allocator.allocate_region(l).unwrap(); // 0x1000..0x1100
        let p2 = allocator.allocate_region(l).unwrap(); // 0x1100..0x1200
        assert_eq!(p1, 0x1000);
        assert_eq!(p2, 0x1100);

        // Now free regions should begin at 0x1200..0x2000 (single region)
        assert_eq!(allocator.region_size, 1);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1200,
                size: 0xE00
            }
        );

        // Deallocate first block; should create a separate free region at 0x1000..0x1100
        allocator.deallocate_region(p1, l);
        assert_eq!(allocator.region_size, 2);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1000,
                size: 0x100
            }
        );
        assert_eq!(
            allocator.regions[1],
            MemoryRegions {
                address: 0x1200,
                size: 0xE00
            }
        );

        // Deallocate second block; free regions should merge into a single 0x1000..0x2000
        allocator.deallocate_region(p2, l);
        assert_eq!(allocator.region_size, 1);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1000,
                size: 0x1000
            }
        );
    }

    #[test]
    fn test_deallocate_region_middle_split_reserved() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator.check_regions().unwrap();

        // Allocate three adjacent blocks so reserved merges into one
        let l = Layout::from_size_align(0x100, 0x100).unwrap();
        let p1 = allocator.allocate_region(l).unwrap(); // 0x1000..0x1100
        let p2 = allocator.allocate_region(l).unwrap(); // 0x1100..0x1200
        let p3 = allocator.allocate_region(l).unwrap(); // 0x1200..0x1300
        assert_eq!((p1, p2, p3), (0x1000, 0x1100, 0x1200));

        // Free list: [0x1300, 0xD00]
        assert_eq!(allocator.region_size, 1);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1300,
                size: 0xD00
            }
        );

        // Dealloc middle block; reserved should split, free list gains 0x1100..0x1200 as a new region
        allocator.deallocate_region(p2, l);
        assert_eq!(allocator.region_size, 2);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1100,
                size: 0x100
            }
        );
        assert_eq!(
            allocator.regions[1],
            MemoryRegions {
                address: 0x1300,
                size: 0xD00
            }
        );

        // Dealloc ends; after both, the free list should merge fully back
        allocator.deallocate_region(p1, l);
        allocator.deallocate_region(p3, l);
        assert_eq!(allocator.region_size, 1);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1000,
                size: 0x1000
            }
        );
    }
}
