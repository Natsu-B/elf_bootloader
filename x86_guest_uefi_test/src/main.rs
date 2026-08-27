//! Minimal UEFI payload for exercising a nested x86-64 guest.

#![no_main]
#![no_std]

use core::panic::PanicInfo;
use core::ptr;
use r_efi::efi;
use x86_64_hal::cpu;

/// Legacy COM1 base I/O port.
const COM1: u16 = 0x03f8;
/// UEFI global-variable namespace used by `BootOrder`.
const EFI_GLOBAL_VARIABLE_GUID: efi::Guid = efi::Guid::from_fields(
    0x8be4_df61,
    0x93ca,
    0x11d2,
    0xaa,
    0x0d,
    &[0x00, 0xe0, 0x98, 0x03, 0x2b, 0x8c],
);
/// Monitor-private namespace used for physical overlay keys.
const MONITOR_VENDOR_GUID: efi::Guid = efi::Guid::from_fields(
    0xd7e7_166a,
    0x574a,
    0x4c70,
    0xa3,
    0xd0,
    &[0x55, 0xd8, 0xd6, 0x6d, 0x3a, 0x42],
);
/// Logical boot-order variable exposed to the selected profile.
const BOOT_ORDER: [efi::Char16; 10] = ascii_uefi_name(b"BootOrder\0");
/// Physical Windows-profile backend variable.
const WINDOWS_BOOT_ORDER: [efi::Char16; 20] = ascii_uefi_name(b"P00000001:BootOrder\0");
/// Physical Linux-profile backend variable.
const LINUX_BOOT_ORDER: [efi::Char16; 20] = ascii_uefi_name(b"P00000002:BootOrder\0");

/// Converts one fixed ASCII fixture to a UEFI name.
const fn ascii_uefi_name<const N: usize>(ascii: &[u8; N]) -> [efi::Char16; N] {
    let mut name = [0; N];
    let mut index = 0;
    while index < N {
        name[index] = ascii[index] as efi::Char16;
        index += 1;
    }
    name
}

/// Configures COM1 for 115200 baud, 8-N-1 polling output.
fn init_serial() {
    // SAFETY: UEFI runs at CPL0 and the test payload owns COM1.
    unsafe {
        cpu::outb(COM1 + 1, 0x00);
        cpu::outb(COM1 + 3, 0x80);
        cpu::outb(COM1, 0x01);
        cpu::outb(COM1 + 1, 0x00);
        cpu::outb(COM1 + 3, 0x03);
        cpu::outb(COM1 + 2, 0xc7);
        cpu::outb(COM1 + 4, 0x0b);
    }
}

/// Writes one byte after waiting for the transmitter.
fn write_byte(byte: u8) {
    // SAFETY: UEFI runs at CPL0 and the test payload owns COM1.
    unsafe {
        while cpu::inb(COM1 + 5) & 0x20 == 0 {
            core::hint::spin_loop();
        }
        cpu::outb(COM1, byte);
    }
}

/// Writes one fixed byte string to COM1.
fn write_bytes(bytes: &[u8]) {
    for &byte in bytes {
        write_byte(byte);
    }
}

/// Exercises the installed Linux-profile variable hook through the UEFI ABI.
#[allow(clippy::too_many_lines)]
fn variable_overlay_round_trip(system_table: *mut efi::SystemTable) -> bool {
    if system_table.is_null() {
        return false;
    }
    // SAFETY: the firmware supplied the validated System Table to efi_main.
    let runtime = unsafe { (*system_table).runtime_services };
    if runtime.is_null() {
        return false;
    }
    let attributes = efi::VARIABLE_NON_VOLATILE
        | efi::VARIABLE_BOOTSERVICE_ACCESS
        | efi::VARIABLE_RUNTIME_ACCESS;
    let mut global_guid = EFI_GLOBAL_VARIABLE_GUID;
    let mut monitor_guid = MONITOR_VENDOR_GUID;
    let mut logical_name = BOOT_ORDER;
    let mut windows_name = WINDOWS_BOOT_ORDER;
    let mut linux_name = LINUX_BOOT_ORDER;

    // Start from deterministic backend state even if a developer reuses a
    // variable store instead of the smoke harness's fresh copy.
    for name in [&mut windows_name, &mut linux_name] {
        let mut guid = MONITOR_VENDOR_GUID;
        // SAFETY: name and GUID storage remain live for the firmware call.
        let status = unsafe {
            ((*runtime).set_variable)(name.as_mut_ptr(), &raw mut guid, 0, 0, ptr::null_mut())
        };
        if status != efi::Status::SUCCESS && status != efi::Status::NOT_FOUND {
            return false;
        }
    }

    let mut windows_value = 0x1111_u16;
    // SAFETY: all referenced values remain live through SetVariable.
    if unsafe {
        ((*runtime).set_variable)(
            windows_name.as_mut_ptr(),
            &raw mut monitor_guid,
            attributes,
            core::mem::size_of::<u16>(),
            ptr::addr_of_mut!(windows_value).cast(),
        )
    } != efi::Status::SUCCESS
    {
        return false;
    }

    let mut value = 0_u16;
    let mut value_size = core::mem::size_of::<u16>();
    // The other profile's physical key must not satisfy a logical read.
    if unsafe {
        ((*runtime).get_variable)(
            logical_name.as_mut_ptr(),
            &raw mut global_guid,
            ptr::null_mut(),
            &raw mut value_size,
            ptr::addr_of_mut!(value).cast(),
        )
    } != efi::Status::NOT_FOUND
    {
        return false;
    }

    let mut linux_value = 0x2222_u16;
    if unsafe {
        ((*runtime).set_variable)(
            logical_name.as_mut_ptr(),
            &raw mut global_guid,
            attributes,
            core::mem::size_of::<u16>(),
            ptr::addr_of_mut!(linux_value).cast(),
        )
    } != efi::Status::SUCCESS
    {
        return false;
    }

    value = 0;
    value_size = core::mem::size_of::<u16>();
    let mut returned_attributes = 0;
    if unsafe {
        ((*runtime).get_variable)(
            logical_name.as_mut_ptr(),
            &raw mut global_guid,
            &raw mut returned_attributes,
            &raw mut value_size,
            ptr::addr_of_mut!(value).cast(),
        )
    } != efi::Status::SUCCESS
        || value != linux_value
        || value_size != core::mem::size_of::<u16>()
        || returned_attributes != attributes
    {
        return false;
    }

    // A direct backend read proves the logical write used profile 2.
    value = 0;
    value_size = core::mem::size_of::<u16>();
    monitor_guid = MONITOR_VENDOR_GUID;
    if unsafe {
        ((*runtime).get_variable)(
            linux_name.as_mut_ptr(),
            &raw mut monitor_guid,
            ptr::null_mut(),
            &raw mut value_size,
            ptr::addr_of_mut!(value).cast(),
        )
    } != efi::Status::SUCCESS
        || value != linux_value
    {
        return false;
    }

    // Writing profile 2 must leave profile 1's persistent backend intact.
    value = 0;
    value_size = core::mem::size_of::<u16>();
    monitor_guid = MONITOR_VENDOR_GUID;
    if unsafe {
        ((*runtime).get_variable)(
            windows_name.as_mut_ptr(),
            &raw mut monitor_guid,
            ptr::null_mut(),
            &raw mut value_size,
            ptr::addr_of_mut!(value).cast(),
        )
    } != efi::Status::SUCCESS
        || value != windows_value
    {
        return false;
    }

    if !enumeration_is_logical(runtime) {
        return false;
    }

    let mut maximum_storage = 0;
    let mut remaining_storage = 0;
    let mut maximum_variable = 0;
    if unsafe {
        ((*runtime).query_variable_info)(
            attributes,
            &raw mut maximum_storage,
            &raw mut remaining_storage,
            &raw mut maximum_variable,
        )
    } != efi::Status::SUCCESS
    {
        return false;
    }

    runtime_table_crc_is_valid(system_table)
}

/// Recomputes the live Runtime Services table CRC without mutating firmware.
fn runtime_table_crc_is_valid(system_table: *mut efi::SystemTable) -> bool {
    // SAFETY: this payload runs before ExitBootServices with firmware-owned
    // System, Runtime Services, and Boot Services tables still live.
    let (runtime, boot) = unsafe {
        (
            (*system_table).runtime_services,
            (*system_table).boot_services,
        )
    };
    if runtime.is_null() || boot.is_null() {
        return false;
    }
    // This focused OVMF check intentionally has no dynamic scratch fallback.
    let header_size = unsafe { (*runtime).hdr.header_size as usize };
    if header_size != core::mem::size_of::<efi::RuntimeServices>() {
        return false;
    }
    let mut copy = core::mem::MaybeUninit::<efi::RuntimeServices>::uninit();
    // SAFETY: source and destination are valid for exactly one table value.
    unsafe { ptr::copy_nonoverlapping(runtime, copy.as_mut_ptr(), 1) };
    // SAFETY: every byte of the copy was initialized above.
    let mut copy = unsafe { copy.assume_init() };
    let expected = copy.hdr.crc32;
    copy.hdr.crc32 = 0;
    let mut calculated = 0;
    // SAFETY: CalculateCrc32 receives an aligned private copy of the table.
    let status = unsafe {
        ((*boot).calculate_crc32)(
            ptr::addr_of_mut!(copy).cast(),
            header_size,
            &raw mut calculated,
        )
    };
    status == efi::Status::SUCCESS && calculated == expected
}

/// Confirms enumeration exposes one logical key and no physical profile key.
fn enumeration_is_logical(runtime: *mut efi::RuntimeServices) -> bool {
    let mut name = [0; 256];
    let mut guid = efi::Guid::from_fields(0, 0, 0, 0, 0, &[0; 6]);
    let mut logical_count = 0;
    for _ in 0..512 {
        let mut name_size = core::mem::size_of_val(&name);
        // SAFETY: fixed name/GUID buffers remain valid for the call.
        let status = unsafe {
            ((*runtime).get_next_variable_name)(
                &raw mut name_size,
                name.as_mut_ptr(),
                &raw mut guid,
            )
        };
        if status == efi::Status::NOT_FOUND {
            return logical_count == 1;
        }
        if status != efi::Status::SUCCESS {
            return false;
        }
        let Some(length) = name.iter().position(|&unit| unit == 0) else {
            return false;
        };
        if guid == MONITOR_VENDOR_GUID
            && (name[..length] == WINDOWS_BOOT_ORDER[..WINDOWS_BOOT_ORDER.len() - 1]
                || name[..length] == LINUX_BOOT_ORDER[..LINUX_BOOT_ORDER.len() - 1])
        {
            return false;
        }
        if guid == EFI_GLOBAL_VARIABLE_GUID && name[..length] == BOOT_ORDER[..BOOT_ORDER.len() - 1]
        {
            logical_count += 1;
        }
    }
    false
}

/// UEFI image entry point.
#[unsafe(no_mangle)]
pub extern "efiapi" fn efi_main(
    _image: efi::Handle,
    system_table: *mut efi::SystemTable,
) -> efi::Status {
    init_serial();
    for byte in b"thin-hv: guest uefi payload\r\n" {
        write_byte(*byte);
    }
    let leaf_one = cpu::cpuid(1, 0);
    let vmx = u8::from(leaf_one.ecx & (1 << 5) != 0);
    let hypervisor = u8::from(leaf_one.ecx & (1 << 31) != 0);
    for byte in b"thin-hv: guest cpuid vmx=" {
        write_byte(*byte);
    }
    write_byte(b'0' + vmx);
    for byte in b" hypervisor=" {
        write_byte(*byte);
    }
    write_byte(b'0' + hypervisor);
    write_byte(b'\r');
    write_byte(b'\n');

    let variables_ok = variable_overlay_round_trip(system_table);
    if variables_ok {
        write_bytes(b"thin-hv: uefi variable overlay PASS\r\n");
    } else {
        write_bytes(b"thin-hv: uefi variable overlay FAIL\r\n");
    }

    let hypervisor_leaf = cpu::cpuid(0x4000_0000, 0);
    if vmx == 1
        && hypervisor == 0
        && hypervisor_leaf.eax == 0
        && hypervisor_leaf.ebx == 0
        && hypervisor_leaf.ecx == 0
        && hypervisor_leaf.edx == 0
        && variables_ok
    {
        efi::Status::SUCCESS
    } else {
        efi::Status::DEVICE_ERROR
    }
}

/// Stops the payload if an unexpected panic occurs.
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
