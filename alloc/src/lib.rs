#![cfg_attr(not(test), no_std)]

use core::cell::{OnceCell, UnsafeCell};

static mut GLOBAL_REGIONS: [MemoryRegions; 128] = [MemoryRegions {
    address: 0,
    size: 0,
}; 128];
static mut GLOBAL_RESERVED_REGIONS: [MemoryRegions; 128] = [MemoryRegions {
    address: 0,
    size: 0,
}; 128];

static mut MEMORY_ALLOCATOR: UnsafeCell<OnceCell<MemoryBlock>> = UnsafeCell::new(OnceCell::new());

struct MemoryBlock {
    regions: &'static mut [MemoryRegions],
    reserved_regions: &'static mut [MemoryRegions],
    region_size: u32,
    reserved_region_size: u32,
    region_capacity: u32,
    reserved_region_capacity: u32,
    allocatable: bool,
}

impl MemoryBlock {
    pub fn init() -> MemoryBlock {
        MemoryBlock {
            regions: unsafe { &mut *(&raw mut GLOBAL_REGIONS) },
            reserved_regions: unsafe { &mut *(&raw mut GLOBAL_RESERVED_REGIONS) },
            region_size: 0,
            reserved_region_size: 0,
            region_capacity: 128,
            reserved_region_capacity: 128,
            allocatable: false,
        }
    }

    pub fn add_region(&mut self, region: &MemoryRegions) -> Result<(), &str> {
        self.add_region_internal(false, region)
    }

    pub fn add_reserved_region(&mut self, region: &MemoryRegions) -> Result<(), &str> {
        self.add_region_internal(true, region)
    }

    fn merge_memory_region<'a>(
        memory_region: &'a mut [MemoryRegions],
        pre_index: usize,
        size_ref: &'a mut u32,
    ) -> &'a mut [MemoryRegions] {
        memory_region[pre_index] =
            MemoryRegions::merge_regions(&memory_region[pre_index], &memory_region[pre_index + 1]);
        memory_region.copy_within(pre_index + 2..*size_ref as usize, pre_index + 1);
        memory_region[*size_ref as usize - 1] = MemoryRegions {
            address: 0,
            size: 0,
        };
        memory_region
    }

    fn add_region_internal(
        &mut self,
        is_reserved: bool,
        region: &MemoryRegions,
    ) -> Result<(), &str> {
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
                let mut pre_region_overlaps = false;
                let mut next_region_overlaps = false;

                // Check for overlap with pre_region
                if x > 0 {
                    let pre_region = &regions_slice[x - 1];
                    if region.address <= pre_region.address + pre_region.size {
                        pre_region_overlaps = true;
                    }
                }

                // Check for overlap with next_region
                if x < *size_ref as usize {
                    let next_region = &regions_slice[x];
                    if region.address + region.size >= next_region.address {
                        next_region_overlaps = true;
                    }
                }

                match (pre_region_overlaps, next_region_overlaps) {
                    (false, false) => {
                        // No overlap, just insert the new region
                        regions_slice.copy_within(x..*size_ref as usize, x + 1);
                        regions_slice[x] = *region;
                        *size_ref += 1;
                    }
                    (true, false) => {
                        // Overlap with pre_region only
                        let pre_region = &mut regions_slice[x - 1];
                        let new_end = region.address + region.size;
                        let pre_region_end = pre_region.address + pre_region.size;

                        if new_end > pre_region_end {
                            pre_region.size = new_end - pre_region.address;
                        }
                    }
                    (false, true) => {
                        // Overlap with next_region only
                        let next_region = &mut regions_slice[x];
                        let old_end = next_region.address + next_region.size;
                        let new_end = region.address + region.size;

                        next_region.address = region.address;
                        next_region.size = old_end.max(new_end) - region.address;
                    }
                    (true, true) => {
                        // Overlap with both pre_region and next_region
                        let next_region = regions_slice[x];
                        let pre_region = &mut regions_slice[x - 1];

                        let new_end = (pre_region.address + pre_region.size)
                            .max(region.address + region.size)
                            .max(next_region.address + next_region.size);

                        pre_region.size = new_end - pre_region.address;

                        regions_slice.copy_within(x + 1..*size_ref as usize, x);
                        regions_slice[*size_ref as usize - 1] = MemoryRegions {
                            address: 0,
                            size: 0,
                        };
                        *size_ref -= 1;
                    }
                }
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
        // --- 1. Initial Validation ---
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

        for reserved_region in reserved_regions.iter() {
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
                    continue;
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

                    regions[region_idx].size = reserved_region.address - regions[region_idx].address;

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

        self.allocatable = true;
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct MemoryRegions {
    address: usize,
    size: usize,
}

impl MemoryRegions {
    fn merge_regions(first: &Self, second: &Self) -> Self {
        Self {
            address: first.address,
            size: second.size + second.address - first.address,
        }
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
}
