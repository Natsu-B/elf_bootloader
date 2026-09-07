//! Read-only machine inventory before any physical Direct-VMX launch.

use crate::SerialPort;
use crate::chainload;
use crate::chainload::Error;
use core::ffi::c_void;
use core::fmt::Write;
use core::mem;
use core::ptr;
use core::slice;
use r_efi::efi;
use x86_64_hal::cpu;
use x86_64_hal::vmx;

/// Firmware inventory bounds; exceeding them is an explicit unsupported layout.
const MAX_MAP_BYTES: usize = 1024 * 1024;
const MAX_DESCRIPTORS: usize = 4096;
const MAX_TABLE_ENTRIES: usize = 4096;
const MAX_ACPI_BYTES: usize = 1024 * 1024;
const ACPI_HEADER_BYTES: usize = 36;
const PAGE_BYTES: u64 = 4096;

/// Returns a malformed-platform error without exposing firmware contents.
fn malformed(context: &'static str) -> Error {
    Error::Firmware(context, efi::Status::COMPROMISED_DATA.as_usize())
}

/// Collects an inventory and returns to firmware without retaining a runtime.
pub(crate) fn run(
    _image: efi::Handle,
    system_table: *mut efi::SystemTable,
    serial: &mut SerialPort,
) -> Result<efi::Status, Error> {
    serial.init();
    let _ = writeln!(serial, "thin-hv: backend=physical-preflight project_vmx=0");
    let result = inventory(system_table, serial);
    match result {
        Ok(()) => {
            let _ = writeln!(serial, "thin-hv: physical preflight PASS");
            Ok(efi::Status::SUCCESS)
        }
        Err(error) => {
            let _ = writeln!(serial, "thin-hv: physical preflight FAIL: {error}");
            Err(error)
        }
    }
}

/// Uses only temporary Boot Services pool storage; all exits release it.
fn inventory(system_table: *mut efi::SystemTable, serial: &mut SerialPort) -> Result<(), Error> {
    let boot_services = chainload::boot_services(system_table)?;
    let physical_bits = inventory_cpu(serial)?;
    let buffer = acquire_memory_map(boot_services)?;
    let result = (|| {
        // SAFETY: GetMemoryMap initialized this bounded prefix of our exclusive,
        // zero-initialized pool allocation, retained until after this closure.
        let bytes = unsafe { slice::from_raw_parts(buffer.pointer.cast::<u8>(), buffer.size) };
        let map = MemoryMap::new(bytes, buffer.stride, buffer.version, physical_bits)?;
        let _ = writeln!(
            serial,
            "thin-hv: preflight memory_map descriptors={} stride={} version={}",
            map.count(),
            buffer.stride,
            buffer.version
        );
        for index in 0..map.count() {
            let region = map
                .region(index)
                .ok_or_else(|| malformed("memory descriptor"))?;
            let _ = writeln!(
                serial,
                "thin-hv: preflight memory index={index} type={} physical={:#018x} virtual={:#018x} pages={} attributes={:#018x}",
                region.kind, region.start, region.virtual_start, region.pages, region.attributes
            );
        }
        inventory_tables(system_table, &map, serial)
    })();
    let release = chainload::free_pool(boot_services, buffer.pointer);
    // A release failure is itself actionable, even when inventory also failed.
    release?;
    result
}

/// Logs architectural CPU capabilities without enabling any CPU feature.
fn inventory_cpu(serial: &mut SerialPort) -> Result<u8, Error> {
    let vendor = cpu::cpuid(0, 0);
    if vendor.eax < 1 {
        return Err(malformed("CPUID basic leaves"));
    }
    let mut vendor_bytes = [0_u8; 12];
    vendor_bytes[..4].copy_from_slice(&vendor.ebx.to_le_bytes());
    vendor_bytes[4..8].copy_from_slice(&vendor.edx.to_le_bytes());
    vendor_bytes[8..].copy_from_slice(&vendor.ecx.to_le_bytes());
    if !vendor_bytes.iter().all(u8::is_ascii_graphic) {
        return Err(malformed("CPUID vendor"));
    }
    let vendor_name = core::str::from_utf8(&vendor_bytes).map_err(|_| malformed("CPUID vendor"))?;
    let features = cpu::cpuid(1, 0);
    let (family, model) = family_model(features.eax);
    let extended = cpu::cpuid(0x8000_0000, 0).eax;
    if extended < 0x8000_0008 {
        return Err(Error::Firmware(
            "CPUID physical width unavailable",
            efi::Status::UNSUPPORTED.as_usize(),
        ));
    }
    let physical_bits = (cpu::cpuid(0x8000_0008, 0).eax & 0xff) as u8;
    if !(32..=52).contains(&physical_bits) {
        return Err(malformed("CPUID physical width"));
    }
    let _ = writeln!(
        serial,
        "thin-hv: preflight cpu vendor={vendor_name} family={family} model={model} stepping={} physical_bits={physical_bits}",
        features.eax & 0xf
    );
    let vmx_present = features.ecx & (1 << 5) != 0;
    // Inventory is not a claim that the unfinished physical L0 is launch-ready.
    let _ = writeln!(
        serial,
        "thin-hv: preflight VMX={} direct_vmx_ready=0",
        u8::from(vmx_present)
    );
    if vmx_present {
        // SAFETY: CPUID.VMX advertises these architectural read-only MSRs and
        // this UEFI application runs at CPL0 before ExitBootServices.
        let basic = unsafe {
            log_msr(serial, "IA32_FEATURE_CONTROL", cpu::IA32_FEATURE_CONTROL);
            let raw = log_msr(serial, "IA32_VMX_BASIC", vmx::IA32_VMX_BASIC);
            for (name, index) in [
                ("IA32_VMX_PINBASED_CTLS", vmx::IA32_VMX_PINBASED_CTLS),
                ("IA32_VMX_EXIT_CTLS", vmx::IA32_VMX_EXIT_CTLS),
                ("IA32_VMX_ENTRY_CTLS", vmx::IA32_VMX_ENTRY_CTLS),
                ("IA32_VMX_MISC", vmx::IA32_VMX_MISC),
                ("IA32_VMX_CR0_FIXED0", vmx::IA32_VMX_CR0_FIXED0),
                ("IA32_VMX_CR0_FIXED1", vmx::IA32_VMX_CR0_FIXED1),
                ("IA32_VMX_CR4_FIXED0", vmx::IA32_VMX_CR4_FIXED0),
                ("IA32_VMX_CR4_FIXED1", vmx::IA32_VMX_CR4_FIXED1),
                ("IA32_VMX_VMCS_ENUM", vmx::IA32_VMX_VMCS_ENUM),
            ] {
                log_msr(serial, name, index);
            }
            vmx::VmxBasic::from_msr(raw)
        };
        // SAFETY: CPUID.VMX advertises the legacy primary-controls MSR.
        let primary = unsafe {
            log_msr(
                serial,
                "IA32_VMX_PROCBASED_CTLS",
                vmx::IA32_VMX_PROCBASED_CTLS,
            )
        };
        if basic.true_controls {
            // SAFETY: IA32_VMX_BASIC[55] advertises all four true-control MSRs.
            unsafe {
                for (name, index) in [
                    (
                        "IA32_VMX_TRUE_PINBASED_CTLS",
                        vmx::IA32_VMX_TRUE_PINBASED_CTLS,
                    ),
                    (
                        "IA32_VMX_TRUE_PROCBASED_CTLS",
                        vmx::IA32_VMX_TRUE_PROCBASED_CTLS,
                    ),
                    ("IA32_VMX_TRUE_EXIT_CTLS", vmx::IA32_VMX_TRUE_EXIT_CTLS),
                    ("IA32_VMX_TRUE_ENTRY_CTLS", vmx::IA32_VMX_TRUE_ENTRY_CTLS),
                ] {
                    log_msr(serial, name, index);
                }
            }
        } else {
            let _ = writeln!(serial, "thin-hv: preflight VMX_TRUE_CONTROLS=unavailable");
        }
        if primary & (1 << 63) != 0 {
            // SAFETY: Primary allowed-one bit31 advertises secondary controls.
            let secondary = unsafe {
                log_msr(
                    serial,
                    "IA32_VMX_PROCBASED_CTLS2",
                    vmx::IA32_VMX_PROCBASED_CTLS2,
                )
            };
            if (secondary >> 32) & ((1 << 1) | (1 << 5)) != 0 {
                // SAFETY: Secondary allowed-one EPT or VPID advertises this MSR.
                unsafe { log_msr(serial, "IA32_VMX_EPT_VPID_CAP", vmx::IA32_VMX_EPT_VPID_CAP) };
            } else {
                let _ = writeln!(
                    serial,
                    "thin-hv: preflight IA32_VMX_EPT_VPID_CAP=unavailable"
                );
            }
        } else {
            let _ = writeln!(
                serial,
                "thin-hv: preflight VMX_SECONDARY_CONTROLS=unavailable IA32_VMX_EPT_VPID_CAP=unavailable"
            );
        }
    } else {
        let _ = writeln!(
            serial,
            "thin-hv: preflight IA32_FEATURE_CONTROL=unavailable IA32_VMX_BASIC=unavailable VMX_CONTROLS=unavailable IA32_VMX_EPT_VPID_CAP=unavailable"
        );
    }
    let _ = writeln!(
        serial,
        "thin-hv: preflight CR0={:#018x} CR3={:#018x} CR4={:#018x}",
        cpu::read_cr0(),
        cpu::read_cr3(),
        cpu::read_cr4()
    );
    if features.edx & (1 << 16) != 0 {
        // SAFETY: CPUID.PAT advertises IA32_PAT; RDMSR does not change it.
        unsafe { log_msr(serial, "IA32_PAT", cpu::IA32_PAT) };
    } else {
        let _ = writeln!(serial, "thin-hv: preflight IA32_PAT=unavailable");
    }
    if features.edx & (1 << 12) != 0 {
        // SAFETY: CPUID.MTRR advertises MTRRCAP and MTRR_DEF_TYPE.
        let capability = unsafe {
            let value = log_msr(serial, "IA32_MTRRCAP", 0xfe);
            log_msr(serial, "IA32_MTRR_DEF_TYPE", 0x2ff);
            value
        };
        let count = capability as u8;
        if count > 64 {
            return Err(Error::Firmware(
                "MTRR variable count exceeds inventory bound",
                efi::Status::UNSUPPORTED.as_usize(),
            ));
        }
        for index in 0..u32::from(count) {
            // SAFETY: MTRRCAP.VCNT advertises each bounded base/mask pair.
            unsafe {
                log_msr(serial, "IA32_MTRR_PHYSBASE", 0x200 + index * 2);
                log_msr(serial, "IA32_MTRR_PHYSMASK", 0x201 + index * 2);
            }
        }
        if capability & (1 << 8) != 0 {
            // SAFETY: MTRRCAP.FIX advertises all eleven architectural fixed MSRs.
            unsafe {
                for index in [
                    0x250, 0x258, 0x259, 0x268, 0x269, 0x26a, 0x26b, 0x26c, 0x26d, 0x26e, 0x26f,
                ] {
                    log_msr(serial, "IA32_MTRR_FIXED", index);
                }
            }
        }
    } else {
        let _ = writeln!(serial, "thin-hv: preflight MTRR=unavailable");
    }
    Ok(physical_bits)
}

/// Reads a capability-gated MSR at CPL0; the caller must establish its presence.
unsafe fn log_msr(serial: &mut SerialPort, name: &str, index: u32) -> u64 {
    // SAFETY: The caller verified this specific MSR's architectural CPUID or
    // VMX/MTRR capability gate and UEFI executes at CPL0.
    let value = unsafe { cpu::rdmsr(index) };
    let _ = writeln!(
        serial,
        "thin-hv: preflight {name} msr={index:#05x} value={value:#018x}"
    );
    value
}

/// Decodes CPUID.1:EAX without conflating base and extended family/model.
fn family_model(signature: u32) -> (u32, u32) {
    let base_family = (signature >> 8) & 0xf;
    let family = base_family
        + if base_family == 0xf {
            (signature >> 20) & 0xff
        } else {
            0
        };
    let model = ((signature >> 4) & 0xf)
        | if base_family == 6 || base_family == 0xf {
            (signature >> 12) & 0xf0
        } else {
            0
        };
    (family, model)
}

/// One temporary pool allocation containing the firmware memory-map snapshot.
struct MapBuffer {
    pointer: *mut c_void,
    size: usize,
    stride: usize,
    version: u32,
}

/// Acquires a bounded map, accounting for map growth caused by our allocation.
fn acquire_memory_map(boot_services: *mut efi::BootServices) -> Result<MapBuffer, Error> {
    let mut size = 0;
    let mut key = 0;
    let mut stride = 0;
    let mut version = 0;
    // SAFETY: The checked firmware Boot Services table is live; all output
    // pointers refer to local storage and a zero-size buffer may be null.
    let status = unsafe {
        ((*boot_services).get_memory_map)(
            &mut size,
            ptr::null_mut(),
            &mut key,
            &mut stride,
            &mut version,
        )
    };
    if status != efi::Status::BUFFER_TOO_SMALL {
        return Err(Error::Firmware(
            "GetMemoryMap(size)",
            if status.is_error() {
                status.as_usize()
            } else {
                efi::Status::COMPROMISED_DATA.as_usize()
            },
        ));
    }
    for _ in 0..4 {
        if !valid_stride(stride) || size == 0 {
            return Err(malformed("GetMemoryMap descriptor stride"));
        }
        let capacity = stride
            .checked_mul(8)
            .and_then(|slack| size.checked_add(slack))
            .filter(|value| *value <= MAX_MAP_BYTES)
            .ok_or_else(|| malformed("GetMemoryMap allocation size"))?;
        let mut pointer = ptr::null_mut();
        // SAFETY: The firmware table is live and the bounded allocation result
        // is written to our local pointer, without changing persistent state.
        let status = unsafe {
            ((*boot_services).allocate_pool)(efi::BOOT_SERVICES_DATA, capacity, &mut pointer)
        };
        if status.is_error() {
            return Err(Error::Firmware(
                "AllocatePool(preflight)",
                status.as_usize(),
            ));
        }
        if pointer.is_null() {
            return Err(malformed("AllocatePool(preflight) null result"));
        }
        // SAFETY: AllocatePool returned this exclusive allocation of capacity
        // bytes; zeroing also initializes descriptor padding before firmware use.
        unsafe { ptr::write_bytes(pointer.cast::<u8>(), 0, capacity) };
        size = capacity;
        // SAFETY: Firmware receives an exclusive, aligned pool of exactly size
        // bytes and live local output pointers. No ExitBootServices is performed.
        let status = unsafe {
            ((*boot_services).get_memory_map)(
                &mut size,
                pointer.cast(),
                &mut key,
                &mut stride,
                &mut version,
            )
        };
        if status == efi::Status::BUFFER_TOO_SMALL {
            chainload::free_pool(boot_services, pointer)?;
            continue;
        }
        if status.is_error() || size > capacity {
            chainload::free_pool(boot_services, pointer)?;
            return Err(Error::Firmware(
                "GetMemoryMap(snapshot)",
                if status.is_error() {
                    status.as_usize()
                } else {
                    efi::Status::COMPROMISED_DATA.as_usize()
                },
            ));
        }
        return Ok(MapBuffer {
            pointer,
            size,
            stride,
            version,
        });
    }
    Err(Error::Firmware(
        "GetMemoryMap changed across four attempts",
        efi::Status::ABORTED.as_usize(),
    ))
}

/// Validates the known descriptor prefix and a bounded extensible stride.
fn valid_stride(stride: usize) -> bool {
    (mem::size_of::<efi::MemoryDescriptor>()..=256).contains(&stride) && stride.is_multiple_of(8)
}

/// Copy of the descriptor fields needed for range validation and logging.
#[derive(Clone, Copy)]
struct MemoryRegion {
    kind: u32,
    start: u64,
    virtual_start: u64,
    pages: u64,
    attributes: u64,
}

impl MemoryRegion {
    /// Computes the exclusive physical end without overflowing.
    fn end(self) -> Option<u64> {
        self.start.checked_add(self.pages.checked_mul(PAGE_BYTES)?)
    }

    /// Only CPU-readable RAM types may back dereferenced firmware tables.
    fn readable(self) -> bool {
        matches!(self.kind, 1..=7 | 9 | 10 | 14) && self.attributes & efi::MEMORY_RP == 0
    }
}

/// Validated borrowed UEFI memory map, preserving firmware descriptor order.
struct MemoryMap<'a> {
    bytes: &'a [u8],
    stride: usize,
}

impl<'a> MemoryMap<'a> {
    /// Rejects malformed, overlapping, overflowing or unsupported descriptors.
    fn new(bytes: &'a [u8], stride: usize, version: u32, bits: u8) -> Result<Self, Error> {
        if version != efi::MEMORY_DESCRIPTOR_VERSION
            || !valid_stride(stride)
            || bytes.is_empty()
            || !bytes.len().is_multiple_of(stride)
            || bytes.len() / stride > MAX_DESCRIPTORS
            || !(32..=52).contains(&bits)
        {
            return Err(malformed("memory map layout/version"));
        }
        let map = Self { bytes, stride };
        for index in 0..map.count() {
            let region = map
                .region(index)
                .ok_or_else(|| malformed("memory descriptor"))?;
            let end = region
                .end()
                .ok_or_else(|| malformed("memory descriptor overflow"))?;
            if region.kind > efi::UNACCEPTED_MEMORY_TYPE
                || region.pages == 0
                || !region.start.is_multiple_of(PAGE_BYTES)
                || !region.virtual_start.is_multiple_of(PAGE_BYTES)
                || region
                    .virtual_start
                    .checked_add(region.pages * PAGE_BYTES)
                    .is_none()
                || end > 1_u64 << bits
            {
                return Err(malformed("memory descriptor type/address"));
            }
            // ponytail: at most 4096 descriptors permit a heap-free pair scan;
            // sort a descriptor-index buffer if firmware inventory size grows.
            for previous in 0..index {
                let other = map
                    .region(previous)
                    .ok_or_else(|| malformed("memory descriptor"))?;
                if region.start
                    < other
                        .end()
                        .ok_or_else(|| malformed("memory descriptor overflow"))?
                    && other.start < end
                {
                    return Err(malformed("overlapping memory descriptors"));
                }
            }
        }
        Ok(map)
    }

    /// Returns the validated descriptor count.
    fn count(&self) -> usize {
        self.bytes.len() / self.stride
    }

    /// Copies only the standard descriptor prefix, ignoring extension bytes.
    fn region(&self, index: usize) -> Option<MemoryRegion> {
        let start = index.checked_mul(self.stride)?;
        let bytes = self.bytes.get(start..start.checked_add(40)?)?;
        Some(MemoryRegion {
            kind: read_u32(bytes, 0)?,
            start: read_u64(bytes, 8)?,
            virtual_start: read_u64(bytes, 16)?,
            pages: read_u64(bytes, 24)?,
            attributes: read_u64(bytes, 32)?,
        })
    }

    /// Allows spans across adjacent readable descriptors, never gaps or MMIO.
    fn readable(&self, address: u64, length: usize) -> bool {
        let Some(end) = address.checked_add(length as u64) else {
            return false;
        };
        if address == 0 || length == 0 || length > isize::MAX as usize {
            return false;
        }
        let mut cursor = address;
        while cursor < end {
            let Some(region_end) = (0..self.count())
                .filter_map(|index| self.region(index))
                .find(|region| {
                    region.readable()
                        && region.start <= cursor
                        && region.end().is_some_and(|limit| cursor < limit)
                })
                .and_then(MemoryRegion::end)
            else {
                return false;
            };
            cursor = end.min(region_end);
        }
        true
    }

    /// Reads only a validated live firmware RAM span before ExitBootServices.
    fn firmware_bytes(&self, address: u64, length: usize) -> Option<&[u8]> {
        let linear_bits = if cpu::read_cr4() & (1 << 12) == 0 {
            48
        } else {
            57
        };
        if !self.readable(address, length)
            || address.checked_add(length as u64)? > 1_u64 << (linear_bits - 1)
        {
            return None;
        }
        // SAFETY: The complete span is covered by readable, non-MMIO firmware
        // RAM descriptors, below the identity-mapped canonical address limit.
        // The firmware owns these immutable tables while our UEFI app is active.
        Some(unsafe { slice::from_raw_parts(address as *const u8, length) })
    }
}

/// Little-endian field readers never read beyond a validated byte slice.
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
/// Reads one bounded little-endian 64-bit field.
fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// Logs only table presence/addresses; never firmware identity or key content.
fn inventory_tables(
    system_table: *mut efi::SystemTable,
    map: &MemoryMap<'_>,
    serial: &mut SerialPort,
) -> Result<(), Error> {
    let address = system_table as usize as u64;
    if !address.is_multiple_of(mem::align_of::<efi::SystemTable>() as u64)
        || map
            .firmware_bytes(address, mem::size_of::<efi::SystemTable>())
            .is_none()
    {
        return Err(malformed("UEFI system table range"));
    }
    // SAFETY: The aligned, complete SystemTable lies in readable firmware RAM;
    // the UEFI entry contract guarantees its initialized fields remain live.
    let system = unsafe { &*system_table };
    if system.hdr.signature != efi::SYSTEM_TABLE_SIGNATURE
        || !(mem::size_of::<efi::SystemTable>()..=4096).contains(&(system.hdr.header_size as usize))
        || map
            .firmware_bytes(address, system.hdr.header_size as usize)
            .is_none()
    {
        return Err(malformed("UEFI system table header"));
    }
    inventory_runtime(system.runtime_services, map, serial)?;
    let count = system.number_of_table_entries;
    let table_bytes = count
        .checked_mul(mem::size_of::<efi::ConfigurationTable>())
        .ok_or_else(|| malformed("configuration table size"))?;
    if count > MAX_TABLE_ENTRIES
        || (count != 0
            && (!(system.configuration_table as usize)
                .is_multiple_of(mem::align_of::<efi::ConfigurationTable>())
                || map
                    .firmware_bytes(system.configuration_table as usize as u64, table_bytes)
                    .is_none()))
    {
        return Err(malformed("configuration table range/count"));
    }
    let entries = if count == 0 {
        &[][..]
    } else {
        // SAFETY: Count and alignment were checked and the complete initialized
        // firmware array is in readable RAM, retained throughout this inventory.
        unsafe { slice::from_raw_parts(system.configuration_table, count) }
    };
    let mut acpi1 = None;
    let mut acpi2 = None;
    let mut smbios = None;
    let mut smbios3 = None;
    for entry in entries {
        let slot = if entry.vendor_guid == efi::ACPI_10_TABLE_GUID {
            Some(&mut acpi1)
        } else if entry.vendor_guid == efi::ACPI_20_TABLE_GUID {
            Some(&mut acpi2)
        } else if entry.vendor_guid == efi::SMBIOS_TABLE_GUID {
            Some(&mut smbios)
        } else if entry.vendor_guid == efi::SMBIOS3_TABLE_GUID {
            Some(&mut smbios3)
        } else {
            None
        };
        if let Some(slot) = slot {
            let address = entry.vendor_table as usize as u64;
            if address == 0 || slot.is_some_and(|previous| previous != address) {
                return Err(malformed("null/duplicate firmware system table"));
            }
            *slot = Some(address);
        }
    }
    for (name, address) in [
        ("ACPI1", acpi1),
        ("ACPI2", acpi2),
        ("SMBIOS", smbios),
        ("SMBIOS3", smbios3),
    ] {
        let _ = writeln!(
            serial,
            "thin-hv: preflight table={name} present={} address={:#018x}",
            u8::from(address.is_some()),
            address.unwrap_or(0)
        );
    }
    if let Some(rsdp) = acpi2.or(acpi1) {
        let present = scan_acpi(rsdp, map)?;
        let _ = writeln!(
            serial,
            "thin-hv: preflight MSDM={} payload=not-read",
            if present { "present" } else { "absent" }
        );
    } else {
        let _ = writeln!(
            serial,
            "thin-hv: preflight MSDM=unavailable reason=no-acpi-system-table payload=not-read"
        );
    }
    Ok(())
}

/// Reports Runtime Services entry-point availability without calling any entry.
fn inventory_runtime(
    runtime: *mut efi::RuntimeServices,
    map: &MemoryMap<'_>,
    serial: &mut SerialPort,
) -> Result<(), Error> {
    if runtime.is_null() {
        let _ = writeln!(serial, "thin-hv: preflight runtime_services=unavailable");
        return Ok(());
    }
    let address = runtime as usize as u64;
    let bytes = map
        .firmware_bytes(address, mem::size_of::<efi::RuntimeServices>())
        .ok_or_else(|| malformed("Runtime Services table range"))?;
    if read_u64(bytes, 0) != Some(efi::RUNTIME_SERVICES_SIGNATURE)
        || read_u32(bytes, 12).is_none_or(|size| {
            !(mem::size_of::<efi::RuntimeServices>()..=4096).contains(&(size as usize))
                || map.firmware_bytes(address, size as usize).is_none()
        })
    {
        return Err(malformed("Runtime Services table header"));
    }
    let _ = writeln!(
        serial,
        "thin-hv: preflight runtime_services=present address={address:#018x} revision={:#010x}",
        read_u32(bytes, 8).ok_or_else(|| malformed("Runtime Services revision"))?
    );
    for (name, offset) in [
        (
            "GetVariable",
            mem::offset_of!(efi::RuntimeServices, get_variable),
        ),
        (
            "SetVariable",
            mem::offset_of!(efi::RuntimeServices, set_variable),
        ),
        (
            "ResetSystem",
            mem::offset_of!(efi::RuntimeServices, reset_system),
        ),
        (
            "SetVirtualAddressMap",
            mem::offset_of!(efi::RuntimeServices, set_virtual_address_map),
        ),
        (
            "ConvertPointer",
            mem::offset_of!(efi::RuntimeServices, convert_pointer),
        ),
    ] {
        let target = read_u64(bytes, offset).ok_or_else(|| malformed("Runtime Services entry"))?;
        if target != 0 && !map.readable(target, 1) {
            return Err(malformed("Runtime Services entry range"));
        }
        let _ = writeln!(
            serial,
            "thin-hv: preflight runtime_service={name} entry_present={}",
            u8::from(target != 0)
        );
    }
    Ok(())
}

/// Validates RSDP and root pointers, but reads only 36 bytes of each child table.
fn scan_acpi(address: u64, map: &MemoryMap<'_>) -> Result<bool, Error> {
    let prefix = map
        .firmware_bytes(address, 20)
        .ok_or_else(|| malformed("ACPI RSDP range"))?;
    if prefix.get(..8) != Some(&b"RSD PTR "[..]) || !checksum_valid(prefix) {
        return Err(malformed("ACPI RSDP signature/checksum"));
    }
    let (root_address, width) = if prefix[15] >= 2 {
        let rsdp = map
            .firmware_bytes(address, 36)
            .ok_or_else(|| malformed("ACPI extended RSDP range"))?;
        let length = read_u32(rsdp, 20).ok_or_else(|| malformed("ACPI RSDP length"))? as usize;
        if !(36..=4096).contains(&length)
            || !map
                .firmware_bytes(address, length)
                .is_some_and(checksum_valid)
        {
            return Err(malformed("ACPI RSDP extended checksum/length"));
        }
        let xsdt = read_u64(rsdp, 24).ok_or_else(|| malformed("ACPI XSDT pointer"))?;
        if xsdt != 0 {
            (xsdt, 8)
        } else {
            (
                u64::from(read_u32(prefix, 16).ok_or_else(|| malformed("ACPI RSDT pointer"))?),
                4,
            )
        }
    } else {
        (
            u64::from(read_u32(prefix, 16).ok_or_else(|| malformed("ACPI RSDT pointer"))?),
            4,
        )
    };
    let header = map
        .firmware_bytes(root_address, ACPI_HEADER_BYTES)
        .ok_or_else(|| malformed("ACPI root header range"))?;
    let length = table_length(header)?;
    let root = map
        .firmware_bytes(root_address, length)
        .ok_or_else(|| malformed("ACPI root table range"))?;
    scan_root(
        root,
        width,
        |address| {
            map.firmware_bytes(address, ACPI_HEADER_BYTES)?
                .try_into()
                .ok()
        },
        |address, length| map.readable(address, length),
    )
}

/// Parses root entries with a header-only reader, keeping MSDM payload opaque.
fn scan_root(
    root: &[u8],
    width: usize,
    mut read_header: impl FnMut(u64) -> Option<[u8; ACPI_HEADER_BYTES]>,
    range_readable: impl Fn(u64, usize) -> bool,
) -> Result<bool, Error> {
    if !matches!(width, 4 | 8)
        || root.len() < ACPI_HEADER_BYTES
        || table_length(root)? != root.len()
        || root.get(..4)
            != Some(if width == 8 {
                &b"XSDT"[..]
            } else {
                &b"RSDT"[..]
            })
        || !(root.len() - ACPI_HEADER_BYTES).is_multiple_of(width)
        || (root.len() - ACPI_HEADER_BYTES) / width > MAX_TABLE_ENTRIES
        || !checksum_valid(root)
    {
        return Err(malformed("ACPI root signature/length/checksum"));
    }
    let mut found = false;
    for offset in (ACPI_HEADER_BYTES..root.len()).step_by(width) {
        let address = if width == 8 {
            read_u64(root, offset)
        } else {
            read_u32(root, offset).map(u64::from)
        }
        .filter(|address| *address != 0)
        .ok_or_else(|| malformed("ACPI child pointer"))?;
        let header = read_header(address).ok_or_else(|| malformed("ACPI child header range"))?;
        let length = table_length(&header)?;
        if !range_readable(address, length) {
            return Err(malformed("ACPI child table range"));
        }
        found |= header[..4] == *b"MSDM";
        // Intentionally do not checksum/dump a child table: its payload can
        // contain the OEM Windows product key. Presence uses the header only.
    }
    Ok(found)
}

/// Checks a bounded standard ACPI header without reading the table payload.
fn table_length(header: &[u8]) -> Result<usize, Error> {
    let length = read_u32(header, 4).ok_or_else(|| malformed("ACPI table length"))? as usize;
    if header.len() < ACPI_HEADER_BYTES || !(ACPI_HEADER_BYTES..=MAX_ACPI_BYTES).contains(&length) {
        return Err(malformed("ACPI table length bound"));
    }
    Ok(length)
}

/// Applies the ACPI byte-sum checksum only to RSDP and pointer-only roots.
fn checksum_valid(bytes: &[u8]) -> bool {
    bytes.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(start: u64, pages: u64, kind: u32, attributes: u64) -> [u8; 48] {
        let mut bytes = [0; 48];
        bytes[..4].copy_from_slice(&kind.to_le_bytes());
        bytes[8..16].copy_from_slice(&start.to_le_bytes());
        bytes[24..32].copy_from_slice(&pages.to_le_bytes());
        bytes[32..40].copy_from_slice(&attributes.to_le_bytes());
        bytes
    }

    #[test]
    fn preflight_memory_map_rejects_malformed_ranges() {
        let valid = descriptor(0x1000, 2, efi::BOOT_SERVICES_DATA, efi::MEMORY_WB);
        assert!(MemoryMap::new(&valid, 48, 1, 36).is_ok());
        for (stride, version, bits) in [(0, 1, 36), (39, 1, 36), (48, 2, 36), (48, 1, 64)] {
            assert!(MemoryMap::new(&valid, stride, version, bits).is_err());
        }
        assert!(MemoryMap::new(&valid[..47], 48, 1, 36).is_err());
        assert!(MemoryMap::new(&[], 48, 1, 36).is_err());
        for entry in [
            descriptor(0x1001, 1, 4, 0),
            descriptor(0x1000, 0, 4, 0),
            descriptor(0x1000, u64::MAX, 4, 0),
            descriptor(1 << 36, 1, 4, 0),
            descriptor(0x1000, 1, 16, 0),
        ] {
            assert!(MemoryMap::new(&entry, 48, 1, 36).is_err());
        }
        let mut overlap = [0; 96];
        overlap[..48].copy_from_slice(&valid);
        overlap[48..].copy_from_slice(&descriptor(0x2000, 1, 4, 0));
        assert!(MemoryMap::new(&overlap, 48, 1, 36).is_err());
        let upper = descriptor((1 << 36) - PAGE_BYTES, 1, 4, 0);
        assert!(MemoryMap::new(&upper, 48, 1, 36).is_ok());
    }

    #[test]
    fn preflight_pointer_ranges_require_readable_contiguous_ram() {
        let mut bytes = [0; 144];
        bytes[..48].copy_from_slice(&descriptor(0x1000, 1, 4, 0));
        bytes[48..96].copy_from_slice(&descriptor(0x2000, 1, 9, 0));
        bytes[96..].copy_from_slice(&descriptor(0x3000, 1, efi::MEMORY_MAPPED_IO, 0));
        let map = MemoryMap::new(&bytes, 48, 1, 36).unwrap();
        assert!(map.readable(0x1ff0, 32));
        assert!(!map.readable(0x2ff0, 32));
        assert!(!map.readable(0, 1));
        assert!(!map.readable(0x1000, 0));
        assert!(!map.readable(u64::MAX, 2));
        bytes[48..96].copy_from_slice(&descriptor(0x2000, 1, 9, efi::MEMORY_RP));
        assert!(
            !MemoryMap::new(&bytes, 48, 1, 36)
                .unwrap()
                .readable(0x1ff0, 32)
        );
    }

    #[test]
    fn preflight_msdm_discovery_reads_headers_only() {
        let mut root = [0_u8; 44];
        root[..4].copy_from_slice(b"XSDT");
        root[4..8].copy_from_slice(&44_u32.to_le_bytes());
        root[36..].copy_from_slice(&0x1000_u64.to_le_bytes());
        root[9] = 0_u8.wrapping_sub(root.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)));
        let mut header = [0_u8; ACPI_HEADER_BYTES];
        header[..4].copy_from_slice(b"MSDM");
        header[4..8].copy_from_slice(&128_u32.to_le_bytes());
        let mut reads = 0;
        let found = scan_root(
            &root,
            8,
            |address| {
                assert_eq!(address, 0x1000);
                reads += 1;
                Some(header)
            },
            |address, length| address == 0x1000 && length == 128,
        )
        .unwrap();
        assert!(found);
        assert_eq!(reads, 1);
        assert!(scan_root(&root, 8, |_| Some(header), |_, _| false).is_err());
        assert!(scan_root(&root, 8, |_| None, |_, _| true).is_err());
        root[9] ^= 1;
        assert!(scan_root(&root, 8, |_| Some(header), |_, _| true).is_err());
    }

    #[test]
    fn preflight_cpu_family_model_uses_architectural_extension_rules() {
        assert_eq!(family_model(0x0003_06a9), (6, 0x3a));
        assert_eq!(family_model(0x0082_0f10), (23, 0x21));
        assert_eq!(family_model(0x00f2_0530), (5, 3));
    }
}
