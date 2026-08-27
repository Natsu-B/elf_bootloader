//! Small identity EPT used by the first VMX launch check.

use crate::addr::EptPhys;

/// Bytes in one EPT paging-structure page.
pub const EPT_PAGE_BYTES: usize = 4096;
/// EPT capability: 4-level page walks.
pub const EPT_CAP_PAGE_WALK_4: u64 = 1 << 6;
/// EPT capability: write-back EPTP memory type.
pub const EPT_CAP_MEMORY_TYPE_WB: u64 = 1 << 14;
/// EPT capability: 2 MiB leaf pages.
pub const EPT_CAP_PDE_2MB: u64 = 1 << 16;
/// EPT capability: INVEPT instruction.
pub const EPT_CAP_INVEPT: u64 = 1 << 20;
/// EPT capability: global INVEPT.
pub const EPT_CAP_INVEPT_ALL_CONTEXTS: u64 = 1 << 26;

/// Required capability subset for the smoke identity map.
pub const REQUIRED_EPT_CAPS: u64 = EPT_CAP_PAGE_WALK_4 | EPT_CAP_MEMORY_TYPE_WB | EPT_CAP_PDE_2MB;

/// One page of EPT entries.
#[derive(Clone)]
#[repr(C, align(4096))]
pub struct EptPage {
    entries: [u64; 512],
}

impl EptPage {
    /// Creates an empty paging-structure page.
    #[must_use]
    pub const fn new() -> Self {
        Self { entries: [0; 512] }
    }

    /// Returns all raw entries for diagnostics.
    #[must_use]
    pub const fn entries(&self) -> &[u64; 512] {
        &self.entries
    }
}

impl Default for EptPage {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds a 1 GiB identity map from 2 MiB write-back leaves.
///
/// Returns the EPT pointer to write into the VMCS.
pub fn build_identity_1g(
    pml4: &mut EptPage,
    pml4_phys: EptPhys,
    pdpt: &mut EptPage,
    pdpt_phys: EptPhys,
    page_directory: &mut EptPage,
    page_directory_phys: EptPhys,
) -> u64 {
    const READ_WRITE_EXECUTE: u64 = 0b111;
    const WRITE_BACK: u64 = 6 << 3;
    const LARGE_PAGE: u64 = 1 << 7;
    const TWO_MIB: u64 = 2 * 1024 * 1024;

    pml4.entries.fill(0);
    pdpt.entries.fill(0);
    page_directory.entries.fill(0);
    pml4.entries[0] = pdpt_phys.get() | READ_WRITE_EXECUTE;
    pdpt.entries[0] = page_directory_phys.get() | READ_WRITE_EXECUTE;
    for (index, entry) in page_directory.entries.iter_mut().enumerate() {
        *entry = index as u64 * TWO_MIB | READ_WRITE_EXECUTE | WRITE_BACK | LARGE_PAGE;
    }

    // ponytail: 1 GiB WB is only the VMX smoke ceiling; use the UEFI memory map
    // to assign RAM/MMIO types before launching firmware or an OS.
    pml4_phys.get() | 6 | (3 << 3)
}

#[cfg(test)]
mod tests {
    use super::EptPage;
    use super::build_identity_1g;
    use crate::addr::EptPhys;

    #[test]
    fn identity_map_links_three_levels_and_covers_one_gibibyte() {
        let mut pml4 = EptPage::new();
        let mut pdpt = EptPage::new();
        let mut page_directory = EptPage::new();
        let eptp = build_identity_1g(
            &mut pml4,
            EptPhys::new(0x1000).unwrap(),
            &mut pdpt,
            EptPhys::new(0x2000).unwrap(),
            &mut page_directory,
            EptPhys::new(0x3000).unwrap(),
        );

        assert_eq!(eptp, 0x101e);
        assert_eq!(pml4.entries()[0], 0x2007);
        assert_eq!(pdpt.entries()[0], 0x3007);
        assert_eq!(page_directory.entries()[0], 0xb7);
        assert_eq!(page_directory.entries()[511], 0x3fe0_00b7);
    }
}
