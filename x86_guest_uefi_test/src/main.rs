//! Minimal UEFI payload for exercising a nested x86-64 guest.

#![no_main]
#![no_std]

use core::panic::PanicInfo;
use core::ptr;
use r_efi::efi;
use x86_64_hal::cpu;

/// Legacy COM1 base I/O port.
const COM1: u16 = 0x03f8;
/// UEFI global-variable namespace used by the temporary driver key.
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
/// Logical scratch variable exposed to the selected profile.
const TEST_DRIVER: [efi::Char16; 11] = ascii_uefi_name(b"DriverFFFF\0");
/// Physical Windows-profile scratch variable.
const WINDOWS_TEST_DRIVER: [efi::Char16; 21] = ascii_uefi_name(b"P00000001:DriverFFFF\0");
/// Physical Linux-profile scratch variable.
const LINUX_TEST_DRIVER: [efi::Char16; 21] = ascii_uefi_name(b"P00000002:DriverFFFF\0");
/// Inactive EFI_LOAD_OPTION with an empty description and end-only device path.
const INACTIVE_DRIVER_LOAD_OPTION: [u8; 12] = [0, 0, 0, 0, 4, 0, 0, 0, 0x7f, 0xff, 4, 0];

/// Variable-service backend observed by the smoke payload.
#[derive(Clone, Copy)]
enum VariableBackend {
    /// The monitor's live profile overlay maps logical driver state.
    Overlay,
    /// Native OVMF stores the per-VM scratch variable directly.
    Native,
}

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

/// Exercises either the live profile overlay or native per-VM OVMF storage.
fn variable_services_round_trip(
    system_table: *mut efi::SystemTable,
) -> (Option<VariableBackend>, bool) {
    if system_table.is_null() {
        return (None, false);
    }
    // SAFETY: the firmware supplied the validated System Table to efi_main.
    let runtime = unsafe { (*system_table).runtime_services };
    if runtime.is_null() || !runtime_table_crc_is_valid(system_table) {
        return (None, false);
    }
    let attributes = efi::VARIABLE_NON_VOLATILE
        | efi::VARIABLE_BOOTSERVICE_ACCESS
        | efi::VARIABLE_RUNTIME_ACCESS;
    let mut logical_name = TEST_DRIVER;
    let mut windows_name = WINDOWS_TEST_DRIVER;
    let mut linux_name = LINUX_TEST_DRIVER;

    // Refuse to overwrite a developer's retained state. Both backends reserve
    // these disposable scratch names for the duration of the smoke test.
    if !variable_is_absent(runtime, &mut logical_name, EFI_GLOBAL_VARIABLE_GUID)
        || !variable_is_absent(runtime, &mut windows_name, MONITOR_VENDOR_GUID)
        || !variable_is_absent(runtime, &mut linux_name, MONITOR_VENDOR_GUID)
    {
        return (None, false);
    }

    let seed = 0x2222;
    if !set_u16_variable(
        runtime,
        &mut linux_name,
        MONITOR_VENDOR_GUID,
        attributes,
        seed,
    ) || read_u16_variable(runtime, &mut linux_name, MONITOR_VENDOR_GUID)
        != Ok((seed, attributes))
    {
        let _ = cleanup_test_variables(runtime);
        return (None, false);
    }

    let backend = match read_u16_variable(runtime, &mut logical_name, EFI_GLOBAL_VARIABLE_GUID) {
        Ok((value, returned_attributes)) if value == seed && returned_attributes == attributes => {
            VariableBackend::Overlay
        }
        Err(status) if status == efi::Status::NOT_FOUND => VariableBackend::Native,
        _ => {
            let _ = cleanup_test_variables(runtime);
            return (None, false);
        }
    };

    let backend_ok = match backend {
        VariableBackend::Overlay => overlay_round_trip(runtime, attributes),
        VariableBackend::Native => native_round_trip(runtime, attributes),
    };
    let variable_info_ok = query_variable_info_is_valid(runtime, attributes);
    let cleanup_ok = cleanup_test_variables(runtime);
    let crc_ok = runtime_table_crc_is_valid(system_table);
    (
        Some(backend),
        backend_ok && variable_info_ok && cleanup_ok && crc_ok,
    )
}

/// Exercises native OVMF policy with a well-formed disposable Driver#### value.
fn native_round_trip(runtime: *mut efi::RuntimeServices, attributes: u32) -> bool {
    let mut name = TEST_DRIVER;
    let mut guid = EFI_GLOBAL_VARIABLE_GUID;
    let mut returned = [0; INACTIVE_DRIVER_LOAD_OPTION.len()];
    let mut returned_size = returned.len();
    let mut returned_attributes = 0;
    // SAFETY: every input and output buffer remains live for the firmware calls.
    let set_status = unsafe {
        ((*runtime).set_variable)(
            name.as_mut_ptr(),
            &raw mut guid,
            attributes,
            INACTIVE_DRIVER_LOAD_OPTION.len(),
            INACTIVE_DRIVER_LOAD_OPTION.as_ptr().cast_mut().cast(),
        )
    };
    // SAFETY: every input and output buffer remains live for the firmware calls.
    let get_status = unsafe {
        ((*runtime).get_variable)(
            name.as_mut_ptr(),
            &raw mut guid,
            &raw mut returned_attributes,
            &raw mut returned_size,
            returned.as_mut_ptr().cast(),
        )
    };
    set_status == efi::Status::SUCCESS
        && get_status == efi::Status::SUCCESS
        && returned_size == returned.len()
        && returned_attributes == attributes
        && returned == INACTIVE_DRIVER_LOAD_OPTION
}

/// Completes the profile-isolation checks after the Linux key detects hooks.
fn overlay_round_trip(runtime: *mut efi::RuntimeServices, attributes: u32) -> bool {
    let mut logical_name = TEST_DRIVER;
    let mut windows_name = WINDOWS_TEST_DRIVER;
    let mut linux_name = LINUX_TEST_DRIVER;
    let windows_value = 0x1111;
    let linux_value = 0x3333;

    set_u16_variable(
        runtime,
        &mut windows_name,
        MONITOR_VENDOR_GUID,
        attributes,
        windows_value,
    ) && set_u16_variable(
        runtime,
        &mut logical_name,
        EFI_GLOBAL_VARIABLE_GUID,
        attributes,
        linux_value,
    ) && read_u16_variable(runtime, &mut logical_name, EFI_GLOBAL_VARIABLE_GUID)
        == Ok((linux_value, attributes))
        && read_u16_variable(runtime, &mut linux_name, MONITOR_VENDOR_GUID)
            == Ok((linux_value, attributes))
        && read_u16_variable(runtime, &mut windows_name, MONITOR_VENDOR_GUID)
            == Ok((windows_value, attributes))
        && enumeration_is_logical(runtime)
}

/// Reads one exact two-byte variable and its attributes.
fn read_u16_variable(
    runtime: *mut efi::RuntimeServices,
    name: &mut [efi::Char16],
    mut guid: efi::Guid,
) -> Result<(u16, u32), efi::Status> {
    let mut value = 0;
    let mut value_size = core::mem::size_of::<u16>();
    let mut attributes = 0;
    // SAFETY: every buffer remains live and writable for the firmware call.
    let status = unsafe {
        ((*runtime).get_variable)(
            name.as_mut_ptr(),
            &raw mut guid,
            &raw mut attributes,
            &raw mut value_size,
            ptr::addr_of_mut!(value).cast(),
        )
    };
    if status != efi::Status::SUCCESS {
        Err(status)
    } else if value_size != core::mem::size_of::<u16>() {
        Err(efi::Status::DEVICE_ERROR)
    } else {
        Ok((value, attributes))
    }
}

/// Returns whether one variable is absent without changing it.
fn variable_is_absent(
    runtime: *mut efi::RuntimeServices,
    name: &mut [efi::Char16],
    guid: efi::Guid,
) -> bool {
    matches!(
        read_u16_variable(runtime, name, guid),
        Err(status) if status == efi::Status::NOT_FOUND
    )
}

/// Writes one two-byte scratch variable.
fn set_u16_variable(
    runtime: *mut efi::RuntimeServices,
    name: &mut [efi::Char16],
    mut guid: efi::Guid,
    attributes: u32,
    mut value: u16,
) -> bool {
    // SAFETY: every buffer remains live for the firmware call.
    (unsafe {
        ((*runtime).set_variable)(
            name.as_mut_ptr(),
            &raw mut guid,
            attributes,
            core::mem::size_of::<u16>(),
            ptr::addr_of_mut!(value).cast(),
        )
    }) == efi::Status::SUCCESS
}

/// Deletes one scratch variable if present.
fn delete_variable(
    runtime: *mut efi::RuntimeServices,
    name: &mut [efi::Char16],
    mut guid: efi::Guid,
) -> bool {
    // SAFETY: name and GUID storage remain live for the firmware call.
    let status = unsafe {
        ((*runtime).set_variable)(name.as_mut_ptr(), &raw mut guid, 0, 0, ptr::null_mut())
    };
    status == efi::Status::SUCCESS || status == efi::Status::NOT_FOUND
}

/// Removes every scratch key and verifies that no logical key remains.
fn cleanup_test_variables(runtime: *mut efi::RuntimeServices) -> bool {
    let mut logical_name = TEST_DRIVER;
    let mut windows_name = WINDOWS_TEST_DRIVER;
    let mut linux_name = LINUX_TEST_DRIVER;
    let logical_deleted = delete_variable(runtime, &mut logical_name, EFI_GLOBAL_VARIABLE_GUID);
    let windows_deleted = delete_variable(runtime, &mut windows_name, MONITOR_VENDOR_GUID);
    let linux_deleted = delete_variable(runtime, &mut linux_name, MONITOR_VENDOR_GUID);
    let windows_absent = variable_is_absent(runtime, &mut windows_name, MONITOR_VENDOR_GUID);
    let linux_absent = variable_is_absent(runtime, &mut linux_name, MONITOR_VENDOR_GUID);
    let logical_absent = variable_is_absent(runtime, &mut logical_name, EFI_GLOBAL_VARIABLE_GUID);
    logical_deleted
        && windows_deleted
        && linux_deleted
        && windows_absent
        && linux_absent
        && logical_absent
}

/// Checks that the active variable store reports usable capacity.
fn query_variable_info_is_valid(runtime: *mut efi::RuntimeServices, attributes: u32) -> bool {
    let mut maximum_storage = 0;
    let mut remaining_storage = 0;
    let mut maximum_variable = 0;
    // SAFETY: all output values remain writable for the firmware call.
    (unsafe {
        ((*runtime).query_variable_info)(
            attributes,
            &raw mut maximum_storage,
            &raw mut remaining_storage,
            &raw mut maximum_variable,
        )
    }) == efi::Status::SUCCESS
        && maximum_storage >= remaining_storage
        && maximum_variable > 0
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
            && (name[..length] == WINDOWS_TEST_DRIVER[..WINDOWS_TEST_DRIVER.len() - 1]
                || name[..length] == LINUX_TEST_DRIVER[..LINUX_TEST_DRIVER.len() - 1])
        {
            return false;
        }
        if guid == EFI_GLOBAL_VARIABLE_GUID
            && name[..length] == TEST_DRIVER[..TEST_DRIVER.len() - 1]
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

    let (variable_backend, variables_ok) = variable_services_round_trip(system_table);
    match (variable_backend, variables_ok) {
        (Some(VariableBackend::Overlay), true) => {
            write_bytes(b"thin-hv: uefi variable overlay PASS\r\n");
        }
        (Some(VariableBackend::Overlay), false) => {
            write_bytes(b"thin-hv: uefi variable overlay FAIL\r\n");
        }
        (Some(VariableBackend::Native), true) => {
            write_bytes(b"thin-hv: uefi native variables PASS\r\n");
        }
        (Some(VariableBackend::Native), false) => {
            write_bytes(b"thin-hv: uefi native variables FAIL\r\n");
        }
        (None, _) => write_bytes(b"thin-hv: uefi variable probe FAIL\r\n"),
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
