use alloc::vec::Vec;
use core::alloc::Layout;
use core::cmp::max;
use core::cmp::min;
use core::fmt;
use core::mem::size_of;
use core::ops::Deref;
use core::ops::DerefMut;
use core::ptr::copy_nonoverlapping;
use core::slice;
use intrusive_linked_list::IntrusiveLinkedList;

pub(crate) const MINIMUM_ALLOCATABLE_BYTES: usize = size_of::<IntrusiveLinkedList>();
const EMPTY_REGION: MemoryRegions = MemoryRegions {
    address: 0,
    size: 0,
};

fn checked_align_up(value: usize, alignment: usize) -> Option<usize> {
    if alignment == 0 {
        return None;
    }
    let rem = value % alignment;
    if rem == 0 {
        Some(value)
    } else {
        value.checked_add(alignment - rem)
    }
}

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
            regions: RegionContainer(RegionData::Global([EMPTY_REGION; 128])),
            reserved_regions: RegionContainer(RegionData::Global([EMPTY_REGION; 128])),
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
    ) -> Result<(), &'static str> {
        if insertion_point > 0 {
            let pre_region = &regions_slice[insertion_point - 1];
            let pre_end = pre_region.end_checked()?;
            if pre_end > region_to_insert.address {
                return Err("region overlap");
            }
        }
        let ins_end = region_to_insert.end_checked()?;
        if insertion_point < *size_ref as usize {
            let next_region = &regions_slice[insertion_point];
            if ins_end > next_region.address {
                return Err("region overlap");
            }
        }

        regions_slice.copy_within(insertion_point..*size_ref as usize, insertion_point + 1);
        regions_slice[insertion_point] = *region_to_insert;
        *size_ref += 1;
        Ok(())
    }

    fn add_and_merge_region(
        regions_slice: &mut [MemoryRegions],
        size_ref: &mut u32,
        x: usize, // insertion_point
        region: &MemoryRegions,
    ) -> Result<(), &'static str> {
        let pre_region_end = if x > 0 {
            Some(regions_slice[x - 1].end_checked()?)
        } else {
            None
        };
        let next_region_end = if x < *size_ref as usize {
            Some(regions_slice[x].end_checked()?)
        } else {
            None
        };
        let region_end = region.end_checked()?;

        // Adjacency also joins regions, keeping both ordered neighbors in one range.
        let pre_region_overlaps = pre_region_end.is_some_and(|pre_end| region.address <= pre_end);
        let next_region_overlaps = x < *size_ref as usize && region_end >= regions_slice[x].address;

        match (pre_region_overlaps, next_region_overlaps) {
            (false, false) => {
                // No overlap, just insert the new region
                MemoryBlock::insert_region(regions_slice, size_ref, x, region)?;
            }
            (true, false) => {
                // Overlap with pre_region only
                let pre_region = &mut regions_slice[x - 1];
                let pre_region_end = pre_region_end.ok_or("region end overflow")?;

                if region_end > pre_region_end {
                    pre_region.size = region_end
                        .checked_sub(pre_region.address)
                        .ok_or("region size underflow")?;
                }
            }
            (false, true) => {
                // Overlap with next_region only
                let next_region = &mut regions_slice[x];
                let old_end = next_region_end.ok_or("region end overflow")?;

                next_region.address = region.address;
                next_region.size = old_end
                    .max(region_end)
                    .checked_sub(region.address)
                    .ok_or("region end overflow")?;
            }
            (true, true) => {
                // Overlap with both pre_region and next_region
                let pre_region = &mut regions_slice[x - 1];
                let new_end = pre_region_end
                    .ok_or("region end overflow")?
                    .max(region_end)
                    .max(next_region_end.ok_or("region end overflow")?);

                pre_region.size = new_end
                    .checked_sub(pre_region.address)
                    .ok_or("region size underflow")?;

                regions_slice.copy_within(x + 1..*size_ref as usize, x);
                regions_slice[*size_ref as usize - 1] = EMPTY_REGION;
                *size_ref -= 1;
            }
        }
        Ok(())
    }

    fn add_region_internal(
        &mut self,
        is_reserved: bool,
        region: &MemoryRegions,
    ) -> Result<(), &'static str> {
        if region.size == 0 {
            return Ok(());
        }
        region.end_checked()?;
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
                        let new_end = region
                            .address
                            .checked_add(new_size)
                            .ok_or("region end overflow")?;
                        if new_end >= next_region.address {
                            // Overlaps with next, so merge.
                            let merged_end = new_end.max(
                                next_region
                                    .address
                                    .checked_add(next_region.size)
                                    .ok_or("region end overflow")?,
                            );
                            regions_slice[x].size = merged_end
                                .checked_sub(regions_slice[x].address)
                                .ok_or("region end overflow")?;

                            // Remove the next_region
                            regions_slice.copy_within(x + 2..*size_ref as usize, x + 1);
                            regions_slice[*size_ref as usize - 1] = EMPTY_REGION;
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
            Err(x) => MemoryBlock::add_and_merge_region(regions_slice, size_ref, x, region),
        }
    }

    pub fn add_reserved_region_dynamic(
        &mut self,
        size: usize,
        align: Option<usize>,
        alloc_range: Option<(usize, usize)>,
    ) -> Result<Option<usize>, &'static str> {
        // Enforce pre-finalize usage only
        if self.allocatable {
            return Err("allocator already finalized");
        }
        // Normalize inputs
        if size == 0 {
            return Ok(None);
        }
        let alignment = align.unwrap_or(1).max(1);

        // Iterate free regions to find a fitting spot respecting range and alignment.
        let regions_len = self.region_size as usize;
        'regions: for i in 0..regions_len {
            let mut reg_addr = self.regions[i].address;
            let mut reg_end = self.regions[i]
                .address
                .checked_add(self.regions[i].size)
                .ok_or("region end overflow")?;

            let region_start = loop {
                // Check if the region is suitable
                let region_start = if let Some((range_start, range_size)) = alloc_range {
                    let range_end = range_start
                        .checked_add(range_size)
                        .ok_or("alloc_range overflow")?;
                    if range_end < reg_addr {
                        return Ok(None);
                    }
                    if reg_end < range_start {
                        continue 'regions;
                    }
                    let region_start = match checked_align_up(max(reg_addr, range_start), alignment)
                    {
                        Some(addr) => addr,
                        None => continue 'regions,
                    };
                    let end = region_start
                        .checked_add(size)
                        .ok_or("requested size overflow")?;
                    if end > range_end {
                        continue 'regions;
                    }
                    region_start
                } else {
                    match checked_align_up(reg_addr, alignment) {
                        Some(addr) => addr,
                        None => continue 'regions,
                    }
                };
                let alloc_end = region_start
                    .checked_add(size)
                    .ok_or("requested size overflow")?;
                if alloc_end > reg_end {
                    continue 'regions;
                }

                // Check whether the region overlaps with reserved memory regions
                let slice = &self.reserved_regions[0..self.reserved_region_size as usize];
                let (prev_end, next_start) =
                    match slice.binary_search_by_key(&region_start, |x| x.address) {
                        Ok(i) => {
                            let prev_end = slice[i]
                                .address
                                .checked_add(slice[i].size)
                                .ok_or("region end overflow")?;
                            let next_start = if i + 1 < slice.len() {
                                slice[i + 1].address
                            } else {
                                usize::MAX
                            };
                            (prev_end, next_start)
                        }
                        Err(i) => {
                            let prev_end = if i > 0 {
                                slice[i - 1]
                                    .address
                                    .checked_add(slice[i - 1].size)
                                    .ok_or("region end overflow")?
                            } else {
                                0
                            };
                            let next_start = if i < slice.len() {
                                slice[i].address
                            } else {
                                usize::MAX
                            };
                            (prev_end, next_start)
                        }
                    };
                if prev_end > region_start || alloc_end > next_start {
                    reg_addr = max(reg_addr, prev_end);
                    reg_end = min(reg_end, next_start);
                    if reg_addr >= reg_end {
                        continue 'regions;
                    }
                    continue;
                }
                break region_start;
            };
            // Found a candidate. Commit it differently depending on finalized state.
            let new_region = MemoryRegions {
                address: region_start,
                size,
            };
            self.add_region_internal(true, &new_region)?;
            return Ok(Some(region_start));
        }

        Ok(None)
    }

    /// Subtracts the reserved memory regions from the available memory regions.
    ///
    /// This function operates under the following assumption:
    /// 1.  **Caller-Guaranteed Sort**: The `regions` and `reserved_regions` slices
    ///     are guaranteed by the caller to be pre-sorted by their base address.
    /// 2.  Reserved regions may partially overlap or span multiple regions; only the
    ///     overlapping portions are subtracted. Non-overlapping parts are ignored.
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

        let mut region_idx: usize = 0;

        for i in 0..self.reserved_region_size as usize {
            let reserved_region = self.reserved_regions[i];
            let mut cursor = reserved_region.address;
            let reserved_end = reserved_region.end_checked()?;
            let mut processed = false;

            while cursor < reserved_end {
                while region_idx < self.region_size as usize
                    && self.regions[region_idx].end_checked()? <= cursor
                {
                    region_idx += 1;
                }

                if region_idx == self.region_size as usize {
                    // Remaining reserved portion lies outside the known available regions.
                    cursor = reserved_end;
                    processed = true;
                    break;
                }

                if reserved_end <= self.regions[region_idx].address {
                    cursor = reserved_end;
                    processed = true;
                    break;
                }

                let clipped_start = max(cursor, self.regions[region_idx].address);
                let clipped_end = min(reserved_end, self.regions[region_idx].end_checked()?);

                if clipped_start >= clipped_end {
                    cursor = clipped_end;
                    continue;
                }

                processed = true;
                self.subtract_reserved_segment(&mut region_idx, clipped_start, clipped_end)?;
                cursor = clipped_end;
            }

            if cursor < reserved_end && !processed {
                return Err("invalid reserved region: located outside of all available regions");
            }
        }

        // clean reserved memory region
        self.reserved_regions = RegionContainer(RegionData::Global([EMPTY_REGION; 128]));
        self.reserved_region_size = 0;

        self.allocatable = true;
        Ok(())
    }

    fn subtract_reserved_segment(
        &mut self,
        region_idx: &mut usize,
        start: usize,
        end: usize,
    ) -> Result<(), &'static str> {
        debug_assert!(start < end);

        let idx = *region_idx;
        let regions = &mut self.regions;
        let region_address = regions[idx].address;
        let region_end = regions[idx].end_checked()?;

        debug_assert!(start >= region_address);
        debug_assert!(end <= region_end);

        let starts_at_same_address = region_address == start;
        let ends_at_same_address = region_end == end;
        let subtracted_size = end - start;

        match (starts_at_same_address, ends_at_same_address) {
            (true, true) => {
                regions.copy_within((idx + 1)..(self.region_size as usize), idx);
                self.region_size -= 1;
                let last_idx = self.region_size as usize;
                regions[last_idx] = EMPTY_REGION;
            }
            (true, false) => {
                regions[idx].address += subtracted_size;
                regions[idx].size -= subtracted_size;
            }
            (false, true) => {
                regions[idx].size -= subtracted_size;
            }
            (false, false) => {
                let new_region_count = self.region_size as usize + 1;
                if new_region_count > self.region_capacity as usize {
                    return Err("region buffer overflow after splitting");
                }

                let original_end = region_end;
                regions[idx].size = start - region_address;

                let insert_idx = idx + 1;
                regions.copy_within(insert_idx..self.region_size as usize, insert_idx + 1);
                regions[insert_idx] = MemoryRegions {
                    address: end,
                    size: original_end - end,
                };

                self.region_size += 1;
                *region_idx += 1;
            }
        }

        Ok(())
    }

    fn allocate_region_internal(&mut self, size: usize, alignment: usize) -> Option<usize> {
        if alignment == 0 {
            return None;
        }
        for mut i in 0..self.region_size as usize {
            let (address, region_size) = {
                let region = self.regions[i];
                (region.address, region.size)
            };
            let end_addr = address.checked_add(region_size)?;
            let address_multiple_of = checked_align_up(address, alignment)?;
            let alloc_end = address_multiple_of.checked_add(size)?;
            if alloc_end <= end_addr {
                if self
                    .add_reserved_alloc_record(address_multiple_of, size)
                    .is_err()
                {
                    return None;
                }
                if !address.is_multiple_of(alignment) {
                    let size = address_multiple_of - address;
                    self.regions
                        .copy_within(i..self.region_size as usize, i + 1);
                    self.regions[i] = MemoryRegions { address, size };
                    self.regions[i + 1].address = address_multiple_of;
                    self.regions[i + 1].size -= size;
                    i += 1;
                    self.region_size += 1;
                }
                let new_addr = alloc_end;
                if new_addr == end_addr {
                    // The region is consumed completely.
                    self.regions
                        .copy_within(i + 1..self.region_size as usize, i);
                    self.region_size -= 1;
                } else {
                    self.regions[i].address = new_addr;
                    self.regions[i].size = end_addr - new_addr;
                }
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
    fn add_reserved_alloc_record(
        &mut self,
        address: usize,
        size: usize,
    ) -> Result<(), &'static str> {
        let insertion_point = self.reserved_regions[0..self.reserved_region_size as usize]
            .binary_search_by_key(&address, |r| r.address)
            .unwrap_or_else(|x| x);
        MemoryBlock::add_and_merge_region(
            &mut self.reserved_regions,
            &mut self.reserved_region_size,
            insertion_point,
            &MemoryRegions { address, size },
        )
    }

    // Remove allocation range from reserved list: reserved := reserved \ [addr, addr+size)
    fn remove_reserved_alloc_record(
        &mut self,
        addr: usize,
        size: usize,
    ) -> Result<(), &'static str> {
        if size == 0 {
            return Ok(());
        }
        let reserved = &mut self.reserved_regions;
        let rsize = &mut self.reserved_region_size;
        let valid_reserved = &mut reserved[0..*rsize as usize];
        let search = valid_reserved.binary_search_by_key(&addr, |r| r.address);
        match search {
            Ok(i) => {
                let region = &mut reserved[i];
                match size.cmp(&region.size) {
                    core::cmp::Ordering::Less => {
                        region.address = region
                            .address
                            .checked_add(size)
                            .ok_or("region end overflow")?;
                        region.size = region.size.checked_sub(size).ok_or("region end overflow")?;
                    }
                    core::cmp::Ordering::Equal | core::cmp::Ordering::Greater => {
                        reserved.copy_within(i + 1..*rsize as usize, i);
                        reserved[*rsize as usize - 1] = EMPTY_REGION;
                        *rsize -= 1;
                    }
                }
            }
            Err(x) => {
                if x == 0 {
                    return Ok(());
                }
                let i = x - 1;
                let region = &mut reserved[i];
                let region_end = region.end_checked()?;
                let dealloc_end = addr.checked_add(size).ok_or("dealloc end overflow")?;
                if addr >= region.address && dealloc_end <= region_end {
                    let starts_at_same = addr == region.address;
                    let ends_at_same = dealloc_end == region_end;
                    match (starts_at_same, ends_at_same) {
                        (true, true) => {
                            reserved.copy_within(i + 1..*rsize as usize, i);
                            reserved[*rsize as usize - 1] = EMPTY_REGION;
                            *rsize -= 1;
                        }
                        (true, false) => {
                            region.address = region
                                .address
                                .checked_add(size)
                                .ok_or("region end overflow")?;
                            region.size =
                                region.size.checked_sub(size).ok_or("region end overflow")?;
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
        Ok(())
    }

    fn add_free_region_merge(&mut self, address: usize, size: usize) -> Result<(), &'static str> {
        let insertion_point = self.regions[0..self.region_size as usize]
            .binary_search_by_key(&address, |r| r.address)
            .unwrap_or_else(|x| x);
        MemoryBlock::add_and_merge_region(
            &mut self.regions,
            &mut self.region_size,
            insertion_point,
            &MemoryRegions { address, size },
        )
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

    pub fn allocate_region(&mut self, size: usize, align: usize) -> Option<usize> {
        if !self.allocatable {
            return None;
        }
        self.ensure_overflow_headroom();
        self.allocate_region_internal(size, align)
    }

    pub fn try_deallocate_region(
        &mut self,
        ptr: usize,
        layout: Layout,
    ) -> Result<(), &'static str> {
        if !self.allocatable {
            return Ok(());
        }
        let addr = ptr;
        let size = layout.size();
        if size == 0 {
            return Ok(()); // Nothing to do
        }
        // Ensure headroom for metadata operations
        self.ensure_overflow_headroom();

        // Remove from reserved list and return to free list
        self.remove_reserved_alloc_record(addr, size)?;
        self.add_free_region_merge(addr, size)?;
        Ok(())
    }

    pub fn deallocate_region(&mut self, ptr: usize, layout: Layout) {
        let _ = self.try_deallocate_region(ptr, layout);
    }

    /// Trims the allocator metadata and returns reserved regions for boot handoff.
    ///
    /// # Safety
    /// Allocates a `Vec` while `self` is exclusively borrowed under the allocator
    /// lock; callers must ensure this path only runs before `enable_raw_atomics`
    /// (when `RawSpinLock::lock()` is a no-op) and while execution is
    /// single-threaded. After atomic locking is enabled, re-entering the global
    /// allocator here may deadlock or spin forever.
    pub fn trim_for_boot(
        &mut self,
        reserve_bytes: usize,
    ) -> Result<Vec<(usize, usize)>, &'static str> {
        if !self.allocatable {
            return Err("allocator not allocatable");
        }

        self.ensure_overflow_headroom();
        let allocate = self
            .allocate_region_internal(reserve_bytes, 1)
            .ok_or("allocation failed")?;

        // Allocates while holding the guard; only valid in the pre-atomic/no-op lock phase.
        let mut vec = Vec::with_capacity(self.reserved_region_size as usize);

        self.allocatable = false;

        for region in self
            .reserved_regions
            .iter()
            .take(self.reserved_region_size as usize)
        {
            vec.push((region.address, region.size));
        }

        // clean memory region (free list)
        self.regions.fill(EMPTY_REGION);
        self.region_capacity = self.regions.len() as u32; // 128 for inline storage.
        self.region_size = 0;

        self.allocatable = true;

        self.remove_reserved_alloc_record(allocate, reserve_bytes)?;
        self.add_free_region_merge(allocate, reserve_bytes)?;
        Ok(vec)
    }

    pub fn for_each_free_region<F: FnMut(usize, usize)>(&self, mut f: F) {
        for region in &self.regions[..self.region_size as usize] {
            f(region.address, region.size);
        }
    }

    pub fn for_each_reserved_region<F: FnMut(usize, usize)>(&self, mut f: F) {
        for region in &self.reserved_regions[..self.reserved_region_size as usize] {
            f(region.address, region.size);
        }
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

    fn end_checked(&self) -> Result<usize, &'static str> {
        self.address
            .checked_add(self.size)
            .ok_or("region end overflow")
    }
}

/// Debug assertion macro (evaluates in test builds only).
#[cfg(test)]
#[macro_export]
macro_rules! debug_assert {
    ($($arg:tt)*) => (
        assert!($($arg)*);
    )
}

/// Debug assertion macro (no-op in release builds).
#[cfg(not(test))]
#[macro_export]
macro_rules! debug_assert {
    ($($arg:tt)*) => {};
}

#[cfg(test)]
mod tests {

    use std::alloc::alloc;

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
    fn check_regions_does_not_align_without_reserved_regions() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1234,
                size: 0x5000,
            })
            .unwrap();

        allocator.check_regions().expect("reconcile pass failed");

        assert_eq!(allocator.region_size, 1);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x1234,
                size: 0x5000
            }
        );
    }

    #[test]
    fn allocations_are_page_aligned() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x0,
                size: 0x3FC0_0000,
            })
            .unwrap();
        allocator
            .add_region(&MemoryRegions {
                address: 0x4000_0000,
                size: 0xC000_0000,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x0,
                size: 0x80_000,
            })
            .unwrap();
        allocator
            .add_reserved_region_dynamic(0x4000_000, None, Some((0, 0x4000_0000)))
            .unwrap()
            .expect("dynamic reserved region allocation failed");
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x3FD1_67A0,
                size: 0xA1,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x80_000,
                size: 0x3E0_0000,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x2000_0000,
                size: 0x13B29,
            })
            .unwrap();

        allocator.check_regions().unwrap();

        let addr_big = allocator
            .allocate_region_internal(0x8000, 0x8000)
            .expect("failed to allocate 32KiB aligned block");
        assert_eq!(addr_big & 0x7FFF, 0);

        for _ in 0..0x1000 {
            let addr = allocator
                .allocate_region_internal(0x1_000, 0x1000)
                .expect("failed to allocate 4KiB page");
            assert_eq!(addr & 0xFFF, 0);
        }
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
        assert_eq!(allocator.check_regions(), Ok(()));
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
        assert_eq!(allocator.check_regions(), Ok(()));
        assert_eq!(allocator.region_size, 0);
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
        assert_eq!(allocator.allocate_region(0x100, 0x10), None);
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

        let ptr = allocator.allocate_region(0x100, 0x10);
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

        assert_eq!(allocator.allocate_region(0x200, 0x10), None);
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

        let ptr = allocator.allocate_region(0x100, 0x10);
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

        let ptr = allocator.allocate_region(0x100, 0x100);
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
            assert!(allocator.allocate_region(0x10, 0x1000).is_some());
            // Each allocation creates a split, increasing region_size.
        }
        // After 118 allocations, region_size should be 119.
        assert_eq!(allocator.region_size, 119);
        assert_eq!(allocator.region_capacity, initial_region_capacity);
        // This allocation should trigger overflow_wrapping.
        assert!(allocator.allocate_region(0x10, 0x1000).is_some());
        // Verify that the capacities have been doubled.
        assert_eq!(allocator.region_capacity, initial_region_capacity * 2);
        assert_eq!(
            allocator.reserved_region_capacity,
            initial_reserved_capacity * 2
        );

        // region_size should be 120 now.
        assert_eq!(allocator.region_size, 120);

        // Verify that we can still allocate after wrapping.
        assert!(allocator.allocate_region(0x10, 0x1000).is_some());
        assert_eq!(allocator.region_size, 121);

        let capacity = allocator.region_capacity;
        let reserved = allocator.trim_for_boot(0x10).unwrap();
        assert_eq!(reserved.len(), 121);
        assert_eq!(allocator.region_capacity, capacity);
        assert_eq!(allocator.region_size, 1);
        assert!(allocator.regions[1..].iter().all(|&r| r == EMPTY_REGION));
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
        let ptr = allocator
            .allocate_region(0x100, 0x10)
            .expect("alloc failed");
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
        let p1 = allocator.allocate_region(0x100, 0x100).unwrap(); // 0x1000..0x1100
        let p2 = allocator.allocate_region(0x100, 0x100).unwrap(); // 0x1100..0x1200
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
        let p1 = allocator.allocate_region(0x100, 0x100).unwrap(); // 0x1000..0x1100
        let p2 = allocator.allocate_region(0x100, 0x100).unwrap(); // 0x1100..0x1200
        let p3 = allocator.allocate_region(0x100, 0x100).unwrap(); // 0x1200..0x1300
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

    #[test]
    fn test_add_reserved_region_dynamic_pre_finalize_basic() {
        let mut allocator = MemoryBlock::init();
        // Add a single free region [0x1000, 0x2000)
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();

        // Request 0x100 bytes aligned to 0x100 within [0x1010, 0x1510)
        let addr = allocator
            .add_reserved_region_dynamic(0x100, Some(0x100), Some((0x1010, 0x500)))
            .unwrap();

        // Alignment rounds up 0x1010 to 0x1100
        assert_eq!(addr, Some(0x1100));
        assert_eq!(allocator.reserved_region_size, 1);
        assert_eq!(
            allocator.reserved_regions[0],
            MemoryRegions {
                address: 0x1100,
                size: 0x100
            }
        );

        // Finalize and ensure the reserved area is subtracted
        allocator.check_regions().unwrap();
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
    }

    #[test]
    fn test_add_reserved_region_dynamic_pre_finalize_overlap_returns_none() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        // Pre-existing reserved at [0x1100, 0x1200)
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x1100,
                size: 0x100,
            })
            .unwrap();

        // Range [0x1080, 0x1280) rounds up to 0x1100 which overlaps, implementation skips this region
        let addr = allocator
            .add_reserved_region_dynamic(0x100, Some(0x100), Some((0x1080, 0x200)))
            .unwrap();
        assert_eq!(addr, None);
    }

    #[test]
    fn test_add_reserved_region_dynamic_post_finalize_returns_err() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();
        allocator.check_regions().unwrap();

        // After finalize, dynamic reserved add should return Err
        let res = allocator.add_reserved_region_dynamic(0x100, Some(0x80), Some((0x1000, 0x1000)));
        assert!(res.is_err());
    }

    #[test]
    fn test_add_reserved_region_dynamic_err_index_equal_len_should_succeed_when_fixed() {
        let mut allocator = MemoryBlock::init();

        // Two pre-existing reserved regions below the free region range
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x1000,
                size: 0x100,
            })
            .unwrap();
        allocator
            .add_reserved_region(&MemoryRegions {
                address: 0x2000,
                size: 0x100,
            })
            .unwrap();

        // Free regions: provide small regions covering the two existing reserved
        // areas so that finalization can succeed, plus a larger region after them
        // to exercise Err(len) in binary_search for region_start = 0x5000.
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x100, // covers [0x1000,0x1100)
            })
            .unwrap();
        allocator
            .add_region(&MemoryRegions {
                address: 0x2000,
                size: 0x100, // covers [0x2000,0x2100)
            })
            .unwrap();
        allocator
            .add_region(&MemoryRegions {
                address: 0x5000,
                size: 0x1000, // [0x5000, 0x6000)
            })
            .unwrap();

        // With a correct implementation, this returns Some(0x5000) and records
        // the reservation into reserved_regions at the end.
        let addr = allocator
            .add_reserved_region_dynamic(0x80, Some(0x80), None)
            .expect("unexpected Err from pre-finalize dynamic reserve");
        assert_eq!(addr, Some(0x5000));

        // Verify reservation recorded as the last reserved region
        assert_eq!(allocator.reserved_region_size, 3);
        assert_eq!(
            allocator.reserved_regions[2],
            MemoryRegions {
                address: 0x5000,
                size: 0x80
            }
        );

        // Finalize; available region should have the reserved [0x5000,0x5080) subtracted
        allocator.check_regions().unwrap();
        // Two small regions are fully consumed by the earlier fixed reservations,
        // the third region is partially consumed by the dynamic reservation.
        assert_eq!(allocator.region_size, 1);
        assert_eq!(
            allocator.regions[0],
            MemoryRegions {
                address: 0x5080,
                size: 0xF80
            }
        );
    }

    #[test]
    fn allocate_region_alignment_overflow_returns_none() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: usize::MAX - 0xF,
                size: 0x8,
            })
            .unwrap();
        allocator.check_regions().unwrap();

        let addr = allocator.allocate_region(0x4, 0x20);
        assert_eq!(addr, None);
    }

    #[test]
    fn add_reserved_region_dynamic_alloc_range_overflow_returns_err() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: 0x1000,
                size: 0x1000,
            })
            .unwrap();

        let res = allocator.add_reserved_region_dynamic(
            0x10,
            Some(0x10),
            Some((usize::MAX - 0x10, 0x20)),
        );
        assert_eq!(res, Err("alloc_range overflow"));
    }

    #[test]
    fn add_reserved_region_dynamic_align_up_overflow_returns_none() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions {
                address: usize::MAX - 0xF,
                size: 0x8,
            })
            .unwrap();

        let res = allocator.add_reserved_region_dynamic(0x8, Some(0x20), None);
        assert_eq!(res, Ok(None));
        assert_eq!(allocator.reserved_region_size, 0);
    }

    #[test]
    fn add_region_rejects_end_overflow() {
        let mut allocator = MemoryBlock::init();
        let res = allocator.add_region(&MemoryRegions::from_parts(usize::MAX - 0x10, 0x20));
        assert_eq!(res, Err("region end overflow"));
    }

    #[test]
    fn check_regions_returns_err_on_corrupt_reserved_region_end_overflow() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions::from_parts(0x1000, 0x1000))
            .unwrap();
        allocator.reserved_regions[0] = MemoryRegions::from_parts(usize::MAX - 0x10, 0x20);
        allocator.reserved_region_size = 1;

        let res = allocator.check_regions();
        assert_eq!(res, Err("region end overflow"));
    }

    #[test]
    fn try_deallocate_region_err_on_dealloc_end_overflow_no_mutation() {
        let mut allocator = MemoryBlock::init();
        allocator
            .add_region(&MemoryRegions::from_parts(0x1000, 0x1000))
            .unwrap();
        allocator.check_regions().unwrap();

        let _alloc_layout = Layout::from_size_align(0x100, 0x10).unwrap();
        let alloc_ptr = allocator
            .allocate_region(0x100, 0x10)
            .expect("allocation should succeed");

        let pre_region_size = allocator.region_size;
        let pre_reserved_size = allocator.reserved_region_size;

        let res = allocator
            .try_deallocate_region(usize::MAX - 0x8, Layout::from_size_align(0x20, 1).unwrap());
        assert_eq!(res, Err("dealloc end overflow"));
        assert_eq!(allocator.region_size, pre_region_size);
        assert_eq!(allocator.reserved_region_size, pre_reserved_size);
        assert_eq!(
            allocator.reserved_regions[0],
            MemoryRegions::from_parts(alloc_ptr, 0x100)
        );
    }

    #[test]
    fn add_and_merge_region_err_on_preexisting_region_end_overflow() {
        let mut allocator = MemoryBlock::init();
        allocator.regions[0] = MemoryRegions::from_parts(usize::MAX - 0x10, 0x20);
        allocator.region_size = 1;

        let res = allocator.add_region(&MemoryRegions::from_parts(0x1000, 0x1000));
        assert_eq!(res, Err("region end overflow"));
    }
}
