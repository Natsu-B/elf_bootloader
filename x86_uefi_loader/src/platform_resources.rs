//! Read-only PI GCD MMIO discovery and checked union with ACPI resources.
//!
//! PI 1.9 sections 4.1 and 7.2.4.8 define the DXE table and descriptor ABI.
//! A GCD inventory is not yet proof of complete PCI/APIC/ACPI aperture coverage.

use crate::SerialPort;
use crate::chainload;
use crate::chainload::Error;
use crate::platform_snapshot::MemoryMap;
use crate::platform_snapshot::malformed;
use crate::platform_snapshot::read_u32;
use crate::platform_snapshot::read_u64;
use core::fmt::Write;
use core::mem;
use core::ptr;
use core::slice;
use r_efi::efi;
use x86_64_hal::platform_memory::PhysicalRange;
use x86_64_hal::platform_memory::PhysicalWidth;

const DXE_GUID: efi::Guid = efi::Guid::from_fields(
    0x05ad34ba,
    0x6f02,
    0x4214,
    0x95,
    0x2e,
    &[0x4d, 0xa0, 0x39, 0x8e, 0x2b, 0xb9],
);
const DXE_SIGNATURE: u64 = 0x5652_4553_5f45_5844;
const GET_MAP_OFFSET: usize = mem::size_of::<efi::TableHeader>() + 6 * mem::size_of::<usize>();
const DXE_PREFIX: usize = GET_MAP_OFFSET + mem::size_of::<usize>();
const MAX_DESCRIPTORS: usize = 4096;
pub(crate) const MAX_MMIO_RANGES: usize = 128;
const PAGE: u64 = 4096;
const CACHE_ATTRIBUTES: u64 = efi::MEMORY_UC
    | efi::MEMORY_WC
    | efi::MEMORY_WT
    | efi::MEMORY_WB
    | efi::MEMORY_UCE
    | efi::MEMORY_WP;

/// Numeric fields preserve unknown enum values for validation, without reading
/// C padding. Allocation/owner handles are never dereferenced or logged.
#[derive(Clone, Copy)]
#[repr(C)]
struct GcdDescriptor {
    base: u64,
    length: u64,
    capabilities: u64,
    attributes: u64,
    kind: u32,
    image: efi::Handle,
    device: efi::Handle,
}

type GetMemorySpaceMap =
    unsafe extern "efiapi" fn(*mut usize, *mut *mut GcdDescriptor) -> efi::Status;

const _: () = assert!(DXE_PREFIX == 80 && GET_MAP_OFFSET == 72);
const _: () = assert!(mem::size_of::<GcdDescriptor>() == 56);
const _: () = assert!(mem::offset_of!(GcdDescriptor, kind) == 32);
const _: () = assert!(mem::offset_of!(GcdDescriptor, image) == 40);

/// Owned, normalized UC intervals. Only the initialized prefix reaches EPT.
pub(crate) struct MmioMap {
    ranges: [PhysicalRange; MAX_MMIO_RANGES],
    count: usize,
}

impl MmioMap {
    pub(crate) fn empty(width: PhysicalWidth) -> Result<Self, Error> {
        let unused =
            PhysicalRange::new(0, PAGE, width).map_err(|_| malformed("GCD physical width"))?;
        Ok(Self {
            ranges: [unused; MAX_MMIO_RANGES],
            count: 0,
        })
    }

    pub(crate) fn ranges(&self) -> &[PhysicalRange] {
        &self.ranges[..self.count]
    }

    /// Merge adjacent/overlapping page-rounded MMIO without relying on firmware
    /// enumeration order. Raw descriptors have already passed overlap checks.
    pub(crate) fn insert(
        &mut self,
        range: PhysicalRange,
        width: PhysicalWidth,
    ) -> Result<(), Error> {
        let mut range = PhysicalRange::new(range.start(), range.end(), width)
            .map_err(|_| malformed("platform MMIO physical width"))?;
        let mut index = 0;
        while index < self.count {
            let old = self.ranges[index];
            if old.start() <= range.end() && range.start() <= old.end() {
                range = PhysicalRange::new(
                    old.start().min(range.start()),
                    old.end().max(range.end()),
                    width,
                )
                .map_err(|_| malformed("platform merged MMIO range"))?;
                self.count -= 1;
                self.ranges[index] = self.ranges[self.count];
                // A newly enlarged interval may now touch an earlier interval.
                index = 0;
            } else {
                index += 1;
            }
        }
        let slot = self
            .ranges
            .get_mut(self.count)
            .ok_or_else(|| malformed("platform MMIO range capacity"))?;
        *slot = range;
        self.count += 1;
        Ok(())
    }
}

fn descriptor_bytes(count: usize) -> Result<usize, Error> {
    if !(1..=MAX_DESCRIPTORS).contains(&count) {
        return Err(malformed("GCD descriptor count"));
    }
    count
        .checked_mul(mem::size_of::<GcdDescriptor>())
        .ok_or_else(|| malformed("GCD descriptor size"))
}

/// Validates the ABI prefix before interpreting any address as a function.
fn service_header(bytes: &[u8]) -> Result<(usize, u64), Error> {
    let size = read_u32(bytes, 12).ok_or_else(|| malformed("DXE header size"))? as usize;
    let target =
        read_u64(bytes, GET_MAP_OFFSET).ok_or_else(|| malformed("DXE GetMemorySpaceMap slot"))?;
    if bytes.len() < DXE_PREFIX
        || read_u64(bytes, 0) != Some(DXE_SIGNATURE)
        || read_u32(bytes, 20) != Some(0)
        || !(DXE_PREFIX..=4096).contains(&size)
        || target == 0
    {
        return Err(malformed("DXE service header"));
    }
    Ok((size, target))
}

fn descriptor_end(region: &GcdDescriptor, width: PhysicalWidth) -> Result<u64, Error> {
    region
        .base
        .checked_add(region.length)
        .filter(|&end| region.length != 0 && end <= width.limit() && region.kind < 7)
        .ok_or_else(|| malformed("GCD descriptor range/type"))
}

/// Complete validation precedes publication; errors never return a valid prefix.
fn collect_mmio(descriptors: &[GcdDescriptor], width: PhysicalWidth) -> Result<MmioMap, Error> {
    descriptor_bytes(descriptors.len())?;
    let mut result = MmioMap::empty(width)?;
    // ponytail: bounded cold O(n²) validation preserves firmware order; use a
    // sorted sweep only if measured preflight startup time requires it.
    for (index, region) in descriptors.iter().enumerate() {
        let end = descriptor_end(region, width)?;
        for previous in &descriptors[..index] {
            if region.base < descriptor_end(previous, width)? && previous.base < end {
                return Err(malformed("overlapping GCD descriptors"));
            }
        }
    }
    for region in descriptors.iter().filter(|region| region.kind == 3) {
        if region.capabilities & efi::MEMORY_UC == 0
            || (region.attributes & CACHE_ATTRIBUTES).count_ones() > 1
        {
            return Err(malformed("GCD MMIO cache capabilities/attributes"));
        }
        let end = descriptor_end(region, width)?
            .checked_add(PAGE - 1)
            .ok_or_else(|| malformed("GCD MMIO rounding overflow"))?
            & !(PAGE - 1);
        let range = PhysicalRange::new(region.base & !(PAGE - 1), end, width)
            .map_err(|_| malformed("GCD MMIO page range"))?;
        for other in descriptors {
            if !matches!(other.kind, 0 | 1 | 3)
                && range.start() < descriptor_end(other, width)?
                && other.base < range.end()
            {
                return Err(malformed("GCD MMIO page overlaps system memory"));
            }
        }
        result.insert(range, width)?;
    }
    result.ranges[..result.count].sort_unstable_by_key(|range| range.start());
    Ok(result)
}

/// Checks additional ACPI resources while the complete GCD map is still live;
/// a reserved/absent GCD entry is not fabricated into RAM or a fixed PCI bucket.
fn collect_with_acpi(
    descriptors: &[GcdDescriptor],
    width: PhysicalWidth,
    acpi: &[PhysicalRange],
) -> Result<MmioMap, Error> {
    if acpi.len() > MAX_MMIO_RANGES {
        return Err(malformed("ACPI MMIO range capacity"));
    }
    let mut result = collect_mmio(descriptors, width)?;
    for range in acpi {
        for region in descriptors {
            if !matches!(region.kind, 0 | 1 | 3)
                && range.start() < descriptor_end(region, width)?
                && region.base < range.end()
            {
                return Err(malformed("ACPI MMIO conflicts with GCD system memory"));
            }
        }
        result.insert(*range, width)?;
    }
    result.ranges[..result.count].sort_unstable_by_key(|range| range.start());
    Ok(result)
}

/// The only DXE operation is GetMemorySpaceMap, whose temporary pool allocation
/// is released on every inspection result. No firmware table or device is changed.
pub(crate) fn collect(
    system_table: *mut efi::SystemTable,
    map: &MemoryMap<'_>,
    width: PhysicalWidth,
    acpi: &[PhysicalRange],
    serial: &mut SerialPort,
) -> Result<MmioMap, Error> {
    let mut address = None;
    for entry in map.configuration_tables(system_table)? {
        if entry.vendor_guid == DXE_GUID {
            let value = entry.vendor_table as usize as u64;
            if value == 0 || address.is_some() {
                return Err(malformed("null/duplicate DXE services table"));
            }
            address = Some(value);
        }
    }
    let Some(address) = address else {
        let _ = writeln!(
            serial,
            "thin-hv: preflight GCD unavailable mmio_complete=0 direct_vmx_ready=0"
        );
        return Err(Error::Firmware(
            "DXE GCD resource discovery unavailable",
            efi::Status::UNSUPPORTED.as_usize(),
        ));
    };
    if !address.is_multiple_of(mem::align_of::<efi::TableHeader>() as u64) {
        return Err(malformed("DXE table alignment"));
    }
    let prefix = map
        .firmware_bytes(address, DXE_PREFIX)
        .ok_or_else(|| malformed("DXE table prefix range"))?;
    let (size, target) = service_header(prefix)?;
    if map.firmware_bytes(address, size).is_none() || map.firmware_bytes(target, 1).is_none() {
        return Err(malformed("DXE service table/code range"));
    }
    let services = chainload::boot_services(system_table)?;
    // SAFETY: the live, aligned PI DXE table has the expected signature, checked
    // complete header range and non-null GetMemorySpaceMap ABI slot at offset 72.
    // Firmware owns the entry point in readable identity-addressed firmware RAM;
    // Boot Services remain active throughout this synchronous call and cleanup.
    let get_map: GetMemorySpaceMap = unsafe { mem::transmute(target as usize) };
    let mut count = 0;
    let mut pointer = ptr::null_mut();
    // SAFETY: these distinct writable locals match the PI output parameter ABI.
    // SUCCESS transfers one firmware AllocatePool buffer to this caller.
    let status = unsafe { get_map(&mut count, &mut pointer) };
    if status.is_error() {
        return Err(Error::Firmware("GetMemorySpaceMap", status.as_usize()));
    }
    if pointer.is_null() {
        return Err(malformed("GCD null descriptor buffer"));
    }
    let result = (|| {
        let bytes = descriptor_bytes(count)?;
        if !(pointer as usize).is_multiple_of(mem::align_of::<GcdDescriptor>())
            || map.firmware_bytes(pointer as usize as u64, bytes).is_none()
        {
            return Err(malformed("GCD descriptor buffer range/alignment"));
        }
        // SAFETY: SUCCESS supplies exactly count initialized C descriptors in
        // this outstanding pool buffer. Alignment, bounded byte extent and
        // identity-readable RAM were checked. Padding and owner pointers are
        // not inspected; no descriptor reference survives FreePool below.
        let descriptors = unsafe { slice::from_raw_parts(pointer, count) };
        collect_with_acpi(descriptors, width, acpi)
    })();
    // The allocation originates in the successful DXE service, including when
    // metadata/content validation fails. Cleanup errors take precedence.
    chainload::free_pool(services, pointer.cast())?;
    let result = result?;
    for (index, range) in result.ranges().iter().enumerate() {
        let _ = writeln!(
            serial,
            "thin-hv: preflight platform MMIO index={index} start={:#018x} end={:#018x} ept_type=UC",
            range.start(),
            range.end()
        );
    }
    let _ = writeln!(
        serial,
        "thin-hv: preflight MMIO PASS source=gcd+acpi descriptors={count} mmio_ranges={} mmio_complete=0 direct_vmx_ready=0",
        result.count
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn width() -> PhysicalWidth {
        PhysicalWidth::new(48).unwrap()
    }
    fn region(base: u64, length: u64, kind: u32) -> GcdDescriptor {
        GcdDescriptor {
            base,
            length,
            kind,
            capabilities: efi::MEMORY_UC,
            attributes: efi::MEMORY_UC,
            image: ptr::null_mut(),
            device: ptr::null_mut(),
        }
    }

    #[test]
    fn gcd_dxe_header_and_count_bounds() {
        let mut header = [0_u8; DXE_PREFIX];
        header[..8].copy_from_slice(&DXE_SIGNATURE.to_le_bytes());
        header[12..16].copy_from_slice(&(DXE_PREFIX as u32).to_le_bytes());
        header[GET_MAP_OFFSET..].copy_from_slice(&0x123000_u64.to_le_bytes());
        assert_eq!(service_header(&header).unwrap(), (DXE_PREFIX, 0x123000));
        for length in 0..DXE_PREFIX {
            assert!(service_header(&header[..length]).is_err());
        }
        for size in [0, DXE_PREFIX as u32 - 1, 4097, u32::MAX] {
            let mut bad = header;
            bad[12..16].copy_from_slice(&size.to_le_bytes());
            assert!(service_header(&bad).is_err());
        }
        for offset in [0, 20, GET_MAP_OFFSET] {
            let mut bad = header;
            if offset == GET_MAP_OFFSET {
                bad[offset..].fill(0);
            } else {
                bad[offset] ^= 1;
            }
            assert!(service_header(&bad).is_err());
        }
        assert_eq!(
            descriptor_bytes(MAX_DESCRIPTORS).unwrap(),
            MAX_DESCRIPTORS * 56
        );
        for count in [0, MAX_DESCRIPTORS + 1, usize::MAX] {
            assert!(descriptor_bytes(count).is_err());
        }
    }

    #[test]
    fn gcd_high_mmio_union_is_order_independent_and_not_fixed_eight_gib() {
        let base = 1_u64 << 39;
        let descriptors = [
            region(base + PAGE + 16, 128, 3),
            region(0, PAGE, 2),
            region(base + 32, 128, 3),
            region(base + PAGE - 8, 16, 3),
        ];
        let map = collect_mmio(&descriptors, width()).unwrap();
        assert_eq!(
            map.ranges(),
            &[PhysicalRange::new(base, base + 2 * PAGE, width()).unwrap()]
        );
        assert!(collect_mmio(&descriptors, PhysicalWidth::new(32).unwrap()).is_err());
        let top = region(width().limit() - 8, 8, 3);
        assert_eq!(
            collect_mmio(&[top], width()).unwrap().ranges()[0].end(),
            width().limit()
        );
        assert!(collect_mmio(&[region(width().limit() - 8, 9, 3)], width()).is_err());
    }

    #[test]
    fn gcd_rejects_malformed_ranges_overlaps_and_cache_conflicts() {
        for bad in [
            region(0, 0, 3),
            region(u64::MAX - 8, 16, 3),
            region(0, PAGE, 7),
        ] {
            assert!(collect_mmio(&[bad], width()).is_err());
        }
        for kind in 0..7 {
            assert!(
                collect_mmio(&[region(0, PAGE, 3), region(PAGE - 1, PAGE, kind)], width()).is_err()
            );
        }
        let mut bad = region(0, PAGE, 3);
        bad.capabilities = efi::MEMORY_WB;
        assert!(collect_mmio(&[bad], width()).is_err());
        bad.capabilities |= efi::MEMORY_UC;
        bad.attributes |= efi::MEMORY_WB;
        assert!(collect_mmio(&[bad], width()).is_err());
        bad.attributes = efi::MEMORY_WB;
        assert!(collect_mmio(&[bad], width()).is_ok()); // UC is a safe advertised downgrade.
    }

    #[test]
    fn gcd_page_rounding_never_converts_adjacent_system_memory() {
        for kind in 2..7 {
            if kind == 3 {
                continue;
            }
            assert!(
                collect_mmio(&[region(0, 16, 3), region(16, PAGE - 16, kind)], width()).is_err()
            );
        }
        for kind in [0, 1, 3] {
            assert!(
                collect_mmio(&[region(0, 16, 3), region(16, PAGE - 16, kind)], width()).is_ok()
            );
        }
        assert!(collect_mmio(&[region(0, PAGE, 3), region(PAGE, PAGE, 2)], width()).is_ok());
    }

    #[test]
    fn acpi_mmio_union_checks_gcd_ram_and_the_current_physical_width() {
        let ecam = PhysicalRange::new(0xe000_0000, 0xf000_0000, width()).unwrap();
        for kind in [0, 1, 3] {
            let map = collect_with_acpi(
                &[region(ecam.start(), ecam.bytes(), kind)],
                width(),
                &[ecam],
            )
            .unwrap();
            assert_eq!(map.ranges(), &[ecam]);
        }
        for kind in [2, 4, 5, 6] {
            assert!(
                collect_with_acpi(
                    &[region(ecam.start(), ecam.bytes(), kind)],
                    width(),
                    &[ecam]
                )
                .is_err()
            );
        }
        let high = PhysicalRange::new(1 << 39, (1 << 39) + PAGE, width()).unwrap();
        assert!(
            collect_with_acpi(
                &[region(0, PAGE, 2)],
                PhysicalWidth::new(32).unwrap(),
                &[high]
            )
            .is_err()
        );
        assert!(
            collect_with_acpi(&[region(0, PAGE, 2)], width(), &[ecam; MAX_MMIO_RANGES + 1])
                .is_err()
        );
        let low = region(0x8000_0000, 0x6000_0000, 3);
        let reserved = region(ecam.start(), ecam.bytes(), 1);
        let joined = collect_with_acpi(&[reserved, low], width(), &[ecam]).unwrap();
        assert_eq!(
            joined.ranges(),
            &[PhysicalRange::new(0x8000_0000, 0xf000_0000, width()).unwrap()]
        );
    }

    #[test]
    fn gcd_capacity_is_bounded_and_errors_do_not_publish_prefixes() {
        let descriptors: std::vec::Vec<_> = (0..=MAX_MMIO_RANGES)
            .map(|index| region(index as u64 * 2 * PAGE, PAGE, 3))
            .collect();
        assert_eq!(
            collect_mmio(&descriptors[..MAX_MMIO_RANGES], width())
                .unwrap()
                .ranges()
                .len(),
            MAX_MMIO_RANGES
        );
        assert!(collect_mmio(&descriptors, width()).is_err());
        assert!(collect_mmio(&[], width()).is_err());
        assert!(
            collect_mmio(&[region(0, PAGE, 2)], width())
                .unwrap()
                .ranges()
                .is_empty()
        );
    }
}
