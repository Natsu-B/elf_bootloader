//! Bounded shared CPU and UEFI memory-map capture before ExitBootServices.

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
use x86_64_hal::platform_memory;
use x86_64_hal::platform_memory::Mtrrs;
use x86_64_hal::platform_memory::PhysicalWidth;
use x86_64_hal::platform_memory::VariableMtrr;
use x86_64_hal::vmx;

/// Firmware inventory bounds; exceeding them is an explicit unsupported layout.
const MAX_MAP_BYTES: usize = 1024 * 1024;
const MAX_DESCRIPTORS: usize = 4096;
const MAX_VARIABLE_MTRRS: usize = 64;
const PAGE_BYTES: u64 = 4096;

/// Returns a malformed-platform error without exposing firmware contents.
pub(crate) fn malformed(context: &'static str) -> Error {
    Error::Firmware(context, efi::Status::COMPROMISED_DATA.as_usize())
}

/// CPU state needed by the current map audit, captured before ExitBootServices.
///
/// Optional fields are absent unless the specific architectural capability gate
/// advertised them. Inventory deliberately does not validate MTRR memory types;
/// consumers request that stronger HAL policy validation through `mtrrs()`.
pub(crate) struct CpuSnapshot {
    physical_bits: u8,
    cr4: u64,
    ept_caps: Option<u64>,
    mtrr: Option<MtrrSnapshot>,
}

impl CpuSnapshot {
    /// Returns the validated CPUID physical-address width, not an EPT GPA limit.
    pub(crate) fn physical_bits(&self) -> u8 {
        self.physical_bits
    }

    /// Checks numerical bounds using captured CR4 and physical width only.
    ///
    /// This does not prove ownership or page-table presence: the caller must
    /// also establish that firmware supplied a live identity-addressed range.
    pub(crate) fn identity_range_supported(&self, start: u64, length: usize) -> bool {
        let canonical_limit = 1_u64 << if self.cr4 & (1 << 12) == 0 { 47 } else { 56 };
        start != 0
            && length != 0
            && length <= isize::MAX as usize
            && start
                .checked_add(length as u64)
                .is_some_and(|end| end <= canonical_limit && end <= 1_u64 << self.physical_bits)
    }

    /// Returns the captured EPT/VPID capability only when EPT itself is allowed.
    pub(crate) fn ept_caps(&self) -> Option<u64> {
        self.ept_caps
    }

    /// Borrows a validated HAL MTRR policy without any further CPU/MSR reads.
    ///
    /// A missing CPUID.MTRR capability is not treated as synthetic WB memory.
    /// Malformed advertised state remains an error for a mapping consumer.
    pub(crate) fn mtrrs(&self) -> Result<Option<Mtrrs<'_>>, platform_memory::Error> {
        self.mtrr
            .as_ref()
            .map(|raw| raw.policy(PhysicalWidth::new(self.physical_bits)?))
            .transpose()
    }
}

/// VPID alone permits the capability MSR read but does not permit an EPT audit.
fn supported_ept_capability(secondary_controls: u64, capability: Option<u64>) -> Option<u64> {
    capability.filter(|_| secondary_controls & (1_u64 << 33) != 0)
}

/// Bounded inline raw MTRR storage; unused array elements are never policy input.
struct MtrrSnapshot {
    capability: u64,
    default_type: u64,
    fixed: Option<[u64; 11]>,
    variable: [VariableMtrr; MAX_VARIABLE_MTRRS],
    count: u8,
}

impl MtrrSnapshot {
    /// Preserves the captured count and register ordering for HAL validation.
    fn policy(&self, width: PhysicalWidth) -> Result<Mtrrs<'_>, platform_memory::Error> {
        let variable = self
            .variable
            .get(..usize::from(self.count))
            .ok_or(platform_memory::Error::MtrrState)?;
        Mtrrs::new(
            width,
            self.capability,
            self.default_type,
            self.fixed.as_ref(),
            variable,
        )
    }
}

/// Borrows one CPU/map capture while its temporary firmware allocation is live.
///
/// The callback must run before ExitBootServices and must not retain pointers
/// into the map. Allocate any mapping arena before this call: later allocation
/// changes the platform map. Neither an EPTP nor a VM entry is published here.
/// The callback cannot return a borrow of the local CPU/map owner. Cleanup errors
/// take precedence over callback errors, matching the diagnostic's prior policy.
/// Inventory logging is deliberately cold and never used on a VM-exit path.
pub(crate) fn with_snapshot<T>(
    system_table: *mut efi::SystemTable,
    serial: &mut SerialPort,
    inspect: impl FnOnce(&CpuSnapshot, &MemoryMap<'_>, &mut SerialPort) -> Result<T, Error>,
) -> Result<T, Error> {
    let boot_services = chainload::boot_services(system_table)?;
    let cpu = capture_cpu(serial)?;
    let buffer = acquire_memory_map(boot_services)?;
    let result = (|| {
        // SAFETY: GetMemoryMap initialized this bounded prefix of our exclusive,
        // zero-initialized pool allocation, retained until after this closure.
        let bytes = unsafe { slice::from_raw_parts(buffer.pointer.cast::<u8>(), buffer.size) };
        let mut map = MemoryMap::new(bytes, buffer.stride, buffer.version, cpu.physical_bits())?;
        // Reuse the captured paging mode; never reread CR4 for table discovery.
        map.linear_address_limit = 1_u64 << if cpu.cr4 & (1 << 12) == 0 { 47 } else { 56 };
        inspect(&cpu, &map, serial)
    })();
    let release = chainload::free_pool(boot_services, buffer.pointer);
    // A release failure is itself actionable, even when inspection also failed.
    release?;
    result
}

/// Captures each advertised architectural register once without enabling it.
fn capture_cpu(serial: &mut SerialPort) -> Result<CpuSnapshot, Error> {
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
    let ept_caps = if vmx_present {
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
            let capability = if (secondary >> 32) & ((1 << 1) | (1 << 5)) != 0 {
                // SAFETY: Secondary allowed-one EPT or VPID advertises this MSR.
                Some(unsafe {
                    log_msr(serial, "IA32_VMX_EPT_VPID_CAP", vmx::IA32_VMX_EPT_VPID_CAP)
                })
            } else {
                let _ = writeln!(
                    serial,
                    "thin-hv: preflight IA32_VMX_EPT_VPID_CAP=unavailable"
                );
                None
            };
            supported_ept_capability(secondary, capability)
        } else {
            let _ = writeln!(
                serial,
                "thin-hv: preflight VMX_SECONDARY_CONTROLS=unavailable IA32_VMX_EPT_VPID_CAP=unavailable"
            );
            None
        }
    } else {
        let _ = writeln!(
            serial,
            "thin-hv: preflight IA32_FEATURE_CONTROL=unavailable IA32_VMX_BASIC=unavailable VMX_CONTROLS=unavailable IA32_VMX_EPT_VPID_CAP=unavailable"
        );
        None
    };
    let cr0 = cpu::read_cr0();
    let cr3 = cpu::read_cr3();
    let cr4 = cpu::read_cr4();
    let _ = writeln!(
        serial,
        "thin-hv: preflight CR0={:#018x} CR3={:#018x} CR4={:#018x}",
        cr0, cr3, cr4
    );
    if features.edx & (1 << 16) != 0 {
        // SAFETY: CPUID.PAT advertises IA32_PAT; RDMSR does not change it.
        unsafe { log_msr(serial, "IA32_PAT", cpu::IA32_PAT) };
    } else {
        let _ = writeln!(serial, "thin-hv: preflight IA32_PAT=unavailable");
    }
    let mtrr = if features.edx & (1 << 12) != 0 {
        // SAFETY: CPUID.MTRR advertises MTRRCAP and MTRR_DEF_TYPE.
        let (capability, default_type) = unsafe {
            (
                log_msr(serial, "IA32_MTRRCAP", 0xfe),
                log_msr(serial, "IA32_MTRR_DEF_TYPE", 0x2ff),
            )
        };
        let count = capability as u8;
        if usize::from(count) > MAX_VARIABLE_MTRRS {
            return Err(Error::Firmware(
                "MTRR variable count exceeds inventory bound",
                efi::Status::UNSUPPORTED.as_usize(),
            ));
        }
        let mut variable = [VariableMtrr { base: 0, mask: 0 }; MAX_VARIABLE_MTRRS];
        for (index, pair) in variable[..usize::from(count)].iter_mut().enumerate() {
            let index = index as u32;
            // SAFETY: MTRRCAP.VCNT advertises each bounded base/mask pair.
            *pair = unsafe {
                VariableMtrr {
                    base: log_msr(serial, "IA32_MTRR_PHYSBASE", 0x200 + index * 2),
                    mask: log_msr(serial, "IA32_MTRR_PHYSMASK", 0x201 + index * 2),
                }
            };
        }
        let fixed = if capability & (1 << 8) != 0 {
            let mut values = [0; 11];
            // SAFETY: MTRRCAP.FIX advertises all eleven architectural fixed MSRs.
            unsafe {
                for (value, index) in values.iter_mut().zip([
                    0x250, 0x258, 0x259, 0x268, 0x269, 0x26a, 0x26b, 0x26c, 0x26d, 0x26e, 0x26f,
                ]) {
                    *value = log_msr(serial, "IA32_MTRR_FIXED", index);
                }
            }
            Some(values)
        } else {
            None
        };
        Some(MtrrSnapshot {
            capability,
            default_type,
            fixed,
            variable,
            count,
        })
    } else {
        let _ = writeln!(serial, "thin-hv: preflight MTRR=unavailable");
        None
    };
    Ok(CpuSnapshot {
        physical_bits,
        cr4,
        ept_caps,
        mtrr,
    })
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
pub(crate) struct MemoryRegion {
    pub(crate) kind: u32,
    pub(crate) start: u64,
    pub(crate) virtual_start: u64,
    pub(crate) pages: u64,
    pub(crate) attributes: u64,
}

impl MemoryRegion {
    /// Computes the exclusive physical end without overflowing.
    pub(crate) fn end(self) -> Option<u64> {
        self.start.checked_add(self.pages.checked_mul(PAGE_BYTES)?)
    }

    /// Only CPU-readable RAM types may back dereferenced firmware tables.
    fn readable(self) -> bool {
        matches!(self.kind, 1..=7 | 9 | 10 | 14) && self.attributes & efi::MEMORY_RP == 0
    }
}

/// Validated borrowed UEFI memory map, preserving firmware descriptor order.
pub(crate) struct MemoryMap<'a> {
    bytes: &'a [u8],
    stride: usize,
    version: u32,
    // The production capture replaces this with its captured CR4.LA57 limit.
    linear_address_limit: u64,
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
        let map = Self {
            bytes,
            stride,
            version,
            linear_address_limit: 1_u64 << 47,
        };
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

    /// Borrows the exact validated firmware descriptor bytes, including padding.
    pub(crate) fn bytes(&self) -> &[u8] {
        self.bytes
    }

    /// Returns the firmware stride, including any supported extension bytes.
    pub(crate) fn stride(&self) -> usize {
        self.stride
    }

    /// Returns the already validated EFI memory-descriptor version.
    pub(crate) fn version(&self) -> u32 {
        self.version
    }

    /// Returns the validated descriptor count.
    pub(crate) fn count(&self) -> usize {
        self.bytes.len() / self.stride
    }

    /// Copies only the standard descriptor prefix, ignoring extension bytes.
    pub(crate) fn region(&self, index: usize) -> Option<MemoryRegion> {
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
    pub(crate) fn readable(&self, address: u64, length: usize) -> bool {
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
    pub(crate) fn firmware_bytes(&self, address: u64, length: usize) -> Option<&[u8]> {
        if !self.readable(address, length)
            || address.checked_add(length as u64)? > self.linear_address_limit
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
pub(crate) fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
/// Reads one bounded little-endian 64-bit field.
pub(crate) fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
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
    fn snapshot_map_preserves_raw_stride_version_and_extension_bytes() {
        let mut bytes = descriptor(
            0x1000,
            1,
            efi::RUNTIME_SERVICES_DATA,
            efi::MEMORY_WB | efi::MEMORY_RUNTIME,
        );
        bytes[40..].fill(0xa5);
        let map = MemoryMap::new(&bytes, 48, 1, 36).unwrap();
        assert_eq!(map.bytes(), &bytes);
        assert_eq!(map.stride(), 48);
        assert_eq!(map.version(), efi::MEMORY_DESCRIPTOR_VERSION);
        assert_eq!(map.count(), 1);
        let region = map.region(0).unwrap();
        assert_eq!(region.kind, efi::RUNTIME_SERVICES_DATA);
        assert_eq!(region.attributes, efi::MEMORY_WB | efi::MEMORY_RUNTIME);
        assert!(map.region(1).is_none());
        assert!(map.region(usize::MAX).is_none());
    }

    #[test]
    fn snapshot_mtrr_policy_borrows_only_captured_variable_pairs() {
        let width = PhysicalWidth::new(36).unwrap();
        let mut raw = MtrrSnapshot {
            capability: 1,
            default_type: 0x806,
            fixed: None,
            variable: [VariableMtrr { base: 0, mask: 0 }; MAX_VARIABLE_MTRRS],
            count: 1,
        };
        // Deliberately invalid unused storage must not become a captured pair.
        raw.variable[MAX_VARIABLE_MTRRS - 1] = VariableMtrr {
            base: u64::MAX,
            mask: u64::MAX,
        };
        assert!(raw.policy(width).is_ok());
        raw.capability = 2;
        assert!(raw.policy(width).is_err());
        raw.count = 65;
        assert!(raw.policy(width).is_err());
        raw.capability = 1;
        raw.count = 1;
        raw.default_type = 0xc06;
        assert!(raw.policy(width).is_err());
    }

    #[test]
    fn snapshot_identity_range_checks_captured_canonical_and_physical_boundaries() {
        let mut cpu = CpuSnapshot {
            physical_bits: 52,
            cr4: 0,
            ept_caps: None,
            mtrr: None,
        };
        assert!(cpu.identity_range_supported((1 << 47) - 4096, 4096));
        assert!(!cpu.identity_range_supported((1 << 47) - 4096, 4097));
        assert!(!cpu.identity_range_supported(1 << 47, 1));
        assert!(!cpu.identity_range_supported(u64::MAX, 2));
        assert!(!cpu.identity_range_supported(0, 4096));
        assert!(!cpu.identity_range_supported(4096, 0));
        assert!(!cpu.identity_range_supported(4096, usize::MAX));
        cpu.cr4 = 1 << 12;
        assert!(cpu.identity_range_supported(1 << 47, 4096));
        assert!(cpu.identity_range_supported((1 << 52) - 4096, 4096));
        assert!(!cpu.identity_range_supported((1 << 52) - 4096, 4097));
        cpu.physical_bits = 36;
        assert!(cpu.identity_range_supported((1 << 36) - 4096, 4096));
        assert!(!cpu.identity_range_supported((1 << 36) - 4096, 4097));
    }

    #[test]
    fn snapshot_ept_capability_requires_ept_not_only_vpid() {
        let mut cpu = CpuSnapshot {
            physical_bits: 36,
            cr4: 0,
            ept_caps: None,
            mtrr: None,
        };
        assert_eq!(cpu.ept_caps(), None);
        assert!(cpu.mtrrs().unwrap().is_none());
        cpu.ept_caps = supported_ept_capability(1 << 37, Some(0x1234));
        assert_eq!(cpu.ept_caps(), None);
        cpu.ept_caps = supported_ept_capability(1 << 33, Some(0x1234));
        assert_eq!(cpu.ept_caps(), Some(0x1234));
        cpu.ept_caps = supported_ept_capability(1 << 33, None);
        assert_eq!(cpu.ept_caps(), None);
        cpu.ept_caps = supported_ept_capability(0, Some(0x1234));
        assert_eq!(cpu.ept_caps(), None);
    }

    #[test]
    fn preflight_cpu_family_model_uses_architectural_extension_rules() {
        assert_eq!(family_model(0x0003_06a9), (6, 0x3a));
        assert_eq!(family_model(0x0082_0f10), (23, 0x21));
        assert_eq!(family_model(0x00f2_0530), (5, 3));
    }
}
