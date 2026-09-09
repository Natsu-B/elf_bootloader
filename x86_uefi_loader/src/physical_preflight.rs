//! Read-only machine inventory before any physical Direct-VMX launch.

use crate::SerialPort;
use crate::chainload::Error;
use crate::platform_acpi;
use crate::platform_ept_audit;
use crate::platform_resources;
use crate::platform_snapshot;
use crate::platform_snapshot::MemoryMap;
use crate::platform_snapshot::malformed;
use crate::platform_snapshot::read_u32;
use crate::platform_snapshot::read_u64;
use core::fmt::Write;
use core::mem;
use r_efi::efi;
use x86_64_hal::platform_memory::PhysicalWidth;

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
            let tables = inventory_tables(system_table, map, serial)?;
            let width = PhysicalWidth::new(cpu.physical_bits())
                .map_err(|_| malformed("GCD physical-address width"))?;
            let mmio = platform_resources::platform_mmio(
                system_table,
                map,
                width,
                tables.as_ref(),
                serial,
            )?;
            storage.inspect(cpu, map, mmio.ranges(), serial)
        })
    })
}

/// Logs only table presence/addresses; never firmware identity or key content.
fn inventory_tables(
    system_table: *mut efi::SystemTable,
    map: &MemoryMap<'_>,
    serial: &mut SerialPort,
) -> Result<Option<platform_acpi::Tables>, Error> {
    let system = map.system_table(system_table)?;
    inventory_runtime(system.runtime_services, map, serial)?;
    let entries = map.configuration_tables(system_table)?;
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
    let tables = if let Some(tables) = platform_acpi::Tables::from_system_table(system_table, map)?
    {
        let _ = writeln!(
            serial,
            "thin-hv: preflight MSDM={} payload=not-read",
            if tables.msdm { "present" } else { "absent" }
        );
        Some(tables)
    } else {
        let _ = writeln!(
            serial,
            "thin-hv: preflight MSDM=unavailable reason=no-acpi-system-table payload=not-read"
        );
        None
    };
    Ok(tables)
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
