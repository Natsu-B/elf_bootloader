//! Platform-derived identity EPT materialization and explicit QEMU smoke maps.

use crate::addr::EptPhys;
use crate::addr::HostPhys;
use crate::platform_memory;
use crate::platform_memory::Mapping;
use crate::platform_memory::Mappings;
use crate::platform_memory::MemoryType;
use crate::platform_memory::PageCapabilities;
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
    /// Four-level host identity mapping cannot represent a noncanonical address.
    HostAddress,
    /// The captured host PAT is invalid or cannot select normal WB RAM.
    HostPat,
}

/// Host leaf encoding under the captured PAT and independent CPU page-size
/// capabilities. Selecting PAT WB lets physical MTRRs supply the native memory
/// type; the planner still splits large leaves at every memory-type boundary.
#[derive(Clone, Copy)]
pub struct HostPagingPolicy {
    pat: u64,
    wb_index: u8,
    uc_index: u8,
    capabilities: PageCapabilities,
}

impl HostPagingPolicy {
    /// Requires WB for RAM and strong UC for the scratch window, without WRMSR.
    pub fn new(pat: u64, capabilities: PageCapabilities) -> Result<Self, BuildError> {
        let mut wb = None;
        let mut uc = None;
        for (index, value) in pat.to_le_bytes().into_iter().enumerate() {
            if !matches!(value, 0 | 1 | 4 | 5 | 6 | 7) {
                return Err(BuildError::HostPat);
            }
            if value == 6 && wb.is_none() {
                wb = Some(index as u8);
            }
            if value == 0 && uc.is_none() {
                uc = Some(index as u8);
            }
        }
        Ok(Self {
            pat,
            wb_index: wb.ok_or(BuildError::HostPat)?,
            uc_index: uc.ok_or(BuildError::HostPat)?,
            capabilities,
        })
    }
}

/// Completely built host tables; this does not load CR3 or change PAT. The
/// caller must keep the physical backing resident and load `pat()` on VM exit.
/// A/D bits are preset so page walks need not mutate the borrowed table entries.
pub struct HostTables<'a> {
    pages: &'a [EptPage],
    physical: HostPhys,
    policy: HostPagingPolicy,
    leaves: u64,
    width: platform_memory::PhysicalWidth,
}

impl HostTables<'_> {
    /// Private four-level root, available only after complete construction.
    #[must_use]
    pub const fn cr3(&self) -> u64 {
        self.physical.get()
    }
    /// Exact PAT value used to encode leaves; not the guest's dynamic PAT.
    #[must_use]
    pub const fn pat(&self) -> u64 {
        self.policy.pat
    }
    /// Initialized paging-structure pages, excluding unused arena capacity.
    #[must_use]
    pub const fn table_pages(&self) -> usize {
        self.pages.len()
    }
    /// Hardware leaf count, not the number of firmware regions.
    #[must_use]
    pub const fn leaf_count(&self) -> u64 {
        self.leaves
    }
    /// An initially non-present, CPU-owned UC scratch leaf. Before modifying
    /// its PTE, end all Rust table borrows; retain backing and serialize access
    /// on the owning CPU, with INVLPG on every installation and removal.
    #[must_use]
    pub fn window(&self) -> HostWindow {
        HostWindow {
            pte: self.physical.get() + (self.pages.len() as u64 - 1) * EPT_PAGE_BYTES as u64,
            uc_index: self.policy.uc_index,
            width: self.width,
        }
    }
}

/// Validated metadata for one host scratch PTE, not permission to access a
/// physical device. The monitor must validate the selected backing separately.
#[derive(Clone, Copy)]
pub struct HostWindow {
    pte: u64,
    uc_index: u8,
    width: platform_memory::PhysicalWidth,
}

impl HostWindow {
    /// PML4[511], outside every low-canonical RAM identity mapping.
    pub const VIRTUAL_BASE: u64 = 0xffff_ff80_0000_0000;
    /// Physical address of the writable PTE inside the retained private arena.
    #[must_use]
    pub const fn pte_address(self) -> u64 {
        self.pte
    }
    /// Encodes a supervisor 4 KiB strong-UC leaf with preset accessed/dirty bits.
    /// Clearing this PTE requires writing zero and invalidating VIRTUAL_BASE.
    pub fn entry(self, physical: HostPhys) -> Result<u64, BuildError> {
        if physical.get() >= self.width.limit() {
            return Err(BuildError::Storage);
        }
        let index = u64::from(self.uc_index);
        Ok(physical.get() | 3 | (1 << 5) | (1 << 6) | ((index & 3) << 3) | ((index >> 2) << 7))
    }
}

#[derive(Clone, Copy)]
enum Format {
    Ept,
    Host(HostPagingPolicy),
}

impl Format {
    fn mappings<'p, 'd>(self, plan: &'p PlatformMap<'d>) -> Mappings<'p, 'd> {
        match self {
            Self::Ept => plan.mappings(),
            Self::Host(policy) => plan.host_mappings(policy.capabilities),
        }
    }
    fn table_flags(self) -> u64 {
        match self {
            Self::Ept => 7,
            Self::Host(_) => 3 | (1 << 5),
        }
    }
    fn leaf_flags(self, mapping: Mapping) -> u64 {
        let large = mapping.page_size != PageSize::Base4K;
        let flags = match self {
            Self::Ept => 7 | ((mapping.memory_type as u64) << 3),
            Self::Host(policy) => {
                let index = u64::from(policy.wb_index);
                let writable = mapping.attributes & 0x20000 == 0;
                1 | (u64::from(writable) << 1)
                    | (1 << 5)
                    | (1 << 6)
                    | ((index & 3) << 3)
                    | ((index >> 2) << if large { 12 } else { 7 })
            }
        };
        flags | if large { 1 << 7 } else { 0 }
    }
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
    platform_layout(plan, Format::Ept).map(|(pages, _)| pages)
}

/// Sizes the private host map without copying the L1 MMIO apertures into it.
pub fn required_host_pages(
    plan: &PlatformMap<'_>,
    policy: HostPagingPolicy,
) -> Result<usize, BuildError> {
    platform_layout(plan, Format::Host(policy)).map(|(pages, _)| pages)
}

fn platform_layout(plan: &PlatformMap<'_>, format: Format) -> Result<(usize, u64), BuildError> {
    let mut pages = 1_usize;
    let mut leaves = 0_u64;
    let mut last = [None; 3];
    for mapping in format.mappings(plan) {
        let mapping = mapping.map_err(BuildError::Platform)?;
        if matches!(format, Format::Host(_)) && mapping.range.end() > 1 << 47 {
            return Err(BuildError::HostAddress);
        }
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
        if matches!(format, Format::Host(_)) {
            pages = pages.checked_add(3).ok_or(BuildError::Overflow)?;
        }
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
    let (required, leaves) = build_identity(plan, pages, physical.get(), Format::Ept)?;
    Ok(PlatformEpt {
        pages: &pages[..required],
        physical,
        leaves,
    })
}

/// Builds only identity-addressable RAM, including L0 reservations, in a private
/// writable WB arena. No guest PCI aperture is mapped. Four-level canonical
/// bounds, PAT, page sizes and all cache/layout errors precede publication.
pub fn build_host_identity<'a>(
    plan: &PlatformMap<'_>,
    pages: &'a mut [EptPage],
    physical: HostPhys,
    policy: HostPagingPolicy,
) -> Result<HostTables<'a>, BuildError> {
    let (required, leaves) = build_identity(plan, pages, physical.get(), Format::Host(policy))?;
    Ok(HostTables {
        pages: &pages[..required],
        physical,
        policy,
        leaves,
        width: plan.physical_width(),
    })
}

fn host_arena_is_wb(
    plan: &PlatformMap<'_>,
    storage: PhysicalRange,
    policy: HostPagingPolicy,
) -> Result<bool, BuildError> {
    let mut cursor = storage.start();
    for mapping in plan.host_mappings(policy.capabilities) {
        let mapping = mapping.map_err(BuildError::Platform)?;
        if mapping.range.end() <= cursor {
            continue;
        }
        if mapping.range.start() > cursor
            || mapping.memory_type != MemoryType::WriteBack
            || mapping.attributes & 0x20000 != 0
        {
            return Ok(false);
        }
        cursor = storage.end().min(mapping.range.end());
        if cursor == storage.end() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn build_identity(
    plan: &PlatformMap<'_>,
    pages: &mut [EptPage],
    physical: u64,
    format: Format,
) -> Result<(usize, u64), BuildError> {
    if let Some(root) = pages.first_mut() {
        root.entries.fill(0);
    }
    let (required, leaves) = platform_layout(plan, format)?;
    if pages.len() < required {
        return Err(BuildError::Capacity);
    }
    let bytes = u64::try_from(pages.len())
        .ok()
        .and_then(|count| count.checked_mul(EPT_PAGE_BYTES as u64))
        .ok_or(BuildError::Overflow)?;
    let end = physical.checked_add(bytes).ok_or(BuildError::Overflow)?;
    let storage = PhysicalRange::new(physical, end, plan.physical_width())
        .map_err(|_| BuildError::Storage)?;
    if !plan.owns_private_range(storage) {
        return Err(BuildError::Storage);
    }
    if let Format::Host(policy) = format {
        if !host_arena_is_wb(plan, storage, policy)? {
            return Err(BuildError::Storage);
        }
    }
    for page in &mut pages[..required] {
        page.entries.fill(0);
    }
    let ram_pages = required
        - if matches!(format, Format::Host(_)) {
            3
        } else {
            0
        };
    let result = fill_platform_tables(plan, &mut pages[..ram_pages], physical, format);
    if let Err(error) = result {
        pages[0].entries.fill(0);
        return Err(error);
    }
    if matches!(format, Format::Host(_)) {
        // The low-canonical host check guarantees PML4[511] is unused. These
        // three already zeroed private pages terminate at a non-present PTE.
        let flags = format.table_flags();
        pages[0].entries[511] = physical + ram_pages as u64 * EPT_PAGE_BYTES as u64 | flags;
        pages[ram_pages].entries[0] =
            physical + (ram_pages as u64 + 1) * EPT_PAGE_BYTES as u64 | flags;
        pages[ram_pages + 1].entries[0] =
            physical + (ram_pages as u64 + 2) * EPT_PAGE_BYTES as u64 | flags;
    }
    Ok((required, leaves))
}

/// Populates only an already sized arena; links are offsets into that same arena.
fn fill_platform_tables(
    plan: &PlatformMap<'_>,
    pages: &mut [EptPage],
    physical: u64,
    format: Format,
) -> Result<(), BuildError> {
    const ADDRESS: u64 = 0x000f_ffff_ffff_f000;
    let mut used = 1;
    let table_flags = format.table_flags();
    for mapping in format.mappings(plan) {
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
                    pages[table].entries[slot] = address | format.leaf_flags(mapping);
                    break;
                }
                table = if entry == 0 {
                    if used >= pages.len() {
                        return Err(BuildError::Capacity);
                    }
                    pages[table].entries[slot] =
                        physical + used as u64 * EPT_PAGE_BYTES as u64 | table_flags;
                    used += 1;
                    used - 1
                } else {
                    if entry & !ADDRESS != table_flags {
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
    use super::HostPagingPolicy;
    use super::HostTables;
    use super::PlatformEpt;
    use super::build_host_identity;
    use super::build_identity_1g;
    use super::build_identity_8g;
    use super::build_platform_identity;
    use super::required_host_pages;
    use super::required_platform_pages;
    use crate::addr::EptPhys;
    use crate::addr::HostPhys;
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

    fn host_leaf(host: &HostTables<'_>, address: u64) -> Option<(u64, u64)> {
        let mut table = 0;
        for shift in [39, 30, 21, 12] {
            let entry = host.pages[table].entries[((address >> shift) & 511) as usize];
            if entry == 0 {
                return None;
            }
            assert_eq!(entry & 5, 1, "present supervisor mapping");
            assert_ne!(entry & (1 << 5), 0, "preset accessed");
            if shift == 12 || (shift != 39 && entry & (1 << 7) != 0) {
                assert_ne!(entry & (1 << 6), 0, "preset dirty");
                return Some((entry, 1 << shift));
            }
            assert_eq!(entry & !ADDRESS, 3 | (1 << 5));
            table = ((entry & ADDRESS) - host.physical.get()) as usize / PAGE as usize;
            assert!(table > 0 && table < host.table_pages());
        }
        panic!("unterminated host walk");
    }

    fn host_policy(pat: u64, features: u32, extended: u32) -> HostPagingPolicy {
        HostPagingPolicy::new(pat, PageCapabilities::from_host_cpuid(features, extended)).unwrap()
    }

    #[test]
    fn host_maps_private_runtime_and_high_ram_but_not_guest_pci_apertures() {
        let mut runtime = ram(32 * GIB, 32 * GIB + 2 * MIB);
        runtime.memory_type = 6;
        runtime.attributes |= 1 << 63;
        let mut acpi = ram(33 * GIB, 33 * GIB + PAGE);
        acpi.memory_type = 10;
        acpi.attributes |= 0x20000;
        let descriptors = [ram(0, GIB), ram(ARENA, ARENA + 32 * PAGE), runtime, acpi];
        let private = [range(ARENA, ARENA + 32 * PAGE)];
        let mmio = [range(56 << 40, 64 << 40)];
        let plan = plan(&descriptors, &private, &mmio, ALL_CAPS);
        let policy = host_policy(6, 1 << 3, 1 << 26);
        let mut pages = dirty_pages(32);
        let host =
            build_host_identity(&plan, &mut pages, HostPhys::new(ARENA).unwrap(), policy).unwrap();
        assert_eq!(host.cr3(), ARENA);
        assert_eq!(host.pat(), 6);
        assert_eq!(
            host.table_pages(),
            required_host_pages(&plan, policy).unwrap()
        );
        assert!(host.table_pages() < required_platform_pages(&plan).unwrap());
        assert!(host.leaf_count() > 0);
        assert_eq!(host_leaf(&host, 0).unwrap().1, GIB);
        assert!(host_leaf(&host, ARENA).is_some());
        assert_eq!(host_leaf(&host, 32 * GIB).unwrap().1, 2 * MIB);
        assert_eq!(
            host_leaf(&host, 33 * GIB).unwrap().0 & 2,
            0,
            "UEFI RO preserved"
        );
        assert_eq!(host_leaf(&host, 56 << 40), None);
        assert_eq!(host_leaf(&host, (64 << 40) - PAGE), None);
        assert_eq!(host_leaf(&host, 2 * GIB), None);
        let window = host.window();
        assert_eq!(host_leaf(&host, super::HostWindow::VIRTUAL_BASE), None);
        assert_eq!(
            host_leaf(&host, super::HostWindow::VIRTUAL_BASE + PAGE),
            None
        );
        assert_eq!(
            window.pte_address(),
            ARENA + (host.table_pages() as u64 - 1) * PAGE
        );
        let entry = window.entry(HostPhys::new(56 << 40).unwrap()).unwrap();
        assert_eq!(entry & ADDRESS, 56 << 40);
        assert_eq!(entry & !ADDRESS, 3 | (1 << 5) | (1 << 6) | (1 << 3)); // PAT[1]=UC
        assert!(window.entry(HostPhys::new(1 << 52).unwrap()).is_err());
        let mut guest_pages = dirty_pages(32);
        let guest =
            build_platform_identity(&plan, &mut guest_pages, EptPhys::new(ARENA).unwrap()).unwrap();
        assert_eq!(leaf(&guest, ARENA), None);
        assert!(leaf(&guest, 56 << 40).is_some());
    }

    #[test]
    fn host_page_sizes_use_cpuid_and_pat_bits_follow_each_leaf_format() {
        let descriptors = [ram(0, GIB), ram(ARENA, ARENA + 4 * MIB)];
        let private = [range(ARENA, ARENA + 4 * MIB)];
        let plan = plan(&descriptors, &private, &[], BASE_CAPS);
        for (features, extended, bytes) in [(0, 0, PAGE), (1 << 3, 0, 2 * MIB), (0, 1 << 26, GIB)] {
            let policy = host_policy(6 << 40, features, extended); // WB is PAT[5]
            let required = required_host_pages(&plan, policy).unwrap();
            let mut pages = dirty_pages(required);
            let host =
                build_host_identity(&plan, &mut pages, HostPhys::new(ARENA).unwrap(), policy)
                    .unwrap();
            let (entry, size) = host_leaf(&host, 0).unwrap();
            assert_eq!(size, bytes);
            let pat_bit = if bytes == PAGE { 7 } else { 12 };
            assert_ne!(entry & (1 << pat_bit), 0);
            assert_eq!(entry & ((1 << 3) | (1 << 4)), 1 << 3);
            assert_eq!(entry & ADDRESS & !(bytes - 1), 0);
        }
    }

    #[test]
    fn host_pat_and_canonical_limits_are_checked_without_publishing_a_root() {
        let capabilities = PageCapabilities::from_host_cpuid(1 << 3, 1 << 26);
        assert!(matches!(
            HostPagingPolicy::new(0, capabilities),
            Err(BuildError::HostPat)
        ));
        assert!(matches!(
            HostPagingPolicy::new(0x0606_0606_0606_0606, capabilities),
            Err(BuildError::HostPat)
        ));
        for value in [2, 3, 8, 0xff] {
            assert!(matches!(
                HostPagingPolicy::new(6 | (value << 8), capabilities),
                Err(BuildError::HostPat)
            ));
        }
        let policy = host_policy(0x0007_0406_0007_0406, 1 << 3, 1 << 26);
        let private = [range(ARENA, ARENA + 16 * PAGE)];
        let descriptors = [
            ram(ARENA, ARENA + 16 * PAGE),
            ram((1 << 47) - PAGE, (1 << 47) + PAGE),
        ];
        let invalid = plan(&descriptors, &private, &[], ALL_CAPS);
        let mut pages = dirty_pages(16);
        assert!(matches!(
            build_host_identity(&invalid, &mut pages, HostPhys::new(ARENA).unwrap(), policy),
            Err(BuildError::HostAddress)
        ));
        assert_root_empty(&pages);
        let valid_descriptors = [descriptors[0], ram((1 << 47) - PAGE, 1 << 47)];
        let valid = plan(&valid_descriptors, &private, &[], ALL_CAPS);
        let host =
            build_host_identity(&valid, &mut pages, HostPhys::new(ARENA).unwrap(), policy).unwrap();
        assert_eq!(host_leaf(&host, (1 << 47) - 1).unwrap().1, PAGE);
    }

    #[test]
    fn host_arena_requires_complete_private_writable_wb_ram() {
        let private = [range(ARENA, ARENA + 16 * PAGE)];
        let policy = host_policy(6, 1 << 3, 1 << 26);
        let ordinary = ram(0, PAGE);
        for attributes in [8 | 0x2000, 8 | 0x20000] {
            let mut arena = ram(ARENA, ARENA + 16 * PAGE);
            arena.attributes = attributes;
            let descriptors = [ordinary, arena];
            let plan = plan(&descriptors, &private, &[], ALL_CAPS);
            let mut pages = dirty_pages(16);
            assert!(matches!(
                build_host_identity(&plan, &mut pages, HostPhys::new(ARENA).unwrap(), policy),
                Err(BuildError::Storage)
            ));
            assert_root_empty(&pages);
        }
        let descriptors = [ordinary];
        let absent = plan(&descriptors, &private, &[], ALL_CAPS);
        let mut pages = dirty_pages(16);
        assert!(matches!(
            build_host_identity(&absent, &mut pages, HostPhys::new(ARENA).unwrap(), policy),
            Err(BuildError::Storage)
        ));
        assert_root_empty(&pages);
        let mut arena = ram(ARENA, ARENA + 16 * PAGE);
        arena.attributes = 9;
        let mut low = ordinary;
        low.attributes = 9;
        let descriptors = [low, arena];
        let uc = PlatformMap::new(
            &descriptors,
            &private,
            &[],
            Mtrrs::new(width(), 0, 1 << 11, None, &[]).unwrap(),
            PageCapabilities::from_vmx_capability(ALL_CAPS).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            build_host_identity(&uc, &mut pages, HostPhys::new(ARENA).unwrap(), policy),
            Err(BuildError::Storage)
        ));
        assert_root_empty(&pages);
        let unowned = plan(&descriptors, &[], &[], ALL_CAPS);
        assert!(matches!(
            build_host_identity(&unowned, &mut pages, HostPhys::new(ARENA).unwrap(), policy),
            Err(BuildError::Storage)
        ));
        assert_root_empty(&pages);
    }

    #[test]
    fn host_capacity_and_late_cache_failure_clear_only_the_root_before_fill() {
        let private = [range(ARENA, ARENA + 16 * PAGE)];
        let descriptors = [ram(0, GIB), ram(ARENA, ARENA + 16 * PAGE)];
        let good = plan(&descriptors, &private, &[], ALL_CAPS);
        let policy = host_policy(6, 1 << 3, 1 << 26);
        let required = required_host_pages(&good, policy).unwrap();
        let mut small = dirty_pages(required - 1);
        assert!(matches!(
            build_host_identity(&good, &mut small, HostPhys::new(ARENA).unwrap(), policy),
            Err(BuildError::Capacity)
        ));
        assert_root_empty(&small);
        let mut late_region = ram(2 * ARENA, 2 * ARENA + PAGE);
        late_region.attributes = 1; // UC-only capability conflicts with WB MTRR
        let bad_descriptors = [descriptors[0], descriptors[1], late_region];
        let bad = plan(&bad_descriptors, &private, &[], ALL_CAPS);
        let mut pages = dirty_pages(16);
        assert!(matches!(
            build_host_identity(&bad, &mut pages, HostPhys::new(ARENA).unwrap(), policy),
            Err(BuildError::Platform(Error::CacheConflict))
        ));
        assert_root_empty(&pages);
        assert!(pages[1].entries().iter().all(|&entry| entry == u64::MAX));
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
