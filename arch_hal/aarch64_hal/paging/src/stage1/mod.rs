use cpu::get_sctlr_el2;
use cpu::isb;

use crate::PAGE_TABLE_SIZE;
use crate::PagingErr;
use crate::new_table;
use crate::registers::HCR_EL2;
use crate::stage1::descriptors::Stage1_48bitLeafDescriptor;
use crate::stage1::descriptors::Stage1_48bitTableDescriptor;
use crate::stage1::descriptors::Stage1AP;
use crate::stage1::registers::InnerCache;
use crate::stage1::registers::MAIR_EL2;
use crate::stage1::registers::MairDeviceAttr;
use crate::stage1::registers::MairEntry;
use crate::stage1::registers::MairNormalAttr;
use crate::stage1::registers::OuterCache;
use crate::stage1::registers::PhysicalAddressSize;
use crate::stage1::registers::SCTLR_EL2;
use crate::stage1::registers::Shareability;
use crate::stage1::registers::TCR_EL2;
use crate::stage1::registers::TG0;
mod descriptors;
mod registers;

pub struct EL2Stage1Paging;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EL2Stage1PageTypes {
    Normal = 0b0,
    Device = 0b1,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct EL2Stage1PagingSetting {
    pub va: usize,
    pub pa: usize,
    pub size: usize,
    pub types: EL2Stage1PageTypes,
}

impl EL2Stage1Paging {
    /// initialize stage1 paging
    /// 4KiB granule
    /// - level 0 table: 512GiB 2^39
    /// - level 1 table: 1GiB 2^30
    /// - level 2 table: 2MiB 2^21
    /// - level 3 table: 4KiB 2^12
    ///
    /// # safety
    ///     data must be ascending order
    pub fn init_stage1paging(data: &[EL2Stage1PagingSetting]) -> Result<(), PagingErr> {
        if data.is_empty() {
            return Err(PagingErr::Corrupted);
        }

        // set HCR_EL2.E2H = 0
        let hcr = HCR_EL2::from_bits(cpu::get_hcr_el2())
            .set(HCR_EL2::e2h, 0b0)
            .bits();
        cpu::set_hcr_el2(hcr);
        cpu::isb();

        let (ps, t0sz, top_table_level) = match cpu::get_parange() {
            Some(pa) => match pa {
                cpu::registers::PARange::PA32bits4GB => {
                    (PhysicalAddressSize::AddressSize32b, 32, 1)
                }
                cpu::registers::PARange::PA36bits64GB => {
                    (PhysicalAddressSize::AddressSize36b, 28, 1)
                }
                cpu::registers::PARange::PA40bits1TB => {
                    (PhysicalAddressSize::AddressSize40b, 24, 0)
                }
                cpu::registers::PARange::PA42bits4TB => {
                    (PhysicalAddressSize::AddressSize42b, 22, 0)
                }
                cpu::registers::PARange::PA44bits16TB => {
                    (PhysicalAddressSize::AddressSize44b, 20, 0)
                }
                cpu::registers::PARange::PA48bits256TB => {
                    (PhysicalAddressSize::AddressSize48b, 16, 0)
                }
                // va over 52bit is not supported
                // va size == 48bit
                cpu::registers::PARange::PA52bits4PB => {
                    (PhysicalAddressSize::AddressSize52b, 16, 0)
                }
                cpu::registers::PARange::PA56bits64PB => {
                    (PhysicalAddressSize::AddressSize56b, 16, 0)
                }
            },
            None => return Err(PagingErr::Corrupted),
        };

        let mair_el2 = MAIR_EL2::new()
            .set(
                MAIR_EL2::attr0,
                MairEntry::Normal {
                    outer: MairNormalAttr::WriteBackRAWA,
                    inner: MairNormalAttr::WriteBackRAWA,
                }
                .to_u8() as u64,
            )
            .set(
                MAIR_EL2::attr1,
                MairEntry::Device(MairDeviceAttr::nGnRnE).to_u8() as u64,
            )
            .bits();
        cpu::set_mair_el2(mair_el2);

        let tcr_el2 = TCR_EL2::new()
            .set(TCR_EL2::t0sz, t0sz)
            .set_enum(TCR_EL2::irgn0, InnerCache::WBRAWACacheable)
            .set_enum(TCR_EL2::orgn0, OuterCache::WBRAWACacheable)
            .set_enum(TCR_EL2::sh0, Shareability::InnerSharable)
            .set_enum(TCR_EL2::tg0, TG0::Granule4KB)
            .set_enum(TCR_EL2::ps, ps)
            .bits();
        cpu::set_tcr_el2(tcr_el2);

        let table = Self::setup_stage1_translation(data, top_table_level)?;
        debug_assert_eq!(table & 0xfff, 0);
        cpu::set_ttbr0_el2(table as u64);

        cpu::isb();
        cpu::dsb_sy();

        let sctlr_el2 = SCTLR_EL2::from_bits(get_sctlr_el2())
            .set(SCTLR_EL2::m, 0b1)
            .set(SCTLR_EL2::c, 0b1)
            .set(SCTLR_EL2::i, 0b1)
            .bits();

        cpu::set_sctlr_el2(sctlr_el2);
        cpu::flush_tlb_el2();
        cpu::isb();
        Ok(())
    }

    fn setup_stage1_translation(
        data: &[EL2Stage1PagingSetting],
        top_table_level: i8,
    ) -> Result<usize, PagingErr> {
        let table = new_table()?;

        let top_level_offset = (3 - top_table_level) as usize * 9 + 12;
        let top_level = 1 << top_level_offset;
        let mut i = 0;
        let mut pa = data[0].pa;
        let mut va = data[0].va;
        let mut size = data[0].size;
        if size == 0 {
            return Err(PagingErr::ZeroSizedPage);
        }
        if (pa | va | size) & (PAGE_TABLE_SIZE - 1) != 0 {
            return Err(PagingErr::UnalignedPage);
        }
        loop {
            if i == data.len() {
                break;
            }
            let idx = va >> top_level_offset;
            if top_table_level != 0 && (pa | va) & (top_level - 1) == 0 && size >= top_level {
                // block descriptor
                debug_assert_eq!(table[idx], 0);
                table[idx] = Stage1_48bitLeafDescriptor::new_block(
                    pa as u64,
                    top_table_level,
                    data[i].types as u8,
                    Stage1AP::RW_Any,
                    false,
                );
                pa += top_level;
                va += top_level;
                size -= top_level;
                if size == 0 {
                    Self::increment_and_check(&mut i, data, &mut pa, &mut va, &mut size)?;
                }
            } else {
                // table descriptor
                let next_level_table = new_table()?;
                table[idx] =
                    Stage1_48bitTableDescriptor::new_descriptor(next_level_table.as_ptr() as u64);
                let start_va = va & !(top_level - 1);
                Self::setup_stage1_translation_recursive(
                    &mut i,
                    data,
                    top_table_level + 1,
                    next_level_table,
                    start_va,
                    &mut pa,
                    &mut va,
                    &mut size,
                )?;
            }
        }

        cpu::clean_dcache_poc(table.as_ptr() as usize, PAGE_TABLE_SIZE);
        Ok(table.as_ptr() as usize)
    }

    fn setup_stage1_translation_recursive(
        i: &mut usize,
        data: &[EL2Stage1PagingSetting],
        table_level: i8,
        table_addr: &mut [u64],
        start_va: usize,
        pa: &mut usize,
        va: &mut usize,
        size: &mut usize,
    ) -> Result<(), PagingErr> {
        let table_level_offset = (3 - table_level) as usize * 9 + 12;
        let table_level_size = 1 << table_level_offset;

        let table_limit = start_va + table_level_size * 512;

        while *i < data.len() && *va < table_limit {
            // is block descriptor
            if table_level == 3
                || ((*pa | *va) & (table_level_size - 1) == 0 && *size >= table_level_size)
            {
                // check table level 3 is aligned PAGE_SIZE
                debug_assert_eq!((*pa | *va | *size) & (PAGE_TABLE_SIZE - 1), 0);
                // block descriptor
                let idx = (*va - start_va) >> table_level_offset;
                debug_assert_eq!(table_addr[idx], 0);
                table_addr[idx] = if table_level == 3 {
                    Stage1_48bitLeafDescriptor::new_page(
                        *pa as u64,
                        data[*i].types as u8,
                        Stage1AP::RW_Any,
                        false,
                    )
                } else {
                    Stage1_48bitLeafDescriptor::new_block(
                        *pa as u64,
                        table_level,
                        data[*i].types as u8,
                        Stage1AP::RW_Any,
                        false,
                    )
                };
                *pa += table_level_size;
                *va += table_level_size;
                *size -= table_level_size;
                if *size == 0 {
                    Self::increment_and_check(i, data, pa, va, size)?;
                }
            } else {
                // table descriptor
                let next_level_table = new_table()?;
                for j in &mut *next_level_table {
                    *j = 0;
                }
                let idx = (*va - start_va) >> table_level_offset;
                debug_assert_eq!(table_addr[idx], 0);
                table_addr[idx] =
                    Stage1_48bitTableDescriptor::new_descriptor(next_level_table.as_ptr() as u64);
                let start_va = *va & !(table_level_size - 1);
                Self::setup_stage1_translation_recursive(
                    i,
                    data,
                    table_level + 1,
                    next_level_table,
                    start_va,
                    pa,
                    va,
                    size,
                )?;
            }
        }
        cpu::clean_dcache_poc(table_addr.as_ptr() as usize, PAGE_TABLE_SIZE);
        Ok(())
    }

    fn increment_and_check(
        i: &mut usize,
        data: &[EL2Stage1PagingSetting],
        pa: &mut usize,
        va: &mut usize,
        size: &mut usize,
    ) -> Result<(), PagingErr> {
        *i += 1;
        if *i == data.len() {
            return Ok(());
        }
        if data[*i].size == 0 {
            return Err(PagingErr::ZeroSizedPage);
        }
        if (data[*i].pa | data[*i].va | data[*i].size) & (PAGE_TABLE_SIZE - 1) != 0 {
            return Err(PagingErr::UnalignedPage);
        }
        *pa = data[*i].pa;
        *va = data[*i].va;
        *size = data[*i].size;
        Ok(())
    }
}
