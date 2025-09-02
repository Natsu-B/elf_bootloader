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

static mut MEMORY_ALLOATOR: UnsafeCell<OnceCell<MemoryBlock>> = UnsafeCell::new(OnceCell::new());

struct MemoryBlock {
    regions: &'static mut [MemoryRegions],
    reserved_regions: &'static mut [MemoryRegions],
    region_size: u32,
    reserved_region_size: u32,
    region_capacity: u32,
    reserved_region_capacity: u32,
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

        // regionsは以下が保証されていなければならない
        // - regionsがaddressの昇順にソートされており、同じaddressのものはない
        // - メモリの範囲がかぶっているものはない(address~address+sizeの領域がかぶっているものはない)
        let search_result = valid_regions.binary_search_by_key(&region.address, |r| r.address);
        let search_result_unwrap = search_result.unwrap_or_else(|x| x);

        //  追加フロー図
        //  if 同じアドレスあり
        //      if サイズがもとより大きい
        //          if next_regionがかぶっていない
        //              変更して終了
        //          else next_regionとかぶってる
        //              next_regionと今のregionをくっつける
        //      else サイズがちっちゃい
        //          何もしないで終了
        //  else 同じアドレスなし
        //      switch かぶり
        //          case next_regionのみかぶっている
        //              next_regionと今のregionをくっつける
        //          case pre_regionのみかぶっている
        //              pre_regionと今のregionをくっつける
        //          case どっちもかぶっている
        //              next_regionとpre_regionと今のregionをくっつける
        //          default かぶっていない
        //              追加して終了
        let pre_region_opt = if search_result_unwrap != 0 {
            Some(valid_regions[search_result_unwrap - 1])
        } else {
            None
        };
        // Err(x) 用
        let next_region_opt = if search_result_unwrap + 1 < *size_ref as usize {
            Some(valid_regions[search_result_unwrap])
        } else {
            None
        };
        match search_result {
            Ok(x) => {
                assert_eq!(*region, valid_regions[x]);
                let same_address_region = &mut valid_regions[x];
                if same_address_region.size < region.size {
                    if let Some(next_region) = next_region_opt
                        && region.size + region.address >= next_region.address
                    {
                        valid_regions[x] = *region;
                        Self::merge_memory_region(valid_regions, x, size_ref);
                    } else {
                        same_address_region.size = region.size;
                    }
                }
                Ok(())
            }
            Err(x) => {
                let mut pre_region_overlaps = false;
                let mut next_region_overlaps = false;

                // Check for overlap with pre_region
                if let Some(pre_region) = pre_region_opt
                    && region.address <= pre_region.address + pre_region.size
                {
                    pre_region_overlaps = true;
                }

                // Check for overlap with next_region
                if let Some(next_region) = next_region_opt
                    && region.address + region.size >= next_region.address
                {
                    next_region_overlaps = true;
                }

                match (pre_region_overlaps, next_region_overlaps) {
                    (false, false) => {
                        // No overlap, just insert the new region
                        debug_assert!(
                            valid_regions[x - 1].address + valid_regions[x - 1].size
                                < region.address
                        );
                        debug_assert!(region.address + region.size < valid_regions[x].address);
                        valid_regions.copy_within(x..*size_ref as usize, x + 1);
                        valid_regions[x] = *region;
                        *size_ref += 1;
                    }
                    (true, false) => {
                        // Overlap with pre_region only
                        debug_assert!(valid_regions[x - 1].address < region.address);
                        debug_assert!(
                            valid_regions[x - 1].address + valid_regions[x - 1].size
                                > region.address
                        );
                        debug_assert!(region.address + region.size < valid_regions[x].address);
                        valid_regions[x - 1].size =
                            region.size - valid_regions[x - 1].address + region.size;
                    }
                    (false, true) => {
                        // Overlap with next_region only
                        debug_assert!(
                            valid_regions[x - 1].address + valid_regions[x - 1].size
                                < region.address
                        );
                        valid_regions[x].size += valid_regions[x].address - region.address;
                        valid_regions[x].address = region.address;
                    }
                    (true, true) => {
                        // Overlap with both pre_region and next_region
                        valid_regions[x - 1].size = valid_regions[x].address
                            - valid_regions[x - 1].address
                            + valid_regions[x].size;
                        valid_regions.copy_within(x + 1..*size_ref as usize, x);
                        *size_ref -= 1;
                    }
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct MemoryRegions {
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
    fn add_unused_block() {
        let allocator = MemoryBlock::init();
        
    }
}
