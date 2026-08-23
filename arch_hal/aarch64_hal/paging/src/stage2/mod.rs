use alloc::boxed::Box;
use core::alloc::Layout;
use core::cell::SyncUnsafeCell;
use core::mem;
use core::slice;
use cpu::isb;

use cpu::registers::PARange;

use crate::PAGE_TABLE_SIZE;
use crate::PagingErr;
use crate::new_table;
use crate::registers::HCR_EL2;
use crate::stage2::descriptor::Stage2_48bitLeafDescriptor;
use crate::stage2::descriptor::Stage2_48bitTableDescriptor;
pub use crate::stage2::descriptor::Stage2AccessPermission;
use crate::stage2::registers::InnerCache;
use crate::stage2::registers::OuterCache;
use crate::stage2::registers::PhysicalAddressSize;
use crate::stage2::registers::SL0;
use crate::stage2::registers::Shareability;
use crate::stage2::registers::TG0;
use crate::stage2::registers::VTCR_EL2;

mod descriptor;
mod registers;

pub struct Stage2Paging;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage2PageTypes {
    Normal = 0b0,
    Device = 0b1,
}

#[derive(Clone, Copy, Debug)]
pub struct Stage2PagingSetting {
    pub ipa: usize,
    pub pa: usize,
    pub size: usize,
    pub types: Stage2PageTypes,
    pub perm: Stage2AccessPermission,
}

#[derive(Clone)]
struct Stage2Context {
    table_addr: usize,
    top_table_level: i8,
    num_of_tables: usize,
    settings: Box<[Stage2PagingSetting]>,
}

static STAGE2_CONTEXT: SyncUnsafeCell<Option<Stage2Context>> = SyncUnsafeCell::new(None);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage2UpdateError {
    NotInitialized,
    UnalignedPage,
    Unmapped,
    InvalidDescriptor,
    OutOfMemory,
}

impl Stage2Paging {
    /// initialize stage2 paging
    /// 4KiB granule
    /// - level 0 table: 512GiB 2^39
    /// - level 1 table: 1GiB 2^30
    /// - level 2 table: 2MiB 2^21
    /// - level 3 table: 4KiB 2^12
    ///
    /// # safety
    ///     data must be ascending order
    pub fn init_stage2paging(
        data: &[Stage2PagingSetting],
        allocator: &'static allocator::DefaultAllocator,
    ) -> Result<(), PagingErr> {
        if data.is_empty() {
            return Err(PagingErr::Corrupted);
        }

        // Concatenated translation tables (AArch64, Stage-2)
        // --------------------------------------------------
        // WHAT
        // • At the initial lookup of a Stage-2 translation you may replace the top level with
        //   multiple same-level tables concatenated side-by-side, and start the walk from
        //   the *next* lookup level (skipping one level).
        //
        // HOW MANY
        // • Up to 16 tables can be concatenated.
        // • If the initial lookup resolves n extra IA bits (beyond that level’s baseline),
        //   you must concatenate 2^n tables (n ≤ 4).
        //
        // WHEN (rule of thumb)
        // • If the table at the nominal initial level would need ≤ 16 entries for your IPA size
        //   and granule, you can “pull” those top bits into the initial lookup and start at the
        //   next level with 2^n concatenated tables.
        //
        // CONFIGURATION (software responsibilities)
        // • Program VTCR_EL2.SL0 (and SL2 when DS=1) to the *level you actually want to start from*
        //   — i.e., the *lower* level when using concatenation. Hardware does not auto-decrement
        //   the level; you choose it via SL0(/SL2).
        // • Program VTTBR_EL2 to the base of the *first* table in the concatenated set and satisfy
        //   the alignment implied by the concatenated size.
        // • Ensure DS (VTCR_EL2.DS) and IPS/PS settings are consistent with the address size used.
        //
        // 4KB GRANULE EXAMPLES (Stage-2)
        // • Initial level L1 baseline covers IA[38:12]. With concatenation:
        //   - IA[39:12] → 2 tables, IA[40:12] → 4 tables, IA[41:12] → 8 tables, IA[42:12] → 16 tables.
        // • Initial level L0 baseline covers IA[47:12]. With DS=1 for >48-bit IA:
        //   - IA[48:12] → 2 tables, IA[49:12] → 4 tables, IA[50:12] → 8 tables, IA[51:12] → 16 tables.
        //   (Plain 48-bit at L0 needs no concatenation.)
        //
        // WHY
        // • Eliminates one top-level lookup, reducing table-walk overhead.
        let (ps, t0sz, initial_lookup_level, initial_lookup_level_i8, num_of_tables) =
            match cpu::get_parange() {
                Some(pa) => {
                    match pa {
                        // pa size == ipa size
                        PARange::PA32bits4GB => {
                            (PhysicalAddressSize::AddressSize32b, 32, SL0::Level1, 1, 1)
                        }
                        PARange::PA36bits64GB => {
                            (PhysicalAddressSize::AddressSize36b, 28, SL0::Level1, 1, 1)
                        }
                        PARange::PA40bits1TB => {
                            (PhysicalAddressSize::AddressSize40b, 24, SL0::Level1, 1, 2)
                        }
                        PARange::PA42bits4TB => {
                            (PhysicalAddressSize::AddressSize42b, 22, SL0::Level1, 1, 8)
                        }
                        PARange::PA44bits16TB => {
                            (PhysicalAddressSize::AddressSize44b, 20, SL0::Level0, 0, 1)
                        }
                        PARange::PA48bits256TB => {
                            (PhysicalAddressSize::AddressSize48b, 16, SL0::Level0, 0, 1)
                        }
                        // ipa 52bit is not supported
                        // ipa size == 48bit
                        PARange::PA52bits4PB => {
                            (PhysicalAddressSize::AddressSize52b, 16, SL0::Level0, 0, 1)
                        }
                        PARange::PA56bits64PB => {
                            (PhysicalAddressSize::AddressSize56b, 16, SL0::Level0, 0, 1)
                        }
                    }
                }
                None => return Err(PagingErr::Corrupted),
            };

        // mapping page table
        let table = Self::setup_stage2_translation(
            data,
            initial_lookup_level_i8,
            num_of_tables,
            allocator,
        )?;
        cpu::set_vttbr_el2(table as u64);
        unsafe {
            (*STAGE2_CONTEXT.get()) = Some(Stage2Context {
                table_addr: table,
                top_table_level: initial_lookup_level_i8,
                num_of_tables,
                settings: Box::from(data),
            });
        }

        let vtcr_el2 = VTCR_EL2::new()
            .set(VTCR_EL2::t0sz, t0sz)
            .set_enum(VTCR_EL2::sl0, initial_lookup_level)
            .set_enum(VTCR_EL2::irgn0, InnerCache::WBRAnWACacheable)
            .set_enum(VTCR_EL2::orgn0, OuterCache::WBRAnWACacheable)
            .set_enum(VTCR_EL2::sh0, Shareability::InnerSharable)
            .set_enum(VTCR_EL2::tg0, TG0::Granule4KB)
            .set_enum(VTCR_EL2::ps, ps)
            .bits();

        cpu::set_vtcr_el2(vtcr_el2);
        cpu::flush_tlb_el2_el1();
        Ok(())
    }

    fn setup_stage2_translation(
        data: &[Stage2PagingSetting],
        top_table_level: i8,
        num_of_tables: usize,
        allocator: &'static allocator::DefaultAllocator,
    ) -> Result<usize, PagingErr> {
        let table_addr = allocator
            .allocate_with_size_and_align(
                PAGE_TABLE_SIZE * num_of_tables,
                PAGE_TABLE_SIZE * num_of_tables,
            )
            .map_err(|_| PagingErr::OutOfMemory)?;
        let table =
            unsafe { slice::from_raw_parts_mut(table_addr as *mut u64, num_of_tables * 512) };
        // initialize page table
        for i in &mut *table {
            *i = 0;
        }

        let top_level_offset = (3 - top_table_level) as usize * 9 + 12;
        let top_level = 1 << top_level_offset;

        let mut i = 0;
        let mut pa = data[0].pa;
        let mut ipa = data[0].ipa;
        let mut size = data[0].size;
        if data[0].size == 0 {
            return Err(PagingErr::ZeroSizedPage);
        }
        if (data[0].pa | data[0].ipa | data[0].size) & (PAGE_TABLE_SIZE - 1) != 0 {
            return Err(PagingErr::UnalignedPage);
        }
        loop {
            if i == data.len() {
                break;
            }
            let idx = initial_index_with_concat(ipa, top_table_level, num_of_tables);
            debug_assert!(idx < num_of_tables * 512);
            // is block descriptor
            if top_table_level != 0 && (pa | ipa) & (top_level - 1) == 0 && size >= top_level {
                // block descriptor
                debug_assert_eq!(table[idx], 0);
                table[idx] = Stage2_48bitLeafDescriptor::new_block(
                    pa as u64,
                    top_table_level,
                    data[i].types,
                    data[i].perm,
                );
                pa += top_level;
                ipa += top_level;
                size -= top_level;
                if size == 0 {
                    Self::increment_and_check(&mut i, data, &mut pa, &mut ipa, &mut size)?;
                }
            } else {
                // table descriptor
                let next_level_table = new_table()?;
                let next_level_table_addr = next_level_table.as_ptr() as usize;
                debug_assert_eq!(table[idx], 0);
                table[idx] =
                    Stage2_48bitTableDescriptor::new_descriptor(next_level_table_addr as u64);
                let start_ipa = ipa & !(top_level - 1);
                Self::setup_stage2_translation_recursive(
                    &mut i,
                    data,
                    top_table_level + 1,
                    next_level_table,
                    start_ipa,
                    &mut pa,
                    &mut ipa,
                    &mut size,
                )?;
            }
        }
        cpu::clean_dcache_poc(table_addr, PAGE_TABLE_SIZE * num_of_tables);
        Ok(table_addr)
    }

    fn setup_stage2_translation_recursive(
        i: &mut usize,
        data: &[Stage2PagingSetting],
        table_level: i8,
        table_addr: &mut [u64],
        start_ipa: usize,
        pa: &mut usize,
        ipa: &mut usize,
        size: &mut usize,
    ) -> Result<(), PagingErr> {
        let table_level_offset = (3 - table_level) as usize * 9 + 12;
        let table_level_size = 1 << table_level_offset;

        let table_limit = start_ipa + table_level_size * 512;

        while *i < data.len() && *ipa < table_limit {
            let setting = &data[*i];
            // is block descriptor
            if table_level == 3
                || ((*pa | *ipa) & (table_level_size - 1) == 0 && *size >= table_level_size)
            {
                // check table level 3 is aligned PAGE_SIZE
                debug_assert_eq!((*pa | *ipa | *size) & (PAGE_TABLE_SIZE - 1), 0);
                // block descriptor
                let idx = (*ipa - start_ipa) >> table_level_offset;
                debug_assert_eq!(table_addr[idx], 0);
                table_addr[idx] = if table_level == 3 {
                    Stage2_48bitLeafDescriptor::new_page(*pa as u64, data[*i].types, data[*i].perm)
                } else {
                    Stage2_48bitLeafDescriptor::new_block(
                        *pa as u64,
                        table_level,
                        data[*i].types,
                        data[*i].perm,
                    )
                };
                *pa += table_level_size;
                *ipa += table_level_size;
                *size -= table_level_size;
                if *size == 0 {
                    Self::increment_and_check(i, data, pa, ipa, size)?;
                }
            } else {
                // table descriptor
                let next_level_table = new_table()?;
                let next_level_table_addr = next_level_table.as_ptr() as usize;
                let idx = (*ipa - start_ipa) >> table_level_offset;
                debug_assert_eq!(table_addr[idx], 0);
                table_addr[idx] =
                    Stage2_48bitTableDescriptor::new_descriptor(next_level_table_addr as u64);
                let start_ipa = *ipa & !(table_level_size - 1);
                Self::setup_stage2_translation_recursive(
                    i,
                    data,
                    table_level + 1,
                    next_level_table,
                    start_ipa,
                    pa,
                    ipa,
                    size,
                )?;
            }
        }
        cpu::clean_dcache_poc(table_addr.as_ptr() as usize, PAGE_TABLE_SIZE);
        Ok(())
    }

    fn increment_and_check(
        i: &mut usize,
        data: &[Stage2PagingSetting],
        pa: &mut usize,
        ipa: &mut usize,
        size: &mut usize,
    ) -> Result<(), PagingErr> {
        *i += 1;
        if *i == data.len() {
            return Ok(());
        }
        if data[*i].size == 0 {
            return Err(PagingErr::ZeroSizedPage);
        }
        if (data[*i].pa | data[*i].ipa | data[*i].size) & (PAGE_TABLE_SIZE - 1) != 0 {
            return Err(PagingErr::UnalignedPage);
        }
        *pa = data[*i].pa;
        *ipa = data[*i].ipa;
        *size = data[*i].size;
        Ok(())
    }

    pub fn enable_stage2_translation(receive_irq: bool, receive_wfq: bool) {
        let mut hcr = HCR_EL2::new()
            .set(HCR_EL2::vm, 0b1)
            .set(HCR_EL2::api, 0b1)
            .set(HCR_EL2::tsc, 0b1)
            .set(HCR_EL2::rw, 0b1);
        if receive_irq {
            hcr = hcr
                .set(HCR_EL2::fmo, 0b1)
                .set(HCR_EL2::imo, 0b1)
                .set(HCR_EL2::amo, 0b1);
        }
        if receive_wfq {
            hcr = hcr.set(HCR_EL2::twi, 0b1).set(HCR_EL2::twe, 0b1);
        }
        cpu::set_hcr_el2(hcr.bits());
        cpu::isb();
    }

    /// Translate a guest IPA to a physical address using the installed Stage-2 tables.
    ///
    /// Stage-2 translation must already be configured and enabled before calling this helper.
    /// The returned physical address is expected to be directly accessible from EL2 (the current
    /// boot flow installs an identity mapping for RAM).
    pub fn ipa_to_pa(ipa: usize) -> Result<usize, PagingErr> {
        cpu::ipa_to_pa_el2(ipa as u64)
            .map(|pa| pa as usize)
            .ok_or(PagingErr::Stage2Fault)
    }

    /// Clear AF/DBM bits on every mapped leaf so subsequent accesses trap.
    pub fn dirty_bit_remove_all() -> Result<(), PagingErr> {
        let ctx = unsafe { &*STAGE2_CONTEXT.get() }
            .as_ref()
            .ok_or(PagingErr::Corrupted)?;
        let mut table = unsafe {
            slice::from_raw_parts_mut(ctx.table_addr as *mut u64, ctx.num_of_tables * 512)
        };
        Self::clear_dirty_bits_recursive(ctx.top_table_level, &mut table)?;
        cpu::flush_tlb_el2_el1();
        Ok(())
    }

    fn clear_dirty_bits_recursive(table_level: i8, table: &mut [u64]) -> Result<(), PagingErr> {
        let table_bytes = table.len() * mem::size_of::<u64>();
        for desc in table.iter_mut() {
            match *desc & 0b11 {
                0b01 => {
                    *desc = Stage2_48bitLeafDescriptor::clear_access_and_dirty(*desc);
                }
                0b11 => {
                    if table_level == 3 {
                        *desc = Stage2_48bitLeafDescriptor::clear_access_and_dirty(*desc);
                    } else {
                        let next_table_addr = (*desc as usize) & !(PAGE_TABLE_SIZE - 1);
                        let next_table =
                            unsafe { slice::from_raw_parts_mut(next_table_addr as *mut u64, 512) };
                        Self::clear_dirty_bits_recursive(table_level + 1, next_table)?;
                    }
                }
                _ => {}
            }
        }
        cpu::clean_dcache_poc(table.as_ptr() as usize, table_bytes);
        Ok(())
    }
}

pub fn set_4k_exec_only(ipa_page: u64) -> Result<(), Stage2UpdateError> {
    update_4k_leaf(ipa_page, Stage2AccessPermission::NoDataAccess, Some(0))
}

pub fn set_4k_rw(ipa_page: u64) -> Result<(), Stage2UpdateError> {
    update_4k_leaf(ipa_page, Stage2AccessPermission::ReadWrite, None)
}

fn update_4k_leaf(
    ipa_page: u64,
    access: Stage2AccessPermission,
    xn_override: Option<u64>,
) -> Result<(), Stage2UpdateError> {
    if (ipa_page & (PAGE_TABLE_SIZE as u64 - 1)) != 0 {
        return Err(Stage2UpdateError::UnalignedPage);
    }
    // SAFETY: STAGE2_CONTEXT is initialized once during boot before concurrent access.
    let ctx = unsafe { &*STAGE2_CONTEXT.get() }
        .as_ref()
        .ok_or(Stage2UpdateError::NotInitialized)?;
    let total_entries = ctx.num_of_tables * 512;
    // SAFETY: `table_addr` points to the stage-2 table pool with `total_entries` u64 entries.
    let mut table = unsafe { slice::from_raw_parts_mut(ctx.table_addr as *mut u64, total_entries) };

    let mut level = ctx.top_table_level;
    let mut table_slice: &mut [u64] = &mut table;

    loop {
        let idx = if level == ctx.top_table_level {
            initial_index_with_concat(ipa_page as usize, level, ctx.num_of_tables)
        } else {
            index_for_level(ipa_page, level)
        };
        let entry = &mut table_slice[idx];
        match *entry & 0b11 {
            0b00 => return Err(Stage2UpdateError::Unmapped),
            0b01 => {
                if level >= 3 {
                    return Err(Stage2UpdateError::InvalidDescriptor);
                }
                let next_table = split_block_descriptor(*entry, level)?;
                *entry = Stage2_48bitTableDescriptor::new_descriptor(next_table.as_ptr() as u64);
                clean_table_entry(entry as *const u64);
                table_slice = next_table;
                level += 1;
            }
            0b11 => {
                if level == 3 {
                    let updated = update_leaf_descriptor(*entry, access, xn_override)?;
                    *entry = updated;
                    clean_table_entry(entry as *const u64);
                    cpu::flush_tlb_el2_el1();
                    return Ok(());
                }
                let next_table_addr = (*entry as usize) & !(PAGE_TABLE_SIZE - 1);
                // SAFETY: next-level table address comes from a validated table descriptor.
                table_slice =
                    unsafe { slice::from_raw_parts_mut(next_table_addr as *mut u64, 512) };
                level += 1;
            }
            _ => return Err(Stage2UpdateError::InvalidDescriptor),
        }
    }
}

fn split_block_descriptor(desc: u64, level: i8) -> Result<&'static mut [u64], Stage2UpdateError> {
    if !(level == 1 || level == 2) {
        return Err(Stage2UpdateError::InvalidDescriptor);
    }
    let table_level_offset = (3 - (level + 1) as usize) * 9 + 12;
    let next_level_size = 1usize << table_level_offset;

    // SAFETY: allocate a new stage-2 table page; it is owned exclusively hereafter.
    let next_table_addr = unsafe {
        alloc::alloc::alloc(Layout::from_size_align_unchecked(
            PAGE_TABLE_SIZE,
            PAGE_TABLE_SIZE,
        ))
    };
    if next_table_addr.is_null() {
        return Err(Stage2UpdateError::OutOfMemory);
    }
    let next_table_addr = next_table_addr as usize;
    // SAFETY: `next_table_addr` is a freshly allocated table page owned here.
    let next_table = unsafe { slice::from_raw_parts_mut(next_table_addr as *mut u64, 512) };

    let leaf = Stage2_48bitLeafDescriptor::from_bits(desc);
    let base_pa = leaf.get_raw(Stage2_48bitLeafDescriptor::block_oab);
    let is_page_level = level == 2;
    let ty_bits = if is_page_level { 0b11 } else { 0b01 };

    for (idx, slot) in next_table.iter_mut().enumerate() {
        let pa = base_pa + (idx as u64 * next_level_size as u64);
        let mut new_leaf = Stage2_48bitLeafDescriptor::from_bits(desc)
            .set(Stage2_48bitLeafDescriptor::ty, ty_bits);
        if is_page_level {
            new_leaf = new_leaf.set_raw(Stage2_48bitLeafDescriptor::page_oab, pa);
        } else {
            new_leaf = new_leaf.set_raw(Stage2_48bitLeafDescriptor::block_oab, pa);
        }
        *slot = new_leaf.bits();
    }

    cpu::clean_dcache_poc(next_table_addr, PAGE_TABLE_SIZE);
    Ok(next_table)
}

fn update_leaf_descriptor(
    desc: u64,
    access: Stage2AccessPermission,
    xn_override: Option<u64>,
) -> Result<u64, Stage2UpdateError> {
    let mut leaf = Stage2_48bitLeafDescriptor::from_bits(desc)
        .set(Stage2_48bitLeafDescriptor::s2ap, access as u64);
    if let Some(xn) = xn_override {
        leaf = leaf.set(Stage2_48bitLeafDescriptor::xn, xn);
    }
    Ok(leaf.bits())
}

#[inline]
fn clean_table_entry(entry: *const u64) {
    let page = (entry as usize) & !(PAGE_TABLE_SIZE - 1);
    cpu::clean_dcache_poc(page, PAGE_TABLE_SIZE);
}

#[inline]
fn initial_index_with_concat(ipa: usize, level: i8, num_concat: usize) -> usize {
    debug_assert!(num_concat.is_power_of_two());
    let shift = 12 + 9 * (3 - level as usize);
    (ipa >> shift) & (num_concat * 512 - 1)
}

#[inline]
fn index_for_level(ipa: u64, level: i8) -> usize {
    let shift = 12 + 9 * (3 - level as usize);
    ((ipa >> shift) & 0x1ff) as usize
}

#[cfg(all(test, not(target_arch = "aarch64")))]
mod tests {
    use super::*;

    #[test]
    fn leaf_exec_only_sets_s2ap_and_xn() {
        let desc = Stage2_48bitLeafDescriptor::new_page(
            0x4000_0000,
            Stage2PageTypes::Normal,
            Stage2AccessPermission::ReadWrite,
        );
        let updated = update_leaf_descriptor(desc, Stage2AccessPermission::NoDataAccess, Some(0))
            .expect("update leaf");
        let leaf = Stage2_48bitLeafDescriptor::from_bits(updated);
        assert_eq!(leaf.get(Stage2_48bitLeafDescriptor::s2ap), 0);
        assert_eq!(leaf.get(Stage2_48bitLeafDescriptor::xn), 0);
    }
}
