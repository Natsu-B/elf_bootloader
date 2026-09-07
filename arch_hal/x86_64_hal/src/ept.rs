//! Platform-derived identity EPT materialization and explicit QEMU smoke maps.

use crate::addr::EptPhys;
use crate::platform_memory;
use crate::platform_memory::PageSize;
use crate::platform_memory::PhysicalRange;
use crate::platform_memory::PlatformMap;

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

/// A rejected platform map or caller-owned paging-structure arena.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildError {
    /// Complete map validation failed; even a valid prefix must not be used.
    Platform(platform_memory::Error),
    /// There are no usable leaf mappings.
    Empty,
    /// Table/leaf counting or physical arena arithmetic overflowed.
    Overflow,
    /// The supplied contiguous arena is too small.
    Capacity,
    /// Arena addresses exceed the captured physical width or lack L0 ownership.
    Storage,
    /// A table link or leaf conflicts with the validated, ordered plan.
    Conflict,
}

/// A completely constructed EPT, borrowing its storage until the caller retires it.
///
/// Before using `eptp`, the caller must establish that the physical arena really
/// backs this slice, remains resident, and is accessible with compatible cache
/// attributes. No VMCS may reference the arena while it is being rebuilt. This
/// builder preserves guest PAT participation (EPT ignore-PAT is clear); handling
/// later L1 MTRR changes and TLB invalidation is the monitor's responsibility.
pub struct PlatformEpt<'a> {
    pages: &'a [EptPage],
    physical: EptPhys,
    leaves: u64,
}

impl PlatformEpt<'_> {
    /// Returns a four-level WB EPTP only after the entire build succeeded.
    #[must_use]
    pub const fn eptp(&self) -> u64 {
        self.physical.get() | 6 | (3 << 3)
    }

    /// Returns the number of initialized paging-structure pages.
    #[must_use]
    pub const fn table_pages(&self) -> usize {
        self.pages.len()
    }

    /// Returns the number of hardware leaf entries, not the number of segments.
    #[must_use]
    pub const fn leaf_count(&self) -> u64 {
        self.leaves
    }
}

/// Counts all required tables without iterating every 4 KiB leaf of a large map.
///
/// Exhausts the fallible planner before returning. Adjacent segments sharing a
/// table count that table once, even when their attributes differ.
pub fn required_platform_pages(plan: &PlatformMap<'_>) -> Result<usize, BuildError> {
    platform_layout(plan).map(|(pages, _)| pages)
}

fn platform_layout(plan: &PlatformMap<'_>) -> Result<(usize, u64), BuildError> {
    let mut pages = 1_usize;
    let mut leaves = 0_u64;
    let mut last = [None; 3];
    for mapping in plan.mappings() {
        let mapping = mapping.map_err(BuildError::Platform)?;
        leaves = leaves
            .checked_add(mapping.range.bytes() / mapping.page_size.bytes())
            .ok_or(BuildError::Overflow)?;
        for (level, shift) in [39, 30, 21].into_iter().enumerate() {
            if (shift == 30 && mapping.page_size == PageSize::Huge1G)
                || (shift == 21 && mapping.page_size != PageSize::Base4K)
            {
                continue;
            }
            let first = mapping.range.start() >> shift;
            let end = (mapping.range.end() - 1) >> shift;
            let count = end - first + u64::from(last[level] != Some(first));
            pages = pages
                .checked_add(usize::try_from(count).map_err(|_| BuildError::Overflow)?)
                .ok_or(BuildError::Overflow)?;
            last[level] = Some(end);
        }
    }
    if leaves == 0 {
        Err(BuildError::Empty)
    } else {
        Ok((pages, leaves))
    }
}

/// Builds a platform identity map in a contiguous, explicitly private arena.
///
/// All platform errors, capacity and physical-address checks precede leaf writes.
/// Any error clears the root, and never returns an EPTP. Excess arena pages must
/// also be L0-owned. The supplied physical address is not dereferenced here.
pub fn build_platform_identity<'a>(
    plan: &PlatformMap<'_>,
    pages: &'a mut [EptPage],
    physical: EptPhys,
) -> Result<PlatformEpt<'a>, BuildError> {
    if let Some(root) = pages.first_mut() {
        root.entries.fill(0);
    }
    let (required, leaves) = platform_layout(plan)?;
    if pages.len() < required {
        return Err(BuildError::Capacity);
    }
    let bytes = u64::try_from(pages.len())
        .ok()
        .and_then(|count| count.checked_mul(EPT_PAGE_BYTES as u64))
        .ok_or(BuildError::Overflow)?;
    let end = physical
        .get()
        .checked_add(bytes)
        .ok_or(BuildError::Overflow)?;
    let storage = PhysicalRange::new(physical.get(), end, plan.physical_width())
        .map_err(|_| BuildError::Storage)?;
    if !plan.owns_private_range(storage) {
        return Err(BuildError::Storage);
    }
    for page in &mut pages[..required] {
        page.entries.fill(0);
    }
    let result = fill_platform_tables(plan, &mut pages[..required], physical.get());
    if let Err(error) = result {
        pages[0].entries.fill(0);
        return Err(error);
    }
    Ok(PlatformEpt {
        pages: &pages[..required],
        physical,
        leaves,
    })
}

/// Populates only an already sized arena; links are offsets into that same arena.
fn fill_platform_tables(
    plan: &PlatformMap<'_>,
    pages: &mut [EptPage],
    physical: u64,
) -> Result<(), BuildError> {
    const RWX: u64 = 7;
    const LARGE: u64 = 1 << 7;
    const ADDRESS: u64 = 0x000f_ffff_ffff_f000;
    let mut used = 1;
    for mapping in plan.mappings() {
        let mapping = mapping.map_err(BuildError::Platform)?;
        let leaf_shift = match mapping.page_size {
            PageSize::Huge1G => 30,
            PageSize::Large2M => 21,
            PageSize::Base4K => 12,
        };
        let mut address = mapping.range.start();
        while address < mapping.range.end() {
            let mut table = 0;
            for shift in [39, 30, 21, 12] {
                let slot = ((address >> shift) & 511) as usize;
                let entry = pages[table].entries[slot];
                if shift == leaf_shift {
                    if entry != 0 {
                        return Err(BuildError::Conflict);
                    }
                    pages[table].entries[slot] = address
                        | RWX
                        | ((mapping.memory_type as u64) << 3)
                        | if shift == 12 { 0 } else { LARGE };
                    break;
                }
                table = if entry == 0 {
                    if used >= pages.len() {
                        return Err(BuildError::Capacity);
                    }
                    pages[table].entries[slot] =
                        physical + used as u64 * EPT_PAGE_BYTES as u64 | RWX;
                    used += 1;
                    used - 1
                } else {
                    if entry & !ADDRESS != RWX {
                        return Err(BuildError::Conflict);
                    }
                    let offset = (entry & ADDRESS)
                        .checked_sub(physical)
                        .ok_or(BuildError::Conflict)?;
                    let index = usize::try_from(offset / EPT_PAGE_BYTES as u64)
                        .map_err(|_| BuildError::Conflict)?;
                    if index == 0 || index >= used {
                        return Err(BuildError::Conflict);
                    }
                    index
                };
            }
            address += mapping.page_size.bytes();
        }
    }
    if used != pages.len() {
        return Err(BuildError::Conflict);
    }
    Ok(())
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

/// Builds an 8 GiB identity map for QEMU q35 with 4 GiB of RAM.
///
/// Coarse RAM buckets at `[0, 2 GiB)` and `[4 GiB, 6 GiB)` are WB. The low
/// bucket retains q35's small legacy holes; guest page attributes handle them.
/// The PCI hole and remaining ranges are UC. Returns the EPT pointer to write
/// into the VMCS.
pub fn build_identity_8g(
    pml4: &mut EptPage,
    pml4_phys: EptPhys,
    pdpt: &mut EptPage,
    pdpt_phys: EptPhys,
    page_directories: &mut [EptPage; 8],
    page_directory_phys: [EptPhys; 8],
) -> u64 {
    const READ_WRITE_EXECUTE: u64 = 0b111;
    const WRITE_BACK: u64 = 6 << 3;
    const LARGE_PAGE: u64 = 1 << 7;
    const TWO_MIB: u64 = 2 * 1024 * 1024;

    pml4.entries.fill(0);
    pdpt.entries.fill(0);
    pml4.entries[0] = pdpt_phys.get() | READ_WRITE_EXECUTE;
    for (directory_index, (directory, physical)) in page_directories
        .iter_mut()
        .zip(page_directory_phys)
        .enumerate()
    {
        directory.entries.fill(0);
        pdpt.entries[directory_index] = physical.get() | READ_WRITE_EXECUTE;
        let memory_type = if directory_index < 2 || (4..6).contains(&directory_index) {
            WRITE_BACK
        } else {
            0
        };
        for (entry_index, entry) in directory.entries.iter_mut().enumerate() {
            let leaf_index = directory_index * 512 + entry_index;
            *entry = leaf_index as u64 * TWO_MIB | READ_WRITE_EXECUTE | memory_type | LARGE_PAGE;
        }
    }

    // ponytail: this fixed split covers QEMU q35 with 4 GiB RAM; derive WB/UC
    // ranges from the UEFI memory map before using it on arbitrary bare metal.
    pml4_phys.get() | 6 | (3 << 3)
}

#[cfg(test)]
mod tests {
    use super::BuildError;
    use super::EptPage;
    use super::PlatformEpt;
    use super::build_identity_1g;
    use super::build_identity_8g;
    use super::build_platform_identity;
    use super::required_platform_pages;
    use crate::addr::EptPhys;
    use crate::platform_memory::Error;
    use crate::platform_memory::FirmwareDescriptor;
    use crate::platform_memory::MemoryType;
    use crate::platform_memory::Mtrrs;
    use crate::platform_memory::PageCapabilities;
    use crate::platform_memory::PhysicalRange;
    use crate::platform_memory::PhysicalWidth;
    use crate::platform_memory::PlatformMap;
    use crate::platform_memory::RegionKind;

    const PAGE: u64 = 4096;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    const ARENA: u64 = 1 << 40;
    const BASE_CAPS: u64 = (1 << 6) | (1 << 14);
    const ALL_CAPS: u64 = BASE_CAPS | (1 << 16) | (1 << 17);
    const ADDRESS: u64 = 0x000f_ffff_ffff_f000;

    fn width() -> PhysicalWidth {
        PhysicalWidth::new(52).unwrap()
    }

    fn range(start: u64, end: u64) -> PhysicalRange {
        PhysicalRange::new(start, end, width()).unwrap()
    }

    fn ram(start: u64, end: u64) -> FirmwareDescriptor {
        FirmwareDescriptor {
            memory_type: 7,
            physical_start: start,
            number_of_pages: (end - start) / PAGE,
            attributes: 8,
        }
    }

    fn plan<'a>(
        descriptors: &'a [FirmwareDescriptor],
        private: &'a [PhysicalRange],
        mmio: &'a [PhysicalRange],
        capability: u64,
    ) -> PlatformMap<'a> {
        PlatformMap::new(
            descriptors,
            private,
            mmio,
            Mtrrs::new(width(), 0, (1 << 11) | 6, None, &[]).unwrap(),
            PageCapabilities::from_vmx_capability(capability).unwrap(),
        )
        .unwrap()
    }

    /// Walks only the supplied host-test arena, never an actual physical pointer.
    fn leaf(ept: &PlatformEpt<'_>, address: u64) -> Option<(u64, u64)> {
        let mut table = 0;
        for shift in [39, 30, 21, 12] {
            let slot = ((address >> shift) & 511) as usize;
            let entry = ept.pages[table].entries()[slot];
            if entry == 0 {
                return None;
            }
            assert_eq!(entry & 7, 7);
            if shift == 12 || (shift != 39 && entry & (1 << 7) != 0) {
                return Some((entry, 1 << shift));
            }
            assert_eq!(entry & !ADDRESS, 7, "invalid non-leaf flags");
            let offset = (entry & ADDRESS).checked_sub(ept.physical.get()).unwrap();
            assert_eq!(offset % PAGE, 0);
            table = usize::try_from(offset / PAGE).unwrap();
            assert!(table > 0 && table < ept.table_pages());
        }
        panic!("EPT walk did not terminate at a leaf");
    }

    fn assert_identity_leaf(ept: &PlatformEpt<'_>, address: u64, bytes: u64, kind: MemoryType) {
        let (entry, actual_bytes) = leaf(ept, address).unwrap();
        assert_eq!(actual_bytes, bytes);
        assert_eq!(entry & ADDRESS, address & !(bytes - 1));
        let flags = 7 | ((kind as u64) << 3) | if bytes == PAGE { 0 } else { 1 << 7 };
        // Includes ignore-PAT, accessed/dirty and all other unsupported flags.
        assert_eq!(entry & !ADDRESS, flags);
    }

    fn dirty_pages(count: usize) -> Vec<EptPage> {
        let mut pages = vec![EptPage::new(); count];
        for page in &mut pages {
            page.entries.fill(u64::MAX);
        }
        pages
    }

    fn assert_root_empty(pages: &[EptPage]) {
        assert!(pages[0].entries().iter().all(|entry| *entry == 0));
    }

    #[test]
    fn platform_leaf_sizes_follow_capabilities_and_report_hardware_counts() {
        for (end, caps, tables, leaves, bytes) in [
            (GIB, ALL_CAPS, 2, 1, GIB),
            (GIB, BASE_CAPS | (1 << 16), 3, 512, 2 * MIB),
            (2 * MIB, BASE_CAPS, 4, 512, PAGE),
        ] {
            let descriptors = [ram(0, end)];
            let private = [range(ARENA, ARENA + tables as u64 * PAGE)];
            let plan = plan(&descriptors, &private, &[], caps);
            assert_eq!(required_platform_pages(&plan), Ok(tables));
            let mut pages = dirty_pages(tables);
            let ept =
                build_platform_identity(&plan, &mut pages, EptPhys::new(ARENA).unwrap()).unwrap();
            assert_eq!(ept.eptp(), ARENA | 0x1e);
            assert_eq!(ept.table_pages(), tables);
            assert_eq!(ept.leaf_count(), leaves);
            assert_identity_leaf(&ept, 0, bytes, MemoryType::WriteBack);
            assert_identity_leaf(&ept, end - 1, bytes, MemoryType::WriteBack);
            assert_eq!(leaf(&ept, end), None);
        }
    }

    #[test]
    fn platform_huge_only_capability_keeps_a_base_page_tail() {
        let descriptors = [ram(0, GIB + PAGE)];
        let private = [range(ARENA, ARENA + 4 * PAGE)];
        let plan = plan(&descriptors, &private, &[], BASE_CAPS | (1 << 17));
        assert_eq!(required_platform_pages(&plan), Ok(4));
        let mut pages = dirty_pages(4);
        let ept = build_platform_identity(&plan, &mut pages, EptPhys::new(ARENA).unwrap()).unwrap();
        assert_eq!(ept.leaf_count(), 2);
        assert_identity_leaf(&ept, GIB - 1, GIB, MemoryType::WriteBack);
        assert_identity_leaf(&ept, GIB, PAGE, MemoryType::WriteBack);
        assert_eq!(leaf(&ept, GIB + PAGE), None);
    }

    #[test]
    fn platform_unsorted_runtime_acpi_and_mmio_segments_share_tables() {
        let descriptors = [
            FirmwareDescriptor {
                memory_type: 6,
                attributes: (1 << 63) | 8,
                ..ram(2 * MIB, 4 * MIB)
            },
            FirmwareDescriptor {
                memory_type: 9,
                ..ram(6 * MIB, 8 * MIB)
            },
            FirmwareDescriptor {
                memory_type: 11,
                attributes: 1,
                ..ram(4 * MIB, 6 * MIB)
            },
            ram(0, 2 * MIB),
        ];
        let private = [range(ARENA, ARENA + 3 * PAGE)];
        let mmio = [range(8 * MIB, 10 * MIB)];
        let plan = plan(&descriptors, &private, &mmio, ALL_CAPS);
        let runtime = plan
            .mappings()
            .map(Result::unwrap)
            .find(|entry| entry.kind == RegionKind::Runtime)
            .unwrap();
        assert_eq!(runtime.attributes, (1 << 63) | 8);
        assert_eq!(required_platform_pages(&plan), Ok(3));
        let mut pages = dirty_pages(3);
        let ept = build_platform_identity(&plan, &mut pages, EptPhys::new(ARENA).unwrap()).unwrap();
        assert_eq!(ept.leaf_count(), 5);
        for (address, kind) in [
            (0, MemoryType::WriteBack),
            (2 * MIB, MemoryType::WriteBack),
            (4 * MIB, MemoryType::Uncacheable),
            (6 * MIB, MemoryType::WriteBack),
            (8 * MIB, MemoryType::Uncacheable),
        ] {
            assert_identity_leaf(&ept, address, 2 * MIB, kind);
        }
        assert_eq!(leaf(&ept, 10 * MIB), None);
    }

    #[test]
    fn platform_encodes_every_supported_mtrr_type_without_ignoring_guest_pat() {
        for kind in [
            MemoryType::Uncacheable,
            MemoryType::WriteCombining,
            MemoryType::WriteThrough,
            MemoryType::WriteProtected,
            MemoryType::WriteBack,
        ] {
            let descriptors = [FirmwareDescriptor {
                attributes: 0x100f,
                ..ram(0, 2 * MIB)
            }];
            let private = [range(ARENA, ARENA + 3 * PAGE)];
            let plan = PlatformMap::new(
                &descriptors,
                &private,
                &[],
                Mtrrs::new(width(), 1 << 10, (1 << 11) | kind as u64, None, &[]).unwrap(),
                PageCapabilities::from_vmx_capability(ALL_CAPS).unwrap(),
            )
            .unwrap();
            let mut pages = dirty_pages(3);
            let ept =
                build_platform_identity(&plan, &mut pages, EptPhys::new(ARENA).unwrap()).unwrap();
            assert_identity_leaf(&ept, 0, 2 * MIB, kind);
        }
    }

    #[test]
    fn platform_base_page_segments_roll_over_all_nonleaf_boundaries() {
        for (boundary, required) in [(2 * MIB, 5), (GIB, 6), (512 * GIB, 7)] {
            let descriptors = [ram(boundary - PAGE, boundary + PAGE)];
            let private = [range(ARENA, ARENA + required as u64 * PAGE)];
            let plan = plan(&descriptors, &private, &[], ALL_CAPS);
            assert_eq!(required_platform_pages(&plan), Ok(required));
            let mut pages = dirty_pages(required);
            let ept =
                build_platform_identity(&plan, &mut pages, EptPhys::new(ARENA).unwrap()).unwrap();
            assert_eq!(ept.leaf_count(), 2);
            assert_identity_leaf(&ept, boundary - PAGE, PAGE, MemoryType::WriteBack);
            assert_identity_leaf(&ept, boundary, PAGE, MemoryType::WriteBack);
            assert_eq!(leaf(&ept, boundary - 2 * PAGE), None);
            assert_eq!(leaf(&ept, boundary + PAGE), None);
        }
    }

    #[test]
    fn platform_private_hole_splits_huge_mapping_and_is_never_a_leaf() {
        let descriptors = [ram(0, GIB)];
        let private = [range(ARENA, ARENA + 4 * PAGE), range(3 * PAGE, 4 * PAGE)];
        let plan = plan(&descriptors, &private, &[], ALL_CAPS);
        assert_eq!(required_platform_pages(&plan), Ok(4));
        let mut pages = dirty_pages(4);
        let ept = build_platform_identity(&plan, &mut pages, EptPhys::new(ARENA).unwrap()).unwrap();
        assert_eq!(ept.leaf_count(), 1022);
        assert_identity_leaf(&ept, 2 * PAGE, PAGE, MemoryType::WriteBack);
        assert_eq!(leaf(&ept, 3 * PAGE), None);
        assert_identity_leaf(&ept, 4 * PAGE, PAGE, MemoryType::WriteBack);
        assert_identity_leaf(&ept, 2 * MIB, 2 * MIB, MemoryType::WriteBack);
        assert_eq!(leaf(&ept, ARENA), None);
    }

    #[test]
    fn platform_checks_the_entire_arena_and_accepts_adjacent_private_owners() {
        let descriptors = [ram(0, GIB)];
        let adjacent = [
            range(ARENA + PAGE, ARENA + 3 * PAGE),
            range(ARENA, ARENA + PAGE),
        ];
        let plan = plan(&descriptors, &adjacent, &[], ALL_CAPS);
        let mut pages = dirty_pages(3);
        {
            let ept =
                build_platform_identity(&plan, &mut pages, EptPhys::new(ARENA).unwrap()).unwrap();
            assert_eq!(ept.table_pages(), 2);
            assert_identity_leaf(&ept, 0, GIB, MemoryType::WriteBack);
        }
        assert!(pages[2].entries().iter().all(|entry| *entry == u64::MAX));
        for private in [
            vec![range(ARENA, ARENA + 2 * PAGE)],
            vec![
                range(ARENA, ARENA + PAGE),
                range(ARENA + 2 * PAGE, ARENA + 3 * PAGE),
            ],
        ] {
            let plan = self::plan(&descriptors, &private, &[], ALL_CAPS);
            let mut pages = dirty_pages(3);
            assert!(matches!(
                build_platform_identity(&plan, &mut pages, EptPhys::new(ARENA).unwrap()),
                Err(BuildError::Storage)
            ));
            assert_root_empty(&pages);
        }
        let plan = self::plan(&descriptors, &[], &[], ALL_CAPS);
        let mut pages = dirty_pages(2);
        assert!(matches!(
            build_platform_identity(&plan, &mut pages, EptPhys::new(PAGE).unwrap()),
            Err(BuildError::Storage)
        ));
        assert_root_empty(&pages);
    }

    #[test]
    fn platform_capacity_empty_and_late_cache_errors_clear_a_dirty_root() {
        let descriptors = [ram(0, PAGE)];
        let private = [range(ARENA, ARENA + 4 * PAGE)];
        let plan = plan(&descriptors, &private, &[], ALL_CAPS);
        assert_eq!(required_platform_pages(&plan), Ok(4));
        let mut pages = dirty_pages(3);
        assert!(matches!(
            build_platform_identity(&plan, &mut pages, EptPhys::new(ARENA).unwrap()),
            Err(BuildError::Capacity)
        ));
        assert_root_empty(&pages);
        assert!(matches!(
            build_platform_identity(&plan, &mut [], EptPhys::new(ARENA).unwrap()),
            Err(BuildError::Capacity)
        ));

        let excluded = [range(0, PAGE), range(ARENA, ARENA + 4 * PAGE)];
        let empty = self::plan(&descriptors, &excluded, &[], ALL_CAPS);
        assert_eq!(required_platform_pages(&empty), Err(BuildError::Empty));
        let mut pages = dirty_pages(4);
        assert!(matches!(
            build_platform_identity(&empty, &mut pages, EptPhys::new(ARENA).unwrap()),
            Err(BuildError::Empty)
        ));
        assert_root_empty(&pages);

        let descriptors = [
            ram(0, 2 * MIB),
            FirmwareDescriptor {
                attributes: 1,
                ..ram(2 * MIB, 4 * MIB)
            },
        ];
        let late = self::plan(&descriptors, &private, &[], ALL_CAPS);
        let mut stream = late.mappings();
        assert!(stream.next().unwrap().is_ok());
        assert_eq!(stream.next(), Some(Err(Error::CacheConflict)));
        assert_eq!(
            required_platform_pages(&late),
            Err(BuildError::Platform(Error::CacheConflict))
        );
        let mut pages = dirty_pages(4);
        assert!(matches!(
            build_platform_identity(&late, &mut pages, EptPhys::new(ARENA).unwrap()),
            Err(BuildError::Platform(Error::CacheConflict))
        ));
        assert_root_empty(&pages);
        assert!(
            pages[1].entries().iter().all(|entry| *entry == u64::MAX),
            "sizing failure must precede non-root writes"
        );
    }

    #[test]
    fn platform_separates_gpa_walk_width_from_arena_physical_width() {
        let physical = 1 << 49;
        let descriptors = [ram((1 << 48) - PAGE, 1 << 48)];
        let private = [range(physical, physical + 4 * PAGE)];
        let plan = plan(&descriptors, &private, &[], ALL_CAPS);
        let mut pages = dirty_pages(4);
        let ept =
            build_platform_identity(&plan, &mut pages, EptPhys::new(physical).unwrap()).unwrap();
        assert_eq!(ept.eptp(), physical | 0x1e);
        assert_identity_leaf(&ept, (1 << 48) - 1, PAGE, MemoryType::WriteBack);
        assert_eq!(leaf(&ept, (1 << 48) - 2 * PAGE), None);

        let narrow = PhysicalWidth::new(36).unwrap();
        let physical = narrow.limit() - 4 * PAGE;
        let descriptors = [ram(0, PAGE)];
        let private = [range(physical, narrow.limit())];
        let plan = PlatformMap::new(
            &descriptors,
            &private,
            &[],
            Mtrrs::new(narrow, 0, (1 << 11) | 6, None, &[]).unwrap(),
            PageCapabilities::from_vmx_capability(ALL_CAPS).unwrap(),
        )
        .unwrap();
        let mut pages = dirty_pages(4);
        let ept =
            build_platform_identity(&plan, &mut pages, EptPhys::new(physical).unwrap()).unwrap();
        assert_eq!(ept.eptp(), physical | 0x1e);
        assert_identity_leaf(&ept, 0, PAGE, MemoryType::WriteBack);
    }

    #[test]
    fn platform_rejects_arena_width_and_endpoint_overflow() {
        let descriptors = [ram(0, GIB)];
        let private = [range(ARENA, ARENA + 2 * PAGE)];
        let plan = plan(&descriptors, &private, &[], ALL_CAPS);
        assert!(EptPhys::new(ARENA + 1).is_none());
        for (physical, expected) in [
            (width().limit(), BuildError::Storage),
            (u64::MAX & !(PAGE - 1), BuildError::Overflow),
        ] {
            let mut pages = dirty_pages(2);
            let error = build_platform_identity(&plan, &mut pages, EptPhys::new(physical).unwrap())
                .err()
                .unwrap();
            assert_eq!(error, expected);
            assert_root_empty(&pages);
        }
    }

    #[test]
    fn platform_large_base_page_map_is_sized_before_capacity_failure() {
        let descriptors = [ram(0, 1 << 48)];
        let physical = 1 << 49;
        let private = [range(physical, physical + 4 * PAGE)];
        let plan = plan(&descriptors, &private, &[], BASE_CAPS);
        assert_eq!(
            required_platform_pages(&plan),
            Ok(1 + 512 + 262_144 + 134_217_728)
        );
        let mut pages = dirty_pages(4);
        assert!(matches!(
            build_platform_identity(&plan, &mut pages, EptPhys::new(physical).unwrap()),
            Err(BuildError::Capacity)
        ));
        assert_root_empty(&pages);
        assert!(pages[1].entries().iter().all(|entry| *entry == u64::MAX));
    }

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

    #[test]
    fn q35_4g_map_types_low_and_high_ram_as_write_back() {
        let mut pml4 = EptPage::new();
        let mut pdpt = EptPage::new();
        let mut page_directories = core::array::from_fn(|_| EptPage::new());
        let eptp = build_identity_8g(
            &mut pml4,
            EptPhys::new(0x1000).unwrap(),
            &mut pdpt,
            EptPhys::new(0x2000).unwrap(),
            &mut page_directories,
            [
                EptPhys::new(0x3000).unwrap(),
                EptPhys::new(0x4000).unwrap(),
                EptPhys::new(0x5000).unwrap(),
                EptPhys::new(0x6000).unwrap(),
                EptPhys::new(0x7000).unwrap(),
                EptPhys::new(0x8000).unwrap(),
                EptPhys::new(0x9000).unwrap(),
                EptPhys::new(0xa000).unwrap(),
            ],
        );

        assert_eq!(eptp, 0x101e);
        assert_eq!(pdpt.entries()[3], 0x6007);
        assert_eq!(pdpt.entries()[7], 0xa007);
        assert_eq!(page_directories[0].entries()[0], 0xb7);
        assert_eq!(page_directories[1].entries()[0], 0x4000_00b7);
        assert_eq!(page_directories[2].entries()[0], 0x8000_0087);
        assert_eq!(page_directories[3].entries()[503], 0xfee0_0087);
        assert_eq!(page_directories[4].entries()[0], 0x1_0000_00b7);
        assert_eq!(page_directories[5].entries()[511], 0x1_7fe0_00b7);
        assert_eq!(page_directories[6].entries()[0], 0x1_8000_0087);
        assert_eq!(page_directories[7].entries()[511], 0x1_ffe0_0087);
    }
}
