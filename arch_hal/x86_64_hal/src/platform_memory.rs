//! Heap-free platform-derived identity-EPT planning, without publishing an EPTP.
//!
//! Sources: Intel SDM volume 3, "MTRR Precedences" and "EPT Paging Structures";
//! UEFI 2.10A section 7.2.3 (`GetMemoryMap`). UEFI cache attributes describe
//! capabilities, not necessarily current settings. MTRRs determine the planned
//! memory type; non-UC RAM types must also be supported by the firmware region.
//! Guest PAT interaction and subsequent MTRR changes remain the materializer's
//! responsibility. This module does not modify page tables or CPU/firmware state.
//!
//! Every fallible mapping must be consumed and materialized successfully before
//! an EPTP is published or a guest enters VMX. A partial stream is not a valid EPT.

/// Architectural base page size.
const PAGE: u64 = 4096;
/// One MiB, the end of the architectural fixed-MTRR coverage.
const MIB: u64 = 1024 * 1024;
/// Bounded descriptor and override counts keep all validation heap-free.
const MAX_DESCRIPTORS: usize = 4096;
const MAX_OVERRIDES: usize = 128;
const MAX_VARIABLE_MTRRS: usize = 64;
/// UEFI cache-capability bits, including WP's cacheability meaning since UEFI 2.5.
const CACHE_UC: u64 = 1;
const CACHE_WC: u64 = 2;
const CACHE_WT: u64 = 4;
const CACHE_WB: u64 = 8;
const CACHE_WP: u64 = 0x1000;
/// Runtime placement is metadata, not permission to map monitor-private pages.
const MEMORY_RUNTIME: u64 = 1 << 63;
/// This x86 planner does not interpret optional ISA-specific cache encodings.
const MEMORY_ISA: u64 = (1 << 62) | 0x0fff_f000_0000_0000;
/// Standard UEFI 2.10 attributes this planner preserves without reinterpretation.
const KNOWN_ATTRIBUTES: u64 = MEMORY_RUNTIME | 0x000f_f01f;

/// An unsupported or malformed platform input; no fallback map is produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// CPUID physical width is outside the supported architectural range.
    PhysicalWidth,
    /// A range is empty, unaligned, overflowing, or above its address limit.
    PhysicalRange,
    /// UEFI descriptor version, stride, count, or byte length is invalid.
    DescriptorLayout,
    /// The caller's descriptor conversion buffer is too small.
    OutputCapacity,
    /// A firmware memory type is not defined by the supported UEFI format.
    FirmwareMemoryType,
    /// Firmware descriptors or same-purpose explicit overrides overlap.
    Overlap,
    /// Input exceeds a documented bounded-planning limit.
    InputCapacity,
    /// A selected cache type or ISA-specific attribute is unsupported.
    MemoryType,
    /// MTRR capture is incomplete or contains an invalid enabled register.
    MtrrState,
    /// Active variable MTRRs specify an architecturally undefined combination.
    MtrrConflict,
    /// A selected non-UC RAM cache type lacks the firmware capability bit.
    CacheConflict,
    /// Runtime Services code/data is missing the required runtime attribute.
    RuntimeAttribute,
    /// Explicit MMIO would change ordinary, runtime, ACPI, or unusable memory.
    MmioConflict,
    /// Required four-level, write-back EPT paging-structure support is missing.
    EptCapability,
    /// An actual mapping input does not fit the supported four-level GPA space.
    EptAddressWidth,
}

/// Checked CPUID physical-address width; wider CPUs remain usable below 48-bit GPA.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalWidth(u8);

impl PhysicalWidth {
    /// Accepts the x86-64 physical widths supported by this planner.
    pub fn new(bits: u8) -> Result<Self, Error> {
        if (32..=52).contains(&bits) {
            Ok(Self(bits))
        } else {
            Err(Error::PhysicalWidth)
        }
    }

    /// Returns the original CPUID width.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Returns the exclusive physical-address limit.
    #[must_use]
    pub const fn limit(self) -> u64 {
        1_u64 << self.0
    }
}

/// Nonempty, base-page-aligned physical interval with a checked exclusive end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalRange {
    start: u64,
    end: u64,
}

impl PhysicalRange {
    /// Checks endpoints without rounding away partial-page ownership.
    pub fn new(start: u64, end: u64, width: PhysicalWidth) -> Result<Self, Error> {
        if start >= end || start % PAGE != 0 || end % PAGE != 0 || end > width.limit() {
            Err(Error::PhysicalRange)
        } else {
            Ok(Self { start, end })
        }
    }

    /// Returns the first byte address.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Returns the exclusive end address.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }

    /// Returns the nonzero interval length in bytes.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.end - self.start
    }

    /// Tests membership without forming a pointer.
    const fn contains(self, address: u64) -> bool {
        self.start <= address && address < self.end
    }

    /// Tests overlap of half-open intervals.
    const fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// Neutral raw firmware descriptor; `PlatformMap::new` validates its semantics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FirmwareDescriptor {
    /// Numeric UEFI memory type, independent of a firmware-services crate.
    pub memory_type: u32,
    /// Physical address of the first 4 KiB page.
    pub physical_start: u64,
    /// Number of 4 KiB pages.
    pub number_of_pages: u64,
    /// Original UEFI capabilities and placement metadata.
    pub attributes: u64,
}

impl FirmwareDescriptor {
    /// Validates arithmetic before representing the descriptor as a range.
    fn range(self, width: PhysicalWidth) -> Result<PhysicalRange, Error> {
        let end = self
            .number_of_pages
            .checked_mul(PAGE)
            .and_then(|size| self.physical_start.checked_add(size))
            .ok_or(Error::PhysicalRange)?;
        PhysicalRange::new(self.physical_start, end, width)
    }

    /// Keeps ownership distinctions needed by a later EPT materializer.
    fn kind(self) -> Option<RegionKind> {
        match self.memory_type {
            1..=7 | 14 if self.attributes & MEMORY_RUNTIME != 0 => Some(RegionKind::Runtime),
            5 | 6 => Some(RegionKind::Runtime),
            1..=4 | 7 | 14 => Some(RegionKind::Ram),
            9 | 10 => Some(RegionKind::Acpi),
            11 => Some(RegionKind::Mmio),
            _ => None,
        }
    }
}

/// Owned, checked firmware descriptors for bounded physical RAM access.
/// This validates metadata only: callers must separately establish their CPU
/// mapping, memory types, ownership exclusions and absence of concurrent writes.
pub struct FirmwareMap<const N: usize> {
    descriptors: [FirmwareDescriptor; N],
    count: usize,
    width: PhysicalWidth,
}

impl<const N: usize> FirmwareMap<N> {
    /// Rejects malformed/overlapping descriptors without sorting or allocating.
    pub fn new(descriptors: &[FirmwareDescriptor], width: PhysicalWidth) -> Result<Self, Error> {
        if descriptors.len() > N {
            return Err(Error::OutputCapacity);
        }
        validate_firmware_descriptors(descriptors, width)?;
        let mut map = Self {
            descriptors: [FirmwareDescriptor::default(); N],
            count: descriptors.len(),
            width,
        };
        map.descriptors[..map.count].copy_from_slice(descriptors);
        Ok(map)
    }

    /// Original validated descriptor order, excluding unused capacity.
    #[must_use]
    pub fn descriptors(&self) -> &[FirmwareDescriptor] {
        &self.descriptors[..self.count]
    }

    /// Physical width captured with this immutable firmware snapshot.
    #[must_use]
    pub const fn physical_width(&self) -> PhysicalWidth {
        self.width
    }

    /// Tests complete RAM coverage, including adjacent descriptors. Holes,
    /// MMIO, unusable/unaccepted memory and read protection deny access. Writes
    /// additionally reject UEFI read-only memory. UEFI's WP *cache capability*
    /// is not confused with the RO permission bit. Physical address zero is
    /// legal when firmware actually describes accessible RAM there.
    #[must_use]
    pub fn allows_ram_access(&self, address: u64, bytes: u64, write: bool) -> bool {
        let Some(end) = address
            .checked_add(bytes)
            .filter(|&end| bytes != 0 && end <= self.width.limit())
        else {
            return false;
        };
        let mut cursor = address;
        while cursor < end {
            let Some(descriptor) = self.descriptors().iter().find(|descriptor| {
                // Constructor checked both arithmetic and non-overlap.
                descriptor.physical_start <= cursor
                    && cursor < descriptor.physical_start + descriptor.number_of_pages * PAGE
            }) else {
                return false;
            };
            if !matches!(descriptor.memory_type, 1..=7 | 9 | 10 | 14)
                || descriptor.attributes & (0x2000 | if write { 0x20000 } else { 0 }) != 0
            {
                return false;
            }
            cursor = end.min(descriptor.physical_start + descriptor.number_of_pages * PAGE);
        }
        true
    }
}

/// Shared metadata checks; the EPT planner adds its own materialization limits.
fn validate_firmware_descriptors(
    descriptors: &[FirmwareDescriptor],
    width: PhysicalWidth,
) -> Result<(), Error> {
    if descriptors.is_empty() || descriptors.len() > MAX_DESCRIPTORS {
        return Err(Error::InputCapacity);
    }
    for (index, descriptor) in descriptors.iter().enumerate() {
        let range = descriptor.range(width)?;
        if descriptor.memory_type > 15 {
            return Err(Error::FirmwareMemoryType);
        }
        if descriptor.attributes & MEMORY_ISA != 0 || descriptor.attributes & !KNOWN_ATTRIBUTES != 0
        {
            return Err(Error::MemoryType);
        }
        let is_runtime = descriptor.attributes & MEMORY_RUNTIME != 0;
        if (matches!(descriptor.memory_type, 5 | 6) && !is_runtime)
            || (is_runtime && descriptor.kind().is_none())
        {
            return Err(Error::RuntimeAttribute);
        }
        // Bounded O(n²) validation preserves the original firmware order.
        for previous in &descriptors[..index] {
            if range.overlaps(previous.range(width)?) {
                return Err(Error::Overlap);
            }
        }
    }
    Ok(())
}

/// Decodes the standard UEFI descriptor prefix using the firmware-provided stride.
///
/// Version 1 and 40..=256-byte, 8-byte-aligned strides are accepted. No typed
/// pointer or alignment assumption is made about the source buffer. The caller
/// must validate the returned descriptors with `PlatformMap::new` before use.
/// On error, ignore any already-written output entries.
pub fn decode_uefi_map(
    bytes: &[u8],
    stride: usize,
    version: u32,
    output: &mut [FirmwareDescriptor],
) -> Result<usize, Error> {
    if version != 1
        || !(40..=256).contains(&stride)
        || stride % 8 != 0
        || bytes.is_empty()
        || bytes.len() % stride != 0
        || bytes.len() / stride > MAX_DESCRIPTORS
    {
        return Err(Error::DescriptorLayout);
    }
    let count = bytes.len() / stride;
    if output.len() < count {
        return Err(Error::OutputCapacity);
    }
    for (record, destination) in bytes.chunks_exact(stride).zip(output.iter_mut()) {
        // Each offset names the validated standard 40-byte prefix, not padding.
        let word = |offset| {
            let mut value = [0; 8];
            value.copy_from_slice(&record[offset..offset + 8]);
            u64::from_le_bytes(value)
        };
        let mut kind = [0; 4];
        kind.copy_from_slice(&record[..4]);
        let virtual_start = word(16);
        let pages = word(24);
        if virtual_start % PAGE != 0
            || pages == 0
            || pages
                .checked_mul(PAGE)
                .and_then(|length| virtual_start.checked_add(length))
                .is_none()
        {
            return Err(Error::DescriptorLayout);
        }
        *destination = FirmwareDescriptor {
            memory_type: u32::from_le_bytes(kind),
            physical_start: word(8),
            number_of_pages: pages,
            attributes: word(32),
        };
    }
    Ok(count)
}

/// Cache encodings shared by Intel MTRRs and EPT leaf entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MemoryType {
    /// Uncacheable.
    Uncacheable = 0,
    /// Write combining.
    WriteCombining = 1,
    /// Write through.
    WriteThrough = 4,
    /// Write-protected caching, not an EPT write-permission restriction.
    WriteProtected = 5,
    /// Write back.
    WriteBack = 6,
}

impl MemoryType {
    /// Rejects reserved encodings and WC when MTRRCAP does not advertise it.
    fn decode(value: u8, write_combining: bool) -> Result<Self, Error> {
        match value {
            0 => Ok(Self::Uncacheable),
            1 if write_combining => Ok(Self::WriteCombining),
            4 => Ok(Self::WriteThrough),
            5 => Ok(Self::WriteProtected),
            6 => Ok(Self::WriteBack),
            _ => Err(Error::MemoryType),
        }
    }

    /// Tests firmware cache support; MTRR-mandated UC remains safe for RAM.
    fn supported_by(self, attributes: u64) -> bool {
        let capability = match self {
            Self::Uncacheable => CACHE_UC,
            Self::WriteCombining => CACHE_WC,
            Self::WriteThrough => CACHE_WT,
            Self::WriteProtected => CACHE_WP,
            Self::WriteBack => CACHE_WB,
        };
        self == Self::Uncacheable || attributes & capability != 0
    }
}

/// Captured raw variable MTRR pair; inactive pairs are architecturally ignored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VariableMtrr {
    /// IA32_MTRR_PHYSBASEn.
    pub base: u64,
    /// IA32_MTRR_PHYSMASKn, including the valid bit.
    pub mask: u64,
}

/// Validated borrowed architectural MTRR snapshot.
#[derive(Clone, Copy)]
pub struct Mtrrs<'a> {
    width: PhysicalWidth,
    enabled: bool,
    default: MemoryType,
    write_combining: bool,
    fixed: Option<&'a [u64; 11]>,
    variable: &'a [VariableMtrr],
}

impl<'a> Mtrrs<'a> {
    /// Validates a capture made only after CPUID advertises MTRR support.
    ///
    /// Fixed registers use order 0x250, 0x258, 0x259, then 0x268..=0x26f.
    /// Disabled global/fixed/variable state does not contribute memory types.
    pub fn new(
        width: PhysicalWidth,
        capability: u64,
        default_type: u64,
        fixed: Option<&'a [u64; 11]>,
        variable: &'a [VariableMtrr],
    ) -> Result<Self, Error> {
        if variable.len() != (capability & 0xff) as usize || variable.len() > MAX_VARIABLE_MTRRS {
            return Err(Error::MtrrState);
        }
        let enabled = default_type & (1 << 11) != 0;
        let write_combining = capability & (1 << 10) != 0;
        let default = if enabled {
            if default_type & !0xcff != 0 {
                return Err(Error::MtrrState);
            }
            MemoryType::decode(default_type as u8, write_combining)?
        } else {
            MemoryType::Uncacheable
        };
        let fixed = if enabled && default_type & (1 << 10) != 0 {
            if capability & (1 << 8) == 0 {
                return Err(Error::MtrrState);
            }
            let fixed = fixed.ok_or(Error::MtrrState)?;
            for value in fixed {
                for kind in value.to_le_bytes() {
                    MemoryType::decode(kind, write_combining)?;
                }
            }
            Some(fixed)
        } else {
            None
        };
        let result = Self {
            width,
            enabled,
            default,
            write_combining,
            fixed,
            variable,
        };
        if enabled {
            // Checking every boundary handles UC covering otherwise conflicting
            // pairs, without incorrectly applying an order-dependent pair fold.
            for entry in variable {
                if let Some((range, _)) = result.variable_range(*entry)? {
                    result.memory_type(range.start)?;
                    if range.end < width.limit() {
                        result.memory_type(range.end)?;
                    }
                }
            }
            if result.fixed.is_some() {
                // Variable ranges may start below fixed coverage and conflict only
                // after fixed precedence ends, without any variable endpoint there.
                result.memory_type(MIB)?;
            }
        }
        Ok(result)
    }

    /// Resolves one physical address according to Intel's MTRR precedence rules.
    pub fn memory_type(self, address: u64) -> Result<MemoryType, Error> {
        if address >= self.width.limit() {
            return Err(Error::PhysicalRange);
        }
        if !self.enabled {
            return Ok(MemoryType::Uncacheable);
        }
        if let Some(fixed) = self.fixed {
            if address < MIB {
                let (register, byte, _) = fixed_position(address);
                return MemoryType::decode(
                    fixed[register].to_le_bytes()[byte],
                    self.write_combining,
                );
            }
        }
        let mut matches = 0_u8;
        for entry in self.variable {
            if let Some((range, kind)) = self.variable_range(*entry)? {
                if range.contains(address) {
                    matches |= 1 << kind as u8;
                }
            }
        }
        if matches == 0 {
            return Ok(self.default);
        }
        if matches & 1 != 0 {
            return Ok(MemoryType::Uncacheable);
        }
        if matches == ((1 << 4) | (1 << 6)) {
            return Ok(MemoryType::WriteThrough);
        }
        if matches.count_ones() == 1 {
            return MemoryType::decode(matches.trailing_zeros() as u8, self.write_combining);
        }
        Err(Error::MtrrConflict)
    }

    /// Decodes an enabled power-of-two, size-aligned variable range.
    fn variable_range(
        self,
        entry: VariableMtrr,
    ) -> Result<Option<(PhysicalRange, MemoryType)>, Error> {
        if !self.enabled || entry.mask & (1 << 11) == 0 {
            return Ok(None);
        }
        let address_mask = (self.width.limit() - 1) & !(PAGE - 1);
        if entry.base & !(address_mask | 0xff) != 0 || entry.mask & !(address_mask | (1 << 11)) != 0
        {
            return Err(Error::MtrrState);
        }
        let size = ((!entry.mask & address_mask) | (PAGE - 1)) + 1;
        let start = entry.base & address_mask;
        if !size.is_power_of_two() || start & (size - 1) != 0 {
            return Err(Error::MtrrState);
        }
        let end = start.checked_add(size).ok_or(Error::PhysicalRange)?;
        Ok(Some((
            PhysicalRange::new(start, end, self.width)?,
            MemoryType::decode(entry.base as u8, self.write_combining)?,
        )))
    }

    /// Finds the next architectural range boundary, including fixed granules.
    fn next_boundary(self, address: u64, mut end: u64) -> Result<u64, Error> {
        if !self.enabled {
            return Ok(end);
        }
        if self.fixed.is_some() && address < MIB {
            end = end.min(fixed_position(address).2);
        }
        for entry in self.variable {
            if let Some((range, _)) = self.variable_range(*entry)? {
                end = next_range_boundary(range, address, end);
            }
        }
        Ok(end)
    }
}

/// Returns fixed-MTRR register, byte, and exclusive granule end below one MiB.
fn fixed_position(address: u64) -> (usize, usize, u64) {
    let (register, base, size) = if address < 0x8_0000 {
        (0, 0, 0x1_0000)
    } else if address < 0xa_0000 {
        (1, 0x8_0000, 0x4000)
    } else if address < 0xc_0000 {
        (2, 0xa_0000, 0x4000)
    } else {
        let register = 3 + ((address - 0xc_0000) / 0x8000) as usize;
        (register, address & !0x7fff, PAGE)
    };
    let byte = ((address - base) / size) as usize;
    (register, byte, base + (byte as u64 + 1) * size)
}

/// Hardware-approved leaf sizes for a four-level EPT or host walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageCapabilities {
    two_mib: bool,
    one_gib: bool,
}

impl PageCapabilities {
    /// Captured CPUID.1:EDX and CPUID.80000001:EDX, not VMX capabilities.
    #[must_use]
    pub const fn from_host_cpuid(features: u32, extended_features: u32) -> Self {
        Self {
            two_mib: features & (1 << 3) != 0,
            one_gib: extended_features & (1 << 26) != 0,
        }
    }

    /// Reads allowed EPT page-size bits from captured IA32_VMX_EPT_VPID_CAP.
    pub fn from_vmx_capability(capability: u64) -> Result<Self, Error> {
        if capability & ((1 << 6) | (1 << 14)) != ((1 << 6) | (1 << 14)) {
            return Err(Error::EptCapability);
        }
        Ok(Self {
            two_mib: capability & (1 << 16) != 0,
            one_gib: capability & (1 << 17) != 0,
        })
    }
}

/// EPT leaf size selected only within compatible attributes and ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageSize {
    /// 4 KiB PTE leaf.
    Base4K,
    /// 2 MiB PDE leaf, if advertised.
    Large2M,
    /// 1 GiB PDPTE leaf, if advertised.
    Huge1G,
}

impl PageSize {
    /// Returns the hardware leaf length.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        match self {
            Self::Base4K => PAGE,
            Self::Large2M => 2 * MIB,
            Self::Huge1G => 1024 * MIB,
        }
    }
}

/// Original platform ownership class retained for the later materializer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegionKind {
    /// Usable ordinary, loader, Boot Services, or persistent RAM.
    Ram,
    /// Firmware Runtime Services RAM, still subject to explicit L0 exclusions.
    Runtime,
    /// ACPI reclaim or nonvolatile-sleep RAM; original descriptor type is retained.
    Acpi,
    /// Firmware or explicitly declared physical MMIO; always UC.
    Mmio,
}

/// Contiguous identity mapping expressed as one or more equally sized EPT leaves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mapping {
    /// Equal guest and host physical interval.
    pub range: PhysicalRange,
    /// EPT memory type derived from MTRRs or forced UC for MMIO.
    pub memory_type: MemoryType,
    /// Original firmware class; host mappings may include monitor reservations.
    pub kind: RegionKind,
    /// Original numeric UEFI memory type; explicit holes use MMIO type 11.
    pub firmware_type: u32,
    /// Original descriptor attributes; explicit holes have no firmware attributes.
    pub attributes: u64,
    /// Largest currently usable leaf size; divides both range endpoints.
    pub page_size: PageSize,
}

/// Bounded borrowed map plan; construction validates ownership and source layouts.
pub struct PlatformMap<'a> {
    descriptors: &'a [FirmwareDescriptor],
    private: &'a [PhysicalRange],
    mmio: &'a [PhysicalRange],
    mtrrs: Mtrrs<'a>,
    capabilities: PageCapabilities,
}

impl<'a> PlatformMap<'a> {
    /// Validates the platform snapshot without mapping anything.
    ///
    /// Explicit MMIO may cover holes, firmware-reserved memory, or existing MMIO,
    /// never ordinary/runtime/ACPI RAM or unusable/unaccepted memory. Private
    /// reservations always win, including over Runtime Services descriptors.
    pub fn new(
        descriptors: &'a [FirmwareDescriptor],
        private: &'a [PhysicalRange],
        mmio: &'a [PhysicalRange],
        mtrrs: Mtrrs<'a>,
        capabilities: PageCapabilities,
    ) -> Result<Self, Error> {
        if private.len() > MAX_OVERRIDES || mmio.len() > MAX_OVERRIDES {
            return Err(Error::InputCapacity);
        }
        validate_firmware_descriptors(descriptors, mtrrs.width)?;
        for descriptor in descriptors {
            let range = descriptor.range(mtrrs.width)?;
            if descriptor.kind().is_some() && range.end > (1 << 48) {
                return Err(Error::EptAddressWidth);
            }
        }
        for ranges in [private, mmio] {
            for (index, range) in ranges.iter().enumerate() {
                if range.end > mtrrs.width.limit() {
                    return Err(Error::EptAddressWidth);
                }
                if ranges[..index].iter().any(|other| range.overlaps(*other)) {
                    return Err(Error::Overlap);
                }
            }
        }
        for range in mmio {
            if range.end > (1 << 48) {
                return Err(Error::EptAddressWidth);
            }
            for descriptor in descriptors {
                if range.overlaps(descriptor.range(mtrrs.width)?)
                    && !matches!(descriptor.memory_type, 0 | 11)
                {
                    return Err(Error::MmioConflict);
                }
            }
        }
        Ok(Self {
            descriptors,
            private,
            mmio,
            mtrrs,
            capabilities,
        })
    }

    /// Streams bounded-size segments, stopping after the first error.
    ///
    /// Cache conflicts can be reported while iterating. Finish the entire stream
    /// before publishing an EPTP; do not enter a guest with a partial result.
    #[must_use]
    pub fn mappings(&self) -> Mappings<'_, 'a> {
        Mappings {
            plan: self,
            cursor: 0,
            failed: false,
            host: false,
            capabilities: self.capabilities,
        }
    }

    /// Host identity mappings include reserved monitor RAM but no PCI/MMIO
    /// aperture or read-protected RAM. L0 needs RAM for checked guest page walks
    /// and operands; rare MMIO accesses require a separately owned mapping window.
    /// CPU page-size support is independent of EPT page-size capabilities.
    #[must_use]
    pub fn host_mappings(&self, capabilities: PageCapabilities) -> Mappings<'_, 'a> {
        Mappings {
            plan: self,
            cursor: 0,
            failed: false,
            host: true,
            capabilities,
        }
    }

    /// Physical paging-structure addresses use MAXPHYADDR, not the GPA walk width.
    pub(crate) const fn physical_width(&self) -> PhysicalWidth {
        self.mtrrs.width
    }

    /// Requires explicit ownership of every byte, including adjacent reservations.
    pub(crate) fn owns_private_range(&self, range: PhysicalRange) -> bool {
        let mut cursor = range.start;
        while cursor < range.end {
            let Some(owner) = self.private.iter().find(|owner| owner.contains(cursor)) else {
                return false;
            };
            cursor = owner.end;
        }
        true
    }

    /// Returns the next mapped interval start, skipping absent and unowned ranges.
    fn next_start(&self, cursor: u64) -> Result<Option<u64>, Error> {
        let mut start = None;
        for descriptor in self.descriptors {
            if descriptor.kind().is_some() {
                let range = descriptor.range(self.mtrrs.width)?;
                if range.end > cursor {
                    let candidate = range.start.max(cursor);
                    start = Some(start.map_or(candidate, |old: u64| old.min(candidate)));
                }
            }
        }
        for range in self.mmio {
            if range.end > cursor {
                let candidate = range.start.max(cursor);
                start = Some(start.map_or(candidate, |old| old.min(candidate)));
            }
        }
        Ok(start)
    }

    /// Resolves source metadata at one known mapped byte.
    fn source(&self, address: u64) -> Result<(RegionKind, u32, u64), Error> {
        for descriptor in self.descriptors {
            if descriptor.range(self.mtrrs.width)?.contains(address) {
                if let Some(kind) = descriptor.kind() {
                    return Ok((kind, descriptor.memory_type, descriptor.attributes));
                }
                if self.mmio.iter().any(|range| range.contains(address)) {
                    return Ok((
                        RegionKind::Mmio,
                        descriptor.memory_type,
                        descriptor.attributes,
                    ));
                }
            }
        }
        if self.mmio.iter().any(|range| range.contains(address)) {
            Ok((RegionKind::Mmio, 11, 0))
        } else {
            Err(Error::PhysicalRange)
        }
    }

    /// Stops at every descriptor, explicit ownership, and architectural boundary.
    fn boundary(&self, address: u64) -> Result<u64, Error> {
        let mut end = self.mtrrs.width.limit().min(1 << 48);
        for descriptor in self.descriptors {
            end = next_range_boundary(descriptor.range(self.mtrrs.width)?, address, end);
        }
        for range in self.private.iter().chain(self.mmio) {
            end = next_range_boundary(*range, address, end);
        }
        self.mtrrs.next_boundary(address, end)
    }
}

/// Fallible mapping stream; it owns no memory and never mutates the platform.
pub struct Mappings<'plan, 'data> {
    plan: &'plan PlatformMap<'data>,
    cursor: u64,
    failed: bool,
    host: bool,
    capabilities: PageCapabilities,
}

impl Mappings<'_, '_> {
    /// Produces one segment or terminates once no owned platform range remains.
    fn next_mapping(&mut self) -> Result<Option<Mapping>, Error> {
        loop {
            let Some(start) = self.plan.next_start(self.cursor)? else {
                return Ok(None);
            };
            if let Some(private) = self
                .plan
                .private
                .iter()
                .find(|range| !self.host && range.contains(start))
            {
                self.cursor = private.end;
                continue;
            }
            let (kind, firmware_type, attributes) = self.plan.source(start)?;
            let boundary = self.plan.boundary(start)?;
            if self.host && (kind == RegionKind::Mmio || attributes & 0x2000 != 0) {
                self.cursor = boundary;
                continue;
            }
            let memory_type = if kind == RegionKind::Mmio {
                MemoryType::Uncacheable
            } else {
                self.plan.mtrrs.memory_type(start)?
            };
            if kind != RegionKind::Mmio && !memory_type.supported_by(attributes) {
                return Err(Error::CacheConflict);
            }
            let (end, page_size) = leaf_segment(start, boundary, self.capabilities);
            let range = PhysicalRange::new(start, end, self.plan.mtrrs.width)?;
            self.cursor = end;
            return Ok(Some(Mapping {
                range,
                memory_type,
                kind,
                firmware_type,
                attributes,
                page_size,
            }));
        }
    }
}

impl Iterator for Mappings<'_, '_> {
    type Item = Result<Mapping, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        match self.next_mapping() {
            Ok(Some(mapping)) => Some(Ok(mapping)),
            Ok(None) => None,
            Err(error) => {
                self.failed = true;
                Some(Err(error))
            }
        }
    }
}

/// Finds a strict next endpoint without rounding or overflowing.
fn next_range_boundary(range: PhysicalRange, address: u64, mut end: u64) -> u64 {
    for candidate in [range.start, range.end] {
        if candidate > address {
            end = end.min(candidate);
        }
    }
    end
}

/// Groups leaves until a larger alignment or a smaller trailing fragment is met.
fn leaf_segment(start: u64, end: u64, capabilities: PageCapabilities) -> (u64, PageSize) {
    let mut size = PageSize::Base4K;
    for (candidate, allowed) in [
        (PageSize::Large2M, capabilities.two_mib),
        (PageSize::Huge1G, capabilities.one_gib),
    ] {
        if allowed && start % candidate.bytes() == 0 && end - start >= candidate.bytes() {
            size = candidate;
        }
    }
    let mut stop = end - end % size.bytes();
    for (candidate, allowed) in [
        (PageSize::Large2M, capabilities.two_mib),
        (PageSize::Huge1G, capabilities.one_gib),
    ] {
        if allowed && candidate.bytes() > size.bytes() {
            // Inputs are below 2^48, so adding the largest 1 GiB leaf cannot overflow.
            let aligned = start - start % candidate.bytes() + candidate.bytes();
            if aligned < stop && end - aligned >= candidate.bytes() {
                stop = aligned;
            }
        }
    }
    (stop, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_ram_access_requires_complete_permitted_firmware_coverage() {
        let first = ram(0, 0x1000);
        let mut second = ram(0x1000, 0x2000);
        for write in [false, true] {
            let descriptors = [second, first];
            let map = FirmwareMap::<2>::new(&descriptors, width()).unwrap();
            assert!(map.allows_ram_access(0, 16, write));
            assert!(map.allows_ram_access(0xff8, 16, write));
            assert!(map.allows_ram_access(0, 0x2000, write));
            assert!(!map.allows_ram_access(0, 0x2001, write));
            assert!(!map.allows_ram_access(0x2000, 1, write));
            assert!(!map.allows_ram_access(0, 0, write));
            assert!(!map.allows_ram_access(u64::MAX, 2, write));
        }
        for kind in [0, 8, 11, 12, 13, 15] {
            second.memory_type = kind;
            let descriptors = [first, second];
            let map = FirmwareMap::<2>::new(&descriptors, width()).unwrap();
            assert!(!map.allows_ram_access(0xff8, 16, false));
            assert!(!map.allows_ram_access(0xff8, 16, true));
        }
        second = ram(0x1000, 0x2000);
        second.attributes |= 0x20000;
        let descriptors = [first, second];
        let map = FirmwareMap::<2>::new(&descriptors, width()).unwrap();
        assert!(map.allows_ram_access(0xff8, 16, false));
        assert!(!map.allows_ram_access(0xff8, 16, true));
        second.attributes |= 0x2000;
        let descriptors = [first, second];
        assert!(
            !FirmwareMap::<2>::new(&descriptors, width())
                .unwrap()
                .allows_ram_access(0xff8, 16, false)
        );
        second.attributes = CACHE_WB | CACHE_WP;
        let descriptors = [first, second];
        assert!(
            FirmwareMap::<2>::new(&descriptors, width())
                .unwrap()
                .allows_ram_access(0xff8, 16, true)
        );
    }

    #[test]
    fn physical_ram_metadata_checks_actual_width_without_an_ept_fallback() {
        let width = PhysicalWidth::new(52).unwrap();
        let descriptor = ram(width.limit() - PAGE, width.limit());
        let descriptors = [descriptor];
        let map = FirmwareMap::<2>::new(&descriptors, width).unwrap();
        assert!(map.allows_ram_access(width.limit() - 16, 16, false));
        assert!(!map.allows_ram_access(width.limit() - 16, 17, false));
        assert!(FirmwareMap::<2>::new(&descriptors, PhysicalWidth::new(48).unwrap()).is_err());
        assert!(FirmwareMap::<2>::new(&[descriptor, descriptor], width).is_err());
        assert!(FirmwareMap::<2>::new(&[], width).is_err());
    }

    fn width() -> PhysicalWidth {
        PhysicalWidth::new(36).unwrap()
    }
    fn range(start: u64, end: u64) -> PhysicalRange {
        PhysicalRange::new(start, end, width()).unwrap()
    }
    fn ram(start: u64, end: u64) -> FirmwareDescriptor {
        FirmwareDescriptor {
            memory_type: 7,
            physical_start: start,
            number_of_pages: (end - start) / PAGE,
            attributes: CACHE_WB | CACHE_WT | CACHE_UC,
        }
    }
    fn capabilities() -> PageCapabilities {
        PageCapabilities::from_vmx_capability((1 << 6) | (1 << 14) | (1 << 16) | (1 << 17)).unwrap()
    }
    fn wb() -> Mtrrs<'static> {
        Mtrrs::new(width(), 0, (1 << 11) | 6, None, &[]).unwrap()
    }
    fn variable(start: u64, bytes: u64, kind: u64) -> VariableMtrr {
        VariableMtrr {
            base: start | kind,
            mask: ((width().limit() - 1) & !(bytes - 1)) | (1 << 11),
        }
    }
    fn collect(plan: &PlatformMap<'_>) -> Result<Vec<Mapping>, Error> {
        plan.mappings().collect()
    }

    #[test]
    fn descriptor_conversion_rejects_bad_layout_and_preserves_stride() {
        let mut bytes = [0; 96];
        bytes[..4].copy_from_slice(&7_u32.to_le_bytes());
        bytes[8..16].copy_from_slice(&PAGE.to_le_bytes());
        bytes[24..32].copy_from_slice(&2_u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&CACHE_WB.to_le_bytes());
        let first: [u8; 48] = bytes[..48].try_into().unwrap();
        bytes[48..].copy_from_slice(&first);
        let mut output = [FirmwareDescriptor::default(); 2];
        assert_eq!(decode_uefi_map(&bytes, 48, 1, &mut output), Ok(2));
        assert_eq!(output[0].physical_start, PAGE);
        assert_eq!(output[0], output[1]);
        for (stride, version) in [(0, 1), (39, 1), (41, 1), (264, 1), (48, 2)] {
            assert_eq!(
                decode_uefi_map(&bytes, stride, version, &mut output),
                Err(Error::DescriptorLayout)
            );
        }
        assert_eq!(
            decode_uefi_map(&bytes[..95], 48, 1, &mut output),
            Err(Error::DescriptorLayout)
        );
        assert_eq!(
            decode_uefi_map(&bytes, 48, 1, &mut output[..1]),
            Err(Error::OutputCapacity)
        );
        bytes[16..24].copy_from_slice(&1_u64.to_le_bytes());
        assert_eq!(
            decode_uefi_map(&bytes, 48, 1, &mut output),
            Err(Error::DescriptorLayout)
        );
    }

    #[test]
    fn mtrr_precedence_is_order_independent_and_rejects_undefined_overlap() {
        let wb_range = variable(0, 4 * MIB, 6);
        let wt_range = variable(2 * MIB, 2 * MIB, 4);
        let pair = [wb_range, wt_range];
        let state = Mtrrs::new(width(), 2, (1 << 11) | 6, None, &pair).unwrap();
        assert_eq!(state.memory_type(MIB), Ok(MemoryType::WriteBack));
        assert_eq!(state.memory_type(3 * MIB), Ok(MemoryType::WriteThrough));
        let conflict = [wb_range, variable(2 * MIB, 2 * MIB, 1)];
        assert!(matches!(
            Mtrrs::new(width(), 2 | (1 << 10), (1 << 11) | 6, None, &conflict),
            Err(Error::MtrrConflict)
        ));
        for triple in [
            [conflict[0], conflict[1], variable(2 * MIB, 2 * MIB, 0)],
            [variable(2 * MIB, 2 * MIB, 0), conflict[1], conflict[0]],
        ] {
            assert_eq!(
                Mtrrs::new(width(), 3 | (1 << 10), (1 << 11) | 6, None, &triple)
                    .unwrap()
                    .memory_type(3 * MIB),
                Ok(MemoryType::Uncacheable)
            );
        }
    }

    #[test]
    fn fixed_and_disabled_mtrrs_obey_enable_gates() {
        let mut fixed = [0x0606_0606_0606_0606; 11];
        fixed[10] = 0x0000_0000_0000_0000;
        let vars = [variable(0, 2 * MIB, 4)];
        let state = Mtrrs::new(
            width(),
            1 | (1 << 8),
            (1 << 11) | (1 << 10) | 6,
            Some(&fixed),
            &vars,
        )
        .unwrap();
        assert_eq!(state.memory_type(0), Ok(MemoryType::WriteBack));
        assert_eq!(state.memory_type(0xf_ffff), Ok(MemoryType::Uncacheable));
        assert_eq!(state.memory_type(MIB), Ok(MemoryType::WriteThrough));
        let invalid = [VariableMtrr {
            base: u64::MAX,
            mask: u64::MAX,
        }];
        assert_eq!(
            Mtrrs::new(width(), 1, 0, None, &invalid)
                .unwrap()
                .memory_type(0),
            Ok(MemoryType::Uncacheable)
        );
        assert!(matches!(
            Mtrrs::new(
                width(),
                1 | (1 << 8),
                (1 << 11) | (1 << 10) | 6,
                None,
                &vars
            ),
            Err(Error::MtrrState)
        ));
        let conflict = [variable(0, 2 * MIB, 6), variable(0, 2 * MIB, 1)];
        assert!(matches!(
            Mtrrs::new(
                width(),
                2 | (1 << 8) | (1 << 10),
                (1 << 11) | (1 << 10) | 6,
                Some(&fixed),
                &conflict
            ),
            Err(Error::MtrrConflict)
        ));
        fixed[0] = 2;
        assert!(matches!(
            Mtrrs::new(
                width(),
                1 | (1 << 8),
                (1 << 11) | (1 << 10) | 6,
                Some(&fixed),
                &vars
            ),
            Err(Error::MemoryType)
        ));
    }

    #[test]
    fn malformed_mtrrs_masks_types_and_capture_lengths_fail() {
        assert!(matches!(
            Mtrrs::new(width(), 1, (1 << 11) | 6, None, &[]),
            Err(Error::MtrrState)
        ));
        for bad in [
            VariableMtrr {
                base: 2,
                mask: (width().limit() - PAGE) | (1 << 11),
            },
            VariableMtrr {
                base: 0,
                mask: (width().limit() - PAGE - 0x2000) | (1 << 11),
            },
            variable(PAGE, 2 * MIB, 6),
            VariableMtrr {
                base: 1 << 40,
                mask: (1 << 11),
            },
        ] {
            assert!(Mtrrs::new(width(), 1, (1 << 11) | 6, None, &[bad]).is_err());
        }
        assert!(Mtrrs::new(width(), 0, (1 << 11) | 1, None, &[]).is_err());
        assert!(Mtrrs::new(width(), 0, (1 << 11) | 7, None, &[]).is_err());
    }

    #[test]
    fn private_pages_split_large_leaves_and_metadata_survives() {
        let mut descriptors = [ram(0, 4 * MIB), ram(4 * MIB, 6 * MIB)];
        descriptors[1].memory_type = 6;
        descriptors[1].attributes |= MEMORY_RUNTIME;
        let private = [
            range(2 * MIB, 2 * MIB + PAGE),
            range(4 * MIB, 4 * MIB + PAGE),
        ];
        let mappings =
            collect(&PlatformMap::new(&descriptors, &private, &[], wb(), capabilities()).unwrap())
                .unwrap();
        assert_eq!(mappings[0].page_size, PageSize::Large2M);
        assert!(
            mappings
                .iter()
                .all(|entry| private.iter().all(|hidden| !entry.range.overlaps(*hidden)))
        );
        assert!(mappings.iter().any(
            |entry| entry.kind == RegionKind::Runtime && entry.attributes & MEMORY_RUNTIME != 0
        ));
        assert_eq!(
            mappings
                .iter()
                .map(|entry| entry.range.bytes())
                .sum::<u64>(),
            6 * MIB - 2 * PAGE
        );
    }

    #[test]
    fn holes_unusable_memory_and_unaccepted_memory_stay_unmapped() {
        let mut descriptors = [
            ram(PAGE, 2 * PAGE),
            ram(3 * PAGE, 4 * PAGE),
            ram(5 * PAGE, 6 * PAGE),
            ram(7 * PAGE, 8 * PAGE),
        ];
        descriptors[1].memory_type = 8;
        descriptors[2].memory_type = 15;
        descriptors[3].memory_type = 9;
        let mmio = [range(2 * MIB, 4 * MIB)];
        let mappings =
            collect(&PlatformMap::new(&descriptors, &[], &mmio, wb(), capabilities()).unwrap())
                .unwrap();
        assert_eq!(mappings.len(), 3);
        assert_eq!(mappings[1].kind, RegionKind::Acpi);
        assert_eq!(mappings[2].memory_type, MemoryType::Uncacheable);
        assert_eq!(mappings[2].kind, RegionKind::Mmio);
        assert!(matches!(
            PlatformMap::new(
                &descriptors,
                &[],
                &[range(PAGE, 2 * PAGE)],
                wb(),
                capabilities()
            ),
            Err(Error::MmioConflict)
        ));
    }

    #[test]
    fn runtime_attributes_reject_unsupported_unmapped_memory_kinds() {
        for memory_type in [0, 8, 12, 13, 15] {
            let descriptors = [FirmwareDescriptor {
                memory_type,
                attributes: CACHE_WB | MEMORY_RUNTIME,
                ..ram(PAGE, 2 * PAGE)
            }];
            assert!(matches!(
                PlatformMap::new(&descriptors, &[], &[], wb(), capabilities()),
                Err(Error::RuntimeAttribute)
            ));
            // An override cannot turn an unsupported runtime layout into one
            // whose preservation was actually validated.
            assert!(matches!(
                PlatformMap::new(
                    &descriptors,
                    &[],
                    &[range(PAGE, 2 * PAGE)],
                    wb(),
                    capabilities()
                ),
                Err(Error::RuntimeAttribute)
            ));
        }
    }

    #[test]
    fn runtime_attributes_survive_all_supported_mapping_kinds() {
        for memory_type in [1, 2, 3, 4, 5, 6, 7, 9, 10, 11, 14] {
            let descriptors = [FirmwareDescriptor {
                memory_type,
                attributes: CACHE_WB | MEMORY_RUNTIME,
                ..ram(PAGE, 2 * PAGE)
            }];
            let mappings =
                collect(&PlatformMap::new(&descriptors, &[], &[], wb(), capabilities()).unwrap())
                    .unwrap();
            assert_eq!(mappings.len(), 1);
            assert_eq!(mappings[0].range, range(PAGE, 2 * PAGE));
            assert_eq!(mappings[0].attributes, descriptors[0].attributes);
            let expected_kind = match memory_type {
                9 | 10 => RegionKind::Acpi,
                11 => RegionKind::Mmio,
                _ => RegionKind::Runtime,
            };
            assert_eq!(mappings[0].kind, expected_kind);
        }
    }

    #[test]
    fn malformed_maps_and_actual_address_limits_are_rejected() {
        assert_eq!(PhysicalWidth::new(64), Err(Error::PhysicalWidth));
        assert_eq!(
            PhysicalRange::new(1, PAGE, width()),
            Err(Error::PhysicalRange)
        );
        assert!(
            PlatformMap::new(
                &[ram(0, 2 * PAGE), ram(PAGE, 3 * PAGE)],
                &[],
                &[],
                wb(),
                capabilities()
            )
            .is_err()
        );
        for descriptor in [
            FirmwareDescriptor {
                number_of_pages: u64::MAX,
                ..ram(0, PAGE)
            },
            FirmwareDescriptor {
                physical_start: width().limit(),
                ..ram(0, PAGE)
            },
            FirmwareDescriptor {
                memory_type: 16,
                ..ram(0, PAGE)
            },
            FirmwareDescriptor {
                attributes: MEMORY_ISA,
                ..ram(0, PAGE)
            },
            FirmwareDescriptor {
                memory_type: 6,
                ..ram(0, PAGE)
            },
        ] {
            assert!(PlatformMap::new(&[descriptor], &[], &[], wb(), capabilities()).is_err());
        }
        let wide = PhysicalWidth::new(52).unwrap();
        let mtrrs = Mtrrs::new(wide, 0, (1 << 11) | 6, None, &[]).unwrap();
        assert!(PlatformMap::new(&[ram(0, PAGE)], &[], &[], mtrrs, capabilities()).is_ok());
        assert!(matches!(
            PlatformMap::new(
                &[ram(1 << 48, (1 << 48) + PAGE)],
                &[],
                &[],
                mtrrs,
                capabilities()
            ),
            Err(Error::EptAddressWidth)
        ));
        assert!(
            PlatformMap::new(
                &[ram(width().limit() - PAGE, width().limit())],
                &[],
                &[],
                wb(),
                capabilities()
            )
            .is_ok()
        );
    }

    #[test]
    fn mtrr_and_descriptor_boundaries_prevent_large_page_crossings() {
        let vars = [variable(2 * MIB, 2 * MIB, 4)];
        let mtrrs = Mtrrs::new(width(), 1, (1 << 11) | 6, None, &vars).unwrap();
        let descriptors = [ram(0, 4 * MIB), ram(4 * MIB, 6 * MIB)];
        let mappings =
            collect(&PlatformMap::new(&descriptors, &[], &[], mtrrs, capabilities()).unwrap())
                .unwrap();
        assert_eq!(mappings.len(), 3);
        assert_eq!(mappings[1].memory_type, MemoryType::WriteThrough);
        let narrow = [FirmwareDescriptor {
            attributes: CACHE_WB,
            ..ram(0, 4 * MIB)
        }];
        assert_eq!(
            collect(&PlatformMap::new(&narrow, &[], &[], mtrrs, capabilities()).unwrap()),
            Err(Error::CacheConflict)
        );
    }

    #[test]
    fn page_sizes_follow_capabilities_alignment_and_tail_boundaries() {
        let descriptors = [ram(PAGE, 2 * 1024 * MIB + PAGE)];
        let mappings =
            collect(&PlatformMap::new(&descriptors, &[], &[], wb(), capabilities()).unwrap())
                .unwrap();
        assert_eq!(
            mappings
                .iter()
                .map(|entry| entry.page_size)
                .collect::<Vec<_>>(),
            vec![
                PageSize::Base4K,
                PageSize::Large2M,
                PageSize::Huge1G,
                PageSize::Base4K
            ]
        );
        let base_only = PageCapabilities::from_vmx_capability((1 << 6) | (1 << 14)).unwrap();
        let small =
            collect(&PlatformMap::new(&descriptors, &[], &[], wb(), base_only).unwrap()).unwrap();
        assert_eq!(small.len(), 1);
        assert_eq!(small[0].page_size, PageSize::Base4K);
        assert_eq!(small[0].range.bytes(), 2 * 1024 * MIB);
        assert_eq!(
            PageCapabilities::from_vmx_capability(0),
            Err(Error::EptCapability)
        );
    }
}
