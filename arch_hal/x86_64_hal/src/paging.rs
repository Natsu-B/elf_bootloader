//! Minimal long-mode page-table walking for trusted nested guests.

/// CR4.LA57 selects five-level linear-address translation.
const CR4_LA57: u64 = 1 << 12;
/// Present bit in a paging-structure entry.
const PRESENT: u64 = 1;
/// Page-size bit in a PDPTE or PDE.
const PAGE_SIZE: u64 = 1 << 7;
/// Architectural address field for CPUs with at most 52 physical bits.
const MAX_ADDRESS_FIELD: u64 = 0x000f_ffff_ffff_f000;

/// Architectural inputs for an explicit long-mode guest data access.
#[derive(Clone, Copy, Debug)]
pub struct DataAccess {
    /// Guest CR0, including supervisor write protection.
    pub cr0: u64,
    /// Guest CR3, including PCID and LAM controls.
    pub cr3: u64,
    /// Architectural guest CR4, not the private host CR4.
    pub cr4: u64,
    /// Guest EFER, including NX enablement.
    pub efer: u64,
    /// Guest RFLAGS; AC controls explicit supervisor accesses under SMAP.
    pub rflags: u64,
    /// Guest privilege, 0 through 3.
    pub cpl: u8,
    /// CPUID physical-address width.
    pub physical_bits: u8,
    /// Whether the guest CPU supports 1 GiB ordinary paging leaves.
    pub page_1g: bool,
    /// Live guest protection-key rights for user pages.
    pub pkru: u32,
    /// Live guest protection-key rights for supervisor pages.
    pub pkrs: u32,
}

/// A precise walk failure; backing failures are not fabricated guest #PFs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataFault {
    /// Noncanonical address or an operand spanning the linear-address boundary.
    /// The instruction decoder chooses #SS(0) for SS, otherwise #GP(0).
    Address,
    /// Architectural #PF, with the faulting linear address and complete code.
    Page {
        /// Untagged linear address to publish in guest CR2.
        linear: u64,
        /// P/W/U/RSVD/PK bits for this explicit data access.
        error: u32,
    },
    /// Invalid CPU context supplied by the monitor, not a guest operand fault.
    InvalidState,
    /// The monitor cannot access the backing physical paging-structure word.
    /// Its platform mapping must be repaired; this is not a nonpresent PTE.
    Backing(u64),
}

impl DataAccess {
    /// Removes LAM metadata only for explicit data operands. Invalidation
    /// addresses and instruction fetches must not use this operation.
    #[must_use]
    pub const fn untag(self, address: u64) -> u64 {
        let sign_bit = if address & (1 << 63) == 0 {
            if self.cr3 & (1 << 61) != 0 {
                56
            } else if self.cr3 & (1 << 62) != 0 {
                47
            } else {
                return address;
            }
        } else if self.cr4 & (1 << 28) != 0 {
            if self.cr4 & CR4_LA57 != 0 { 56 } else { 47 }
        } else {
            return address;
        };
        let shift = 63 - sign_bit;
        let extended = ((address << shift) as i64 >> shift) as u64;
        (extended & !(1 << 63)) | (address & (1 << 63))
    }

    /// Validates the entire operand before any page-table or payload access.
    /// The returned start has LAM metadata removed once, before adding offsets.
    pub fn operand_range(self, linear: u64, bytes: usize) -> Result<u64, DataFault> {
        if !(12..=52).contains(&self.physical_bits) || self.cpl > 3 || bytes == 0 {
            return Err(DataFault::InvalidState);
        }
        let start = self.untag(linear);
        let end = start
            .checked_add((bytes - 1) as u64)
            .ok_or(DataFault::Address)?;
        let bits = if self.cr4 & CR4_LA57 != 0 { 57 } else { 48 };
        if !is_canonical(start, bits) || !is_canonical(end, bits) {
            return Err(DataFault::Address);
        }
        Ok(start)
    }

    fn page_fault(self, linear: u64, write: bool, flags: u32) -> DataFault {
        DataFault::Page {
            linear,
            error: flags | (u32::from(write) << 1) | (u32::from(self.cpl == 3) << 2),
        }
    }

    /// Checks effective permissions from every walk level and the leaf's key.
    fn permissions(
        self,
        linear: u64,
        write: bool,
        user: bool,
        writable: bool,
        leaf: u64,
    ) -> Result<(), DataFault> {
        let write_protected = self.cpl == 3 || self.cr0 & (1 << 16) != 0;
        if (self.cpl == 3 && !user)
            || (write && write_protected && !writable)
            || (self.cpl < 3 && user && self.cr4 & (1 << 21) != 0 && self.rflags & (1 << 18) == 0)
        {
            return Err(self.page_fault(linear, write, 1));
        }
        let keys = if user && self.cr4 & (1 << 22) != 0 {
            self.pkru
        } else if !user && self.cr4 & (1 << 24) != 0 {
            self.pkrs
        } else {
            0
        };
        let rights = (keys >> (((leaf >> 59) & 15) * 2)) & 3;
        if rights & 1 != 0 || (write && write_protected && rights & 2 != 0) {
            return Err(self.page_fault(linear, write, 1 | (1 << 5)));
        }
        Ok(())
    }
}

/// Whether an address is canonical for a validated 48- or 57-bit paging mode.
#[must_use]
pub const fn is_canonical(address: u64, bits: u8) -> bool {
    if bits != 48 && bits != 57 {
        return false;
    }
    let shift = 64 - bits;
    ((address << shift) as i64 >> shift) as u64 == address
}

/// Walks one already range-checked, untagged explicit data address.
///
/// `access_entry(address, 0)` reads a little-endian paging word. A nonzero second
/// argument requests an OR of architectural A/D bits into that word. The caller
/// must serialize those updates with other CPUs' page-table updates, and report
/// inaccessible backing as `None`, never as a fabricated zero PTE. A may become
/// set in traversed parents even when a later level faults; D is set only after
/// a writable leaf passes every permission check. No payload store happens here.
pub fn translate_data(
    linear: u64,
    state: DataAccess,
    write: bool,
    mut access_entry: impl FnMut(u64, u64) -> Option<u64>,
) -> Result<u64, DataFault> {
    if !(12..=52).contains(&state.physical_bits) || state.cpl > 3 {
        return Err(DataFault::InvalidState);
    }
    let five = state.cr4 & CR4_LA57 != 0;
    if !is_canonical(linear, if five { 57 } else { 48 }) {
        return Err(DataFault::Address);
    }
    let mask = ((1_u64 << state.physical_bits) - 1) & !0xfff;
    if state.cr3 & MAX_ADDRESS_FIELD & !mask != 0 {
        return Err(DataFault::InvalidState);
    }
    let mut table = state.cr3 & mask;
    let mut user = true;
    let mut writable = true;
    let shifts: &[u8] = if five {
        &[48, 39, 30, 21, 12]
    } else {
        &[39, 30, 21, 12]
    };
    for &shift in shifts {
        let slot = table + ((linear >> shift) & 511) * 8;
        let entry = access_entry(slot, 0).ok_or(DataFault::Backing(slot))?;
        if entry & PRESENT == 0 {
            return Err(state.page_fault(linear, write, 0));
        }
        let large = shift != 12 && entry & PAGE_SIZE != 0;
        if entry & MAX_ADDRESS_FIELD & !mask != 0
            || (entry & (1 << 63) != 0 && state.efer & (1 << 11) == 0)
            || (large && (shift > 30 || (shift == 30 && !state.page_1g)))
            || (large && entry & (((1_u64 << shift) - 1) & !0x1fff) != 0)
        {
            return Err(state.page_fault(linear, write, 1 | (1 << 3)));
        }
        user &= entry & 4 != 0;
        writable &= entry & 2 != 0;
        let leaf = shift == 12 || large;
        if leaf {
            state.permissions(linear, write, user, writable, entry)?;
        }
        let update = (1 << 5) | if leaf && write { 1 << 6 } else { 0 };
        if entry & update != update {
            access_entry(slot, update).ok_or(DataFault::Backing(slot))?;
        }
        if leaf {
            let offset_mask = (1_u64 << shift) - 1;
            return Ok((entry & mask & !offset_mask) | (linear & offset_mask));
        }
        table = entry & mask;
    }
    Err(DataFault::InvalidState)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn state() -> DataAccess {
        DataAccess {
            cr0: 1 << 16,
            cr3: 0x1000,
            cr4: 0,
            efer: 1 << 11,
            rflags: 2,
            cpl: 0,
            physical_bits: 46,
            page_1g: true,
            pkru: 0,
            pkrs: 0,
        }
    }

    fn tables() -> BTreeMap<u64, u64> {
        [
            (0x1000, 0x2007),
            (0x2000, 0x3007),
            (0x3000, 0x4007),
            (0x4028, 0x8007),
            (0x4030, 0xa007),
        ]
        .into_iter()
        .collect()
    }

    fn data_walk(
        s: DataAccess,
        linear: u64,
        write: bool,
        table: &mut BTreeMap<u64, u64>,
    ) -> Result<u64, DataFault> {
        translate_data(linear, s, write, |address, bits| {
            let entry = table.get_mut(&address)?;
            *entry |= bits;
            Some(*entry)
        })
    }

    #[test]
    fn data_walk_tracks_accessed_dirty_and_discontiguous_boundary_pages() {
        let mut table = tables();
        assert_eq!(data_walk(state(), 0x5ffd, false, &mut table), Ok(0x8ffd));
        for address in [0x1000, 0x2000, 0x3000, 0x4028] {
            assert_ne!(table[&address] & (1 << 5), 0);
            assert_eq!(table[&address] & (1 << 6), 0);
        }
        assert_eq!(data_walk(state(), 0x6000, true, &mut table), Ok(0xa000));
        assert_eq!(table[&0x4030] & 0x60, 0x60);
        assert_eq!(table[&0x4028] & 0x60, 0x20);
    }

    #[test]
    fn data_faults_distinguish_nonpresent_reserved_and_backing() {
        for cpl in [0, 3] {
            for write in [false, true] {
                let mut s = state();
                s.cpl = cpl;
                let mut table = tables();
                table.insert(0x4028, u64::MAX & !1);
                assert_eq!(
                    data_walk(s, 0x5ffd, write, &mut table),
                    Err(s.page_fault(0x5ffd, write, 0))
                );
                table.insert(0x4028, (1 << s.physical_bits) | 7);
                assert_eq!(
                    data_walk(s, 0x5ffd, write, &mut table),
                    Err(s.page_fault(0x5ffd, write, 9))
                );
                table.insert(0x4028, (1 << 63) | 0x8007);
                s.efer = 0;
                assert_eq!(
                    data_walk(s, 0x5ffd, write, &mut table),
                    Err(s.page_fault(0x5ffd, write, 9))
                );
            }
        }
        assert_eq!(
            translate_data(0, state(), false, |_, _| None),
            Err(DataFault::Backing(0x1000))
        );
    }

    #[test]
    fn data_write_permissions_accumulate_and_honor_supervisor_wp() {
        for readonly in [0x1000, 0x2000, 0x3000, 0x4028] {
            let mut table = tables();
            *table.get_mut(&readonly).unwrap() &= !2;
            assert_eq!(
                data_walk(state(), 0x5000, true, &mut table),
                Err(DataFault::Page {
                    linear: 0x5000,
                    error: 3
                })
            );
            assert_eq!(table[&0x4028] & (1 << 6), 0);
            let mut s = state();
            s.cr0 = 0;
            assert_eq!(data_walk(s, 0x5000, true, &mut table), Ok(0x8000));
            s.cpl = 3;
            assert_eq!(
                data_walk(s, 0x5000, true, &mut table),
                Err(DataFault::Page {
                    linear: 0x5000,
                    error: 7
                })
            );
        }
        let mut table = tables();
        *table.get_mut(&0x2000).unwrap() &= !4;
        let mut s = state();
        s.cpl = 3;
        assert_eq!(
            data_walk(s, 0x5000, false, &mut table),
            Err(DataFault::Page {
                linear: 0x5000,
                error: 5
            })
        );
    }

    #[test]
    fn data_smap_and_protection_keys_preserve_fault_codes() {
        let mut table = tables();
        let mut s = state();
        s.cr4 = 1 << 21;
        assert_eq!(
            data_walk(s, 0x5000, false, &mut table),
            Err(s.page_fault(0x5000, false, 1))
        );
        s.rflags |= 1 << 18;
        assert_eq!(data_walk(s, 0x5000, false, &mut table), Ok(0x8000));
        s.cr4 |= 1 << 22;
        table.insert(0x4028, (3 << 59) | 0x8007);
        s.pkru = 1 << 6;
        assert_eq!(
            data_walk(s, 0x5000, false, &mut table),
            Err(s.page_fault(0x5000, false, 33))
        );
        s.pkru = 2 << 6;
        assert_eq!(
            data_walk(s, 0x5000, true, &mut table),
            Err(s.page_fault(0x5000, true, 33))
        );
        s.cr0 = 0;
        assert_eq!(data_walk(s, 0x5000, true, &mut table), Ok(0x8000));
        s.cpl = 3;
        assert_eq!(
            data_walk(s, 0x5000, true, &mut table),
            Err(DataFault::Page {
                linear: 0x5000,
                error: 39
            })
        );
        s.cpl = 0;
        s.cr4 = 1 << 24;
        s.pkrs = 1 << 6;
        *table.get_mut(&0x1000).unwrap() &= !4;
        assert_eq!(
            data_walk(s, 0x5000, false, &mut table),
            Err(s.page_fault(0x5000, false, 33))
        );
        s.pkrs = 2 << 6;
        assert_eq!(data_walk(s, 0x5000, true, &mut table), Ok(0x8000));
        s.cr0 = 1 << 16;
        assert_eq!(
            data_walk(s, 0x5000, true, &mut table),
            Err(s.page_fault(0x5000, true, 33))
        );
    }

    #[test]
    fn data_large_leaves_check_pat_alignment_and_cpu_support() {
        let mut table = tables();
        table.insert(0x3000, 0x20_0000 | 0x1087);
        assert_eq!(data_walk(state(), 0x5ffd, false, &mut table), Ok(0x20_5ffd));
        *table.get_mut(&0x3000).unwrap() |= 1 << 13;
        assert_eq!(
            data_walk(state(), 0x5ffd, false, &mut table),
            Err(state().page_fault(0x5ffd, false, 9))
        );
        table.insert(0x2000, 0x4000_0000 | 0x1087);
        assert_eq!(
            data_walk(state(), 0x5ffd, false, &mut table),
            Ok(0x4000_5ffd)
        );
        let mut s = state();
        s.page_1g = false;
        assert_eq!(
            data_walk(s, 0x5ffd, false, &mut table),
            Err(s.page_fault(0x5ffd, false, 9))
        );
        table.insert(0x1000, 0x2087);
        assert_eq!(
            data_walk(state(), 0x5ffd, false, &mut table),
            Err(s.page_fault(0x5ffd, false, 9))
        );
    }

    #[test]
    fn data_walk_checks_width_five_levels_and_overflow_without_access() {
        for bits in [0, 11, 53, 64, 255] {
            let mut s = state();
            s.physical_bits = bits;
            assert_eq!(
                translate_data(0, s, false, |_, _| panic!(
                    "invalid context accessed memory"
                )),
                Err(DataFault::InvalidState)
            );
        }
        for bits in [12, 32, 52] {
            let mut s = state();
            s.physical_bits = bits;
            s.cr3 = (1_u64 << bits) - 4096;
            assert_eq!(
                translate_data(4095, s, false, |_, _| Some(s.cr3 | 0x23)),
                Ok((1_u64 << bits) - 1)
            );
        }
        let mut s = state();
        s.cr4 = CR4_LA57;
        let mut table = [
            (0x1008, 0x2007),
            (0x2000, 0x3007),
            (0x3000, 0x4007),
            (0x4000, 0x5007),
            (0x5000, 0x6007),
        ]
        .into_iter()
        .collect();
        assert_eq!(data_walk(s, 1 << 48, false, &mut table), Ok(0x6000));
        assert_eq!(
            translate_data(1 << 48, state(), false, |_, _| panic!(
                "noncanonical access"
            )),
            Err(DataFault::Address)
        );
        assert_eq!(
            state().operand_range(0x7fff_ffff_fffc, 8),
            Err(DataFault::Address)
        );
        assert_eq!(
            state().operand_range(u64::MAX - 3, 8),
            Err(DataFault::Address)
        );
        assert_eq!(state().operand_range(0x5ffd, 8), Ok(0x5ffd));
    }

    #[test]
    fn data_lam_strips_only_metadata_and_preserves_address_half() {
        let mut s = state();
        let tagged = 0x6666_0000_1234_5678;
        assert_eq!(s.operand_range(tagged, 8), Err(DataFault::Address));
        s.cr3 |= 1 << 62;
        assert_eq!(s.operand_range(tagged, 8), Ok(0x1234_5678));
        assert_eq!(
            s.operand_range(0x6666_8000_1234_5678, 8),
            Err(DataFault::Address)
        );
        s.cr3 |= 1 << 61;
        s.cr4 = CR4_LA57;
        assert_eq!(s.operand_range(0x5c00_0000_1234_5678, 8), Ok(0x1234_5678));
        s.cr4 = 1 << 28;
        assert_eq!(
            s.operand_range(0x9999_ffff_1234_5678, 8),
            Ok(0xffff_ffff_1234_5678)
        );
        assert_eq!(
            s.operand_range(0x9999_0000_1234_5678, 8),
            Err(DataFault::Address)
        );
    }

    #[test]
    fn walks_linux_stack_and_five_level_huge_page() {
        let linear = 0xffff_8000_1234_5ffc_u64;
        let pml4 = 0x1000;
        let pdpt = 0x2000;
        let pd = 0x3000;
        let pt = 0x4000;
        let entry_address = |table: u64, shift: u8| table + ((linear >> shift) & 0x1ff) * 8;
        let read = |address| match address {
            value if value == entry_address(pml4, 39) => Some(pdpt | PRESENT),
            value if value == entry_address(pdpt, 30) => Some(pd | PRESENT),
            value if value == entry_address(pd, 21) => Some(pt | PRESENT),
            value if value == entry_address(pt, 12) => Some(0xb000 | PRESENT),
            value if value == entry_address(pt, 12) + 8 => Some(0xd000 | PRESENT),
            _ => None,
        };
        let mut context = state();
        context.cr3 = pml4 | 0xabc;
        assert_eq!(
            translate_data(linear, context, false, |address, bits| read(address)
                .map(|entry| entry | bits)),
            Ok(0xbffc)
        );
        assert_eq!(
            translate_data(linear + 7, context, false, |address, bits| read(address)
                .map(|entry| entry | bits)),
            Ok(0xd003)
        );

        let linear = 0x0001_2345_6789_abcd_u64;
        let pml5 = 0x10_000;
        let pml4 = 0x11_000;
        let pdpt = 0x12_000;
        let entry_address = |table: u64, shift: u8| table + ((linear >> shift) & 0x1ff) * 8;
        let read = |address| match address {
            value if value == entry_address(pml5, 48) => Some(pml4 | PRESENT),
            value if value == entry_address(pml4, 39) => Some(pdpt | PRESENT),
            value if value == entry_address(pdpt, 30) => Some(0x8000_0000 | PAGE_SIZE | PRESENT),
            _ => None,
        };
        context.cr3 = pml5;
        context.cr4 = CR4_LA57;
        assert_eq!(
            translate_data(linear, context, false, |address, bits| read(address)
                .map(|entry| entry | bits)),
            Ok(0x8000_0000 | (linear & ((1 << 30) - 1)))
        );
        assert_eq!(
            translate_data(1 << 48, state(), false, |_, _| None),
            Err(DataFault::Address)
        );
    }
}
