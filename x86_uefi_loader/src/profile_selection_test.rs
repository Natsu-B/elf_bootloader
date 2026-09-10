//! QEMU-only caller for the actual profile Direct application, not a mock L0.
#![cfg_attr(not(test), no_main)]
#![cfg_attr(not(test), no_std)]

mod chainload;
mod runtime_variables;

use core::ptr;
use r_efi::efi;
use uefi_variable_overlay::UefiProfile;

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
