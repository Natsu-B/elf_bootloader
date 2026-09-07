//! Read-only machine inventory before any physical Direct-VMX launch.

use crate::SerialPort;
use crate::chainload::Error;
use crate::platform_ept_audit;
use crate::platform_snapshot;
use crate::platform_snapshot::MemoryMap;
use crate::platform_snapshot::malformed;
use crate::platform_snapshot::read_u32;
use crate::platform_snapshot::read_u64;
use core::fmt::Write;
use core::mem;
use core::slice;
use r_efi::efi;

/// Table inventory bounds; exceeding them is an explicit unsupported layout.
const MAX_TABLE_ENTRIES: usize = 4096;
const MAX_ACPI_BYTES: usize = 1024 * 1024;
const ACPI_HEADER_BYTES: usize = 36;

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

/// Captures once after reserving the temporary EPT-audit storage.
fn inventory(system_table: *mut efi::SystemTable, serial: &mut SerialPort) -> Result<(), Error> {
    platform_ept_audit::with_storage(system_table, |storage| {
        platform_snapshot::with_snapshot(system_table, serial, |cpu, map, serial| {
            let _ = writeln!(
                serial,
                "thin-hv: preflight memory_map descriptors={} stride={} version={}",
                map.count(),
                map.stride(),
                map.version()
            );
            for index in 0..map.count() {
                let region = map
                    .region(index)
                    .ok_or_else(|| malformed("memory descriptor"))?;
                let _ = writeln!(
                    serial,
                    "thin-hv: preflight memory index={index} type={} physical={:#018x} virtual={:#018x} pages={} attributes={:#018x}",
                    region.kind,
                    region.start,
                    region.virtual_start,
                    region.pages,
                    region.attributes
                );
            }
            inventory_tables(system_table, map, serial)?;
            storage.inspect(cpu, map, serial)
        })
    })
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
}
