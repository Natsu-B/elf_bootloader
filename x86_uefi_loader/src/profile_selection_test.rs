//! QEMU-only caller for the actual profile Direct application, not a mock L0.
#![cfg_attr(not(test), no_main)]
#![cfg_attr(not(test), no_std)]

mod chainload;
mod runtime_variables;

use core::ptr;
use r_efi::efi;
use uefi_variable_overlay::UefiProfile;

/// This GUID owns only disposable fixture and profile data, never security state.
fn project_guid() -> efi::Guid {
    efi::Guid::from_fields(
        0xd7e7_166a,
        0x574a,
        0x4c70,
        0xa3,
        0xd0,
        &[0x55, 0xd8, 0xd6, 0x6d, 0x3a, 0x42],
    )
}

fn set_private(
    runtime: *mut efi::RuntimeServices,
    profile: UefiProfile,
    name: &[u16],
    data: &[u8],
) -> Result<(), efi::Status> {
    let key = uefi_variable_overlay::map_private_variable(
        profile.id(),
        uefi_variable_overlay::EFI_GLOBAL_VARIABLE_GUID,
        name,
    )
    .ok_or(efi::Status::INVALID_PARAMETER)?;
    let mut physical = [0; uefi_variable_overlay::BACKEND_NAME_CAPACITY + 1];
    physical[..key.name().len()].copy_from_slice(key.name());
    let mut guid = project_guid();
    // SAFETY: this fixture owns a fresh disposable firmware store before EBS;
    // mapped name, GUID and bounded data live for the synchronous native write.
    let status = unsafe {
        ((*runtime).set_variable)(
            physical.as_mut_ptr(),
            &mut guid,
            7,
            data.len(),
            data.as_ptr().cast_mut().cast(),
        )
    };
    if status.is_error() {
        Err(status)
    } else {
        Ok(())
    }
}

/// Windows BootNext deliberately references an inactive option; Linux BootOrder
/// skips an inactive entry and a missing one after consuming a missing BootNext.
fn seed_boot_option(
    system: *mut efi::SystemTable,
    device: efi::Handle,
    runtime: *mut efi::RuntimeServices,
    profile: UefiProfile,
) -> Result<(), efi::Status> {
    let mut record = [0u8; 4096];
    record[0] = u8::from(profile == UefiProfile::Linux);
    let mut used = 8;
    if profile == UefiProfile::Linux {
        let services = chainload::boot_services(system).map_err(chainload::Error::status)?;
        let utilities =
            chainload::device_path_utilities_protocol(system).map_err(chainload::Error::status)?;
        let mut guid = efi::protocols::device_path::PROTOCOL_GUID;
        let mut base = ptr::null_mut();
        // SAFETY: live current ESP handle and owned protocol/GUID result slots.
        let status = unsafe { ((*services).handle_protocol)(device, &mut guid, &mut base) };
        if status.is_error() {
            return Err(status);
        }
        if base.is_null() {
            return Err(efi::Status::DEVICE_ERROR);
        }
        // SAFETY: firmware returned the complete DevicePath allocation.
        let size = unsafe { ((*utilities).get_device_path_size)(base.cast()) };
        if !(4..=1024).contains(&size) || (base as usize).checked_add(size).is_none() {
            return Err(efi::Status::COMPROMISED_DATA);
        }
        // SAFETY: this checked firmware-sized path is readable; owned record
        // storage has room for its prefix and never aliases the protocol bytes.
        let path = unsafe { core::slice::from_raw_parts(base.cast::<u8>(), size) };
        if !path.ends_with(&[0x7f, 0xff, 4, 0]) {
            return Err(efi::Status::COMPROMISED_DATA);
        }
        record[used..used + size - 4].copy_from_slice(&path[..size - 4]);
        used += size - 4;
    }
    let path = chainload::ascii_uefi_path(b"\\EFI\\Test\\OPTION.EFI\0");
    let size = 4 + path.len() * 2;
    record[used..used + 4].copy_from_slice(&[4, 4, size as u8, (size >> 8) as u8]);
    for (bytes, value) in record[used + 4..used + size].chunks_exact_mut(2).zip(path) {
        bytes.copy_from_slice(&value.to_le_bytes());
    }
    used += size;
    record[used..used + 4].copy_from_slice(&[0x7f, 0xff, 4, 0]);
    used += 4;
    record[4..6].copy_from_slice(&((used - 8) as u16).to_le_bytes());
    record[used..used + 8].copy_from_slice(b"THVOPT1\0");
    used += 8;
    let option_name = chainload::ascii_uefi_path(b"Boot0042\0");
    set_private(runtime, profile, &option_name[..8], &record[..used])?;
    if profile == UefiProfile::Linux {
        record[0] = 0;
        let inactive = chainload::ascii_uefi_path(b"Boot0043\0");
        set_private(runtime, profile, &inactive[..8], &record[..used])?;
        let order = chainload::ascii_uefi_path(b"BootOrder\0");
        set_private(runtime, profile, &order[..9], &[0x43, 0, 0x44, 0, 0x42, 0])?;
    }
    let next = chainload::ascii_uefi_path(b"BootNext\0");
    set_private(
        runtime,
        profile,
        &next[..8],
        &[
            if profile == UefiProfile::Windows {
                0x42
            } else {
                0x44
            },
            0,
        ],
    )?;
    let mut expected = chainload::ascii_uefi_path(b"ProfileOptionFixture\0");
    let mut guid = project_guid();
    let mut number = 0x42u16;
    // SAFETY: fixture-only boot-services variable declares the exact expected
    // option to the native guest; it contains no boot command or identity data.
    let status = unsafe {
        ((*runtime).set_variable)(
            expected.as_mut_ptr(),
            &mut guid,
            efi::VARIABLE_BOOTSERVICE_ACCESS,
            2,
            ptr::addr_of_mut!(number).cast(),
        )
    };
    if status.is_error() {
        Err(status)
    } else {
        Ok(())
    }
}

/// Bounded fixture diagnostics contain no variable values or machine identity.
fn message(bytes: &[u8]) {
    for &byte in bytes {
        let mut ready = false;
        for _ in 0..65_536 {
            let status: u8;
            // SAFETY: this disposable x86 UEFI fixture owns QEMU COM1 at CPL0.
            unsafe {
                core::arch::asm!("in al, dx", in("dx") 0x3fdu16, out("al") status, options(nomem, nostack, preserves_flags))
            };
            if status != 255 && status & 32 != 0 {
                ready = true;
                break;
            }
        }
        if !ready {
            return;
        }
        // SAFETY: the owned COM1 transmitter just reported available space.
        unsafe {
            core::arch::asm!("out dx, al", in("dx") 0x3f8u16, in("al") byte, options(nomem, nostack, preserves_flags))
        };
    }
}

#[cfg(not(test))]
#[unsafe(no_mangle)]
pub extern "efiapi" fn efi_main(image: efi::Handle, system: *mut efi::SystemTable) -> efi::Status {
    let result: Result<(), efi::Status> = (|| {
        let (profile, explicit) = match option_env!("THIN_HV_PROFILE_FIXTURE") {
            Some("windows-explicit") => (UefiProfile::Windows, true),
            Some("linux-explicit") => (UefiProfile::Linux, true),
            Some("windows-persistent") => (UefiProfile::Windows, false),
            Some("linux-persistent") => (UefiProfile::Linux, false),
            Some("windows-next") => (UefiProfile::Windows, false),
            Some("linux-order") => (UefiProfile::Linux, false),
            _ => return Err(efi::Status::INVALID_PARAMETER),
        };
        let loaded =
            chainload::loaded_image_protocol(image, system).map_err(chainload::Error::status)?;
        let services = chainload::boot_services(system).map_err(chainload::Error::status)?;
        // SAFETY: these checked firmware tables/image interfaces remain live;
        // no code in this fixture exits Boot Services before child launch.
        let (device, runtime) = unsafe { ((*loaded).device_handle, (*system).runtime_services) };
        let opposite = if profile == UefiProfile::Windows {
            UefiProfile::Linux
        } else {
            UefiProfile::Windows
        };
        runtime_variables::write_boot_profile(runtime, if explicit { opposite } else { profile })?;
        if matches!(
            option_env!("THIN_HV_PROFILE_FIXTURE"),
            Some("windows-next" | "linux-order")
        ) {
            seed_boot_option(system, device, runtime, profile)?;
        }
        let utilities =
            chainload::device_path_utilities_protocol(system).map_err(chainload::Error::status)?;
        let path = chainload::ascii_uefi_path(b"\\EFI\\Test\\PROFILE.EFI\0");
        let application = chainload::load_image_on_device(image, system, device, utilities, &path)
            .map_err(chainload::Error::status)?;
        let child = match chainload::loaded_image_protocol(application, system) {
            Ok(child) => child,
            Err(error) => {
                chainload::unload_image(services, application).map_err(chainload::Error::status)?;
                return Err(error.status());
            }
        };
        let mut options = [0u16; 8];
        let name = if profile == UefiProfile::Windows {
            b"windows".as_slice()
        } else {
            b"linux".as_slice()
        };
        for (unit, &byte) in options.iter_mut().zip(name) {
            *unit = u16::from(byte);
        }
        // SAFETY: this unstarted child is exclusively owned; stack options live
        // throughout its synchronous StartImage (successful Direct never returns).
        unsafe {
            (*child).load_options_size = if explicit {
                ((name.len() + 1) * 2) as u32
            } else {
                0
            };
            (*child).load_options = if explicit {
                options.as_mut_ptr().cast()
            } else {
                ptr::null_mut()
            };
        }
        message(b"thin-hv: profile selection fixture begin\n");
        let started = chainload::start_image(application, system);
        // Exit normally unloads an application. Query again in case StartImage
        // rejected it before entry, and retire only a proven-live child.
        match chainload::loaded_image_protocol(application, system) {
            Ok(live) => {
                // SAFETY: a fresh firmware lookup proved this unstarted/returned
                // image remains live; release its borrow of our stack before unload.
                unsafe {
                    (*live).load_options_size = 0;
                    (*live).load_options = ptr::null_mut();
                }
                chainload::unload_image(services, application).map_err(chainload::Error::status)?;
            }
            Err(chainload::Error::Firmware("HandleProtocol(LoadedImage)", status))
                if status == efi::Status::INVALID_PARAMETER.as_usize()
                    || status == efi::Status::UNSUPPORTED.as_usize() => {}
            Err(error) => return Err(error.status()),
        }
        started.map_err(chainload::Error::status)?;
        Err(efi::Status::ABORTED)
    })();
    message(b"thin-hv: profile selection fixture FAIL\n");
    result.err().unwrap_or(efi::Status::DEVICE_ERROR)
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    message(b"thin-hv: profile selection fixture FAIL panic\n");
    loop {
        core::hint::spin_loop();
    }
}
