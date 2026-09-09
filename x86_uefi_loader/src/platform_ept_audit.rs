//! Disposable EPT construction from the actual read-only preflight snapshot.
//!
//! This validates UEFI memory plus captured GCD/ACPI MMIO ranges. It does not prove
//! complete PCI/APIC aperture coverage, publish an EPTP, enable VMX, or leave
//! any allocation resident.

use crate::SerialPort;
use crate::chainload;
use crate::chainload::Error;
use crate::platform_snapshot::CpuSnapshot;
use crate::platform_snapshot::MemoryMap;
use core::fmt::Write;
use core::mem;
use core::ptr;
use core::slice;
use r_efi::efi;
use x86_64_hal::addr::EptPhys;
use x86_64_hal::ept;
use x86_64_hal::platform_memory;
use x86_64_hal::platform_memory::FirmwareDescriptor;
use x86_64_hal::platform_memory::PageCapabilities;
use x86_64_hal::platform_memory::PhysicalRange;
use x86_64_hal::platform_memory::PhysicalWidth;
use x86_64_hal::platform_memory::PlatformMap;

const PAGE: usize = 4096;
/// A bounded diagnostic arena, not an implicit limit or fallback for deployment.
const TABLE_PAGES: usize = 256;
const DESCRIPTORS: usize = 4096;
const DESCRIPTOR_PAGES: usize = (DESCRIPTORS * mem::size_of::<FirmwareDescriptor>()).div_ceil(PAGE);
const TOTAL_PAGES: usize = TABLE_PAGES + DESCRIPTOR_PAGES;

/// Exclusive temporary pages, retained until the snapshot callback has returned.
pub(crate) struct AuditStorage {
    base: u64,
    end: u64,
}

/// Allocates before GetMemoryMap, and releases storage on every callback result.
pub(crate) fn with_storage<T>(
    system_table: *mut efi::SystemTable,
    inspect: impl FnOnce(&mut AuditStorage) -> Result<T, Error>,
) -> Result<T, Error> {
    let services = chainload::boot_services(system_table)?;
    let mut base = 0;
    // SAFETY: the checked Boot Services table is live; `base` is a writable
    // physical-address output. This application has not called ExitBootServices.
    let status = unsafe {
        ((*services).allocate_pages)(
            efi::ALLOCATE_ANY_PAGES,
            efi::BOOT_SERVICES_DATA,
            TOTAL_PAGES,
            &mut base,
        )
    };
    if status.is_error() {
        return Err(Error::Firmware(
            "EPT audit AllocatePages",
            status.as_usize(),
        ));
    }
    let result = (|| {
        let end = base
            .checked_add((TOTAL_PAGES * PAGE) as u64)
            .filter(|_| base != 0 && base.is_multiple_of(PAGE as u64))
            .ok_or(Error::Firmware(
                "EPT audit storage range",
                efi::Status::COMPROMISED_DATA.as_usize(),
            ))?;
        inspect(&mut AuditStorage { base, end })
    })();
    // SAFETY: the callback has returned, so no references to this exclusive
    // allocation survive. The firmware allocation has not previously been freed.
    let release = unsafe { ((*services).free_pages)(base, TOTAL_PAGES) };
    if release.is_error() {
        return Err(Error::Firmware("EPT audit FreePages", release.as_usize()));
    }
    result
}

impl AuditStorage {
    /// Builds but never activates tables; all failures remain explicit diagnostics.
    pub(crate) fn inspect(
        &mut self,
        cpu: &CpuSnapshot,
        map: &MemoryMap<'_>,
        mmio: &[PhysicalRange],
        serial: &mut SerialPort,
    ) -> Result<(), Error> {
        let Some(capability) = cpu.ept_caps() else {
            let _ = writeln!(
                serial,
                "thin-hv: preflight EPT audit SKIP reason=no-ept-capability direct_vmx_ready=0"
            );
            return Ok(());
        };
        let Some(mtrrs) = cpu
            .mtrrs()
            .map_err(|error| report(serial, "MTRRs", error))?
        else {
            let _ = writeln!(
                serial,
                "thin-hv: preflight EPT audit SKIP reason=no-mtrr-capture direct_vmx_ready=0"
            );
            return Ok(());
        };
        let width = PhysicalWidth::new(cpu.physical_bits())
            .map_err(|error| report(serial, "physical width", error))?;
        let private = [PhysicalRange::new(self.base, self.end, width)
            .map_err(|error| report(serial, "storage range", error))?];
        let capabilities = PageCapabilities::from_vmx_capability(capability)
            .map_err(|error| report(serial, "EPT capability", error))?;
        if !cpu.identity_range_supported(self.base, TOTAL_PAGES * PAGE) {
            return Err(Error::Firmware(
                "EPT audit identity address range",
                efi::Status::UNSUPPORTED.as_usize(),
            ));
        }
        // SAFETY: AllocatePages supplied this exclusive identity-addressed range;
        // its complete physical and canonical bounds were checked using the
        // captured CPU state. Zero initializes EptPage and FirmwareDescriptor.
        unsafe { ptr::write_bytes(self.base as *mut u8, 0, TOTAL_PAGES * PAGE) };
        // SAFETY: these nonoverlapping slices cover initialized, properly aligned
        // parts of the exclusive AuditStorage allocation. `&mut self` prevents a
        // concurrent inspection, and neither slice escapes this call.
        let (pages, descriptors) = unsafe {
            (
                slice::from_raw_parts_mut(self.base as *mut ept::EptPage, TABLE_PAGES),
                slice::from_raw_parts_mut(
                    (self.base + (TABLE_PAGES * PAGE) as u64) as *mut FirmwareDescriptor,
                    DESCRIPTORS,
                ),
            )
        };
        let count =
            platform_memory::decode_uefi_map(map.bytes(), map.stride(), map.version(), descriptors)
                .map_err(|error| report(serial, "UEFI map", error))?;
        let plan = PlatformMap::new(&descriptors[..count], &private, mmio, mtrrs, capabilities)
            .map_err(|error| report(serial, "platform map", error))?;
        let physical = EptPhys::new(self.base)
            .ok_or_else(|| report(serial, "EPT arena", ept::BuildError::Storage))?;
        let tables = ept::build_platform_identity(&plan, pages, physical)
            .map_err(|error| report(serial, "EPT construction", error))?;
        let host_pages = ept::required_host_pages(&plan, cpu.host_paging()?)
            .map_err(|error| report(serial, "host map sizing", error))?;
        let _ = writeln!(
            serial,
            "thin-hv: preflight HOST sizing PASS tables={host_pages} mmio_window=uc activated=0 direct_vmx_ready=0"
        );
        let _ = writeln!(
            serial,
            "thin-hv: preflight EPT audit PASS scope=uefi-memory-map+gcd+acpi+pci tables={} leaves={} private_pages={} mmio_complete=0 direct_vmx_ready=0",
            tables.table_pages(),
            tables.leaf_count(),
            TOTAL_PAGES
        );
        Ok(())
    }
}

/// Logs only a bounded structural error; never includes raw firmware payloads.
fn report(serial: &mut SerialPort, stage: &'static str, error: impl core::fmt::Debug) -> Error {
    let _ = writeln!(
        serial,
        "thin-hv: preflight EPT audit FAIL stage={stage} error={error:?}"
    );
    Error::Firmware("EPT audit", efi::Status::UNSUPPORTED.as_usize())
}
