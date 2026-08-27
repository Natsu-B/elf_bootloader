//! Minimal long-mode page-table walking for trusted nested guests.

/// CR4.LA57 selects five-level linear-address translation.
const CR4_LA57: u64 = 1 << 12;
/// Present bit in a paging-structure entry.
const PRESENT: u64 = 1;
/// Page-size bit in a PDPTE or PDE.
const PAGE_SIZE: u64 = 1 << 7;
/// Architectural address field for CPUs with at most 52 physical bits.
const MAX_ADDRESS_FIELD: u64 = 0x000f_ffff_ffff_f000;

/// Translates one canonical long-mode linear address.
///
/// `read_entry` reads a little-endian 64-bit paging entry at a physical
/// address. The caller retains responsibility for validating that the
/// physical address names readable guest RAM.
#[must_use]
pub fn translate_long_mode(
    linear: u64,
    cr3: u64,
    cr4: u64,
    max_physical_bits: u8,
    mut read_entry: impl FnMut(u64) -> Option<u64>,
) -> Option<u64> {
    if !(12..=52).contains(&max_physical_bits) {
        return None;
    }

    let five_level = cr4 & CR4_LA57 != 0;
    let virtual_bits = if five_level { 57 } else { 48 };
    let sign = 1_u64 << (virtual_bits - 1);
    let upper = !((1_u64 << virtual_bits) - 1);
    if (linear & sign == 0 && linear & upper != 0)
        || (linear & sign != 0 && linear & upper != upper)
    {
        return None;
    }

    let physical_mask = ((1_u64 << max_physical_bits) - 1) & !0xfff;
    if cr3 & MAX_ADDRESS_FIELD & !physical_mask != 0 {
        return None;
    }
    let mut table = cr3 & physical_mask;
    let shifts: &[u8] = if five_level {
        &[48, 39, 30, 21, 12]
    } else {
        &[39, 30, 21, 12]
    };

    for &shift in shifts {
        let slot = (linear >> shift) & 0x1ff;
        let entry = read_entry(table.checked_add(slot * 8)?)?;
        if entry & PRESENT == 0 || entry & MAX_ADDRESS_FIELD & !physical_mask != 0 {
            return None;
        }

        let address = entry & physical_mask;
        if shift == 12 {
            return Some(address | (linear & 0xfff));
        }
        if entry & PAGE_SIZE != 0 {
            if shift != 30 && shift != 21 {
                return None;
            }
            // Bit 12 is PAT for a large leaf; the remaining sub-page address
            // bits are reserved and must be zero.
            let reserved = ((1_u64 << shift) - 1) & !0x1fff;
            if address & reserved != 0 {
                return None;
            }
            return Some((address & !((1_u64 << shift) - 1)) | (linear & ((1_u64 << shift) - 1)));
        }
        table = address;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            translate_long_mode(linear, pml4 | 0xabc, 0, 46, read),
            Some(0xbffc)
        );
        assert_eq!(
            translate_long_mode(linear + 7, pml4 | 0xabc, 0, 46, read),
            Some(0xd003)
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
        assert_eq!(
            translate_long_mode(linear, pml5, CR4_LA57, 46, read),
            Some(0x8000_0000 | (linear & ((1 << 30) - 1)))
        );
        assert_eq!(translate_long_mode(1 << 48, pml4, 0, 46, |_| None), None);
    }
}
