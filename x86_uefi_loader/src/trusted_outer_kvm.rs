//! Test/reference outer-KVM backend using the shared non-resident chainloader.

use crate::SerialPort;
use crate::chainload::Error;
use crate::chainload::ascii_uefi_path;
use crate::chainload::device_path_utilities_protocol;
use crate::chainload::load_image_from_other_filesystem;
use crate::chainload::load_image_on_device;
use crate::chainload::loaded_image_protocol;
use crate::chainload::start_image;
use core::fmt::Write;
use r_efi::efi;

/// Payload staged by `run-uefi-smoke.sh`.
const GUEST_IMAGE_PATH: [efi::Char16; 23] = ascii_uefi_path(b"\\EFI\\BOOT\\GUESTX64.EFI\0");
/// Windows boot manager on an EFI System Partition.
const WINDOWS_BOOT_IMAGE_PATH: [efi::Char16; 33] =
    ascii_uefi_path(b"\\EFI\\Microsoft\\Boot\\bootmgfw.efi\0");
/// Stable profile selected when the staged Linux/test payload is present.
const LINUX_PROFILE: u32 = 2;
/// Stable profile selected when chainloading the installed Windows ESP.
const WINDOWS_PROFILE: u32 = 1;

/// Loads and starts the reference guest without installing a resident runtime.
pub(crate) fn run(
    parent_image: efi::Handle,
    system_table: *mut efi::SystemTable,
    serial: &mut SerialPort,
) -> Result<efi::Status, Error> {
    let loaded_image = loaded_image_protocol(parent_image, system_table)?;
    // SAFETY: HandleProtocol returned a non-null live LoadedImage interface;
    // it remains owned by the running parent UEFI application.
    let parent_device = unsafe { (*loaded_image).device_handle };
    serial.init();
    let _ = writeln!(serial, "thin-hv: uefi entry");
    let _ = writeln!(serial, "thin-hv: backend=outer-kvm role=reference");

    let utilities = device_path_utilities_protocol(system_table)?;
    let (guest_image, profile) =
        load_selected_guest(parent_image, parent_device, system_table, utilities)?;
    let _ = writeln!(
        serial,
        "thin-hv: trusted outer KVM direct chainload profile={profile} resident_runtime=0"
    );
    let status = start_image(guest_image, system_table)?;
    let _ = writeln!(serial, "thin-hv: trusted outer KVM guest PASS");
    Ok(status)
}

/// Keeps the existing test media selection separate from physical boot policy.
fn load_selected_guest(
    parent_image: efi::Handle,
    parent_device: efi::Handle,
    system_table: *mut efi::SystemTable,
    utilities: *mut efi::protocols::device_path_utilities::Protocol,
) -> Result<(efi::Handle, u32), Error> {
    match load_image_on_device(
        parent_image,
        system_table,
        parent_device,
        utilities,
        &GUEST_IMAGE_PATH,
    ) {
        Ok(image) => return Ok((image, LINUX_PROFILE)),
        Err(error) if error.is_missing_image() => {}
        Err(error) => return Err(error),
    }

    match load_image_on_device(
        parent_image,
        system_table,
        parent_device,
        utilities,
        &WINDOWS_BOOT_IMAGE_PATH,
    ) {
        Ok(image) => Ok((image, WINDOWS_PROFILE)),
        Err(error) if error.is_missing_image() => Ok((
            load_image_from_other_filesystem(
                parent_image,
                parent_device,
                system_table,
                utilities,
                &WINDOWS_BOOT_IMAGE_PATH,
            )?,
            WINDOWS_PROFILE,
        )),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::Error;
    use crate::chainload::start_image_result;
    use r_efi::efi;

    #[test]
    fn start_image_status_is_preserved() {
        assert_eq!(
            start_image_result(efi::Status::WARN_RESET_REQUIRED).unwrap(),
            efi::Status::WARN_RESET_REQUIRED
        );
        assert_eq!(
            start_image_result(efi::Status::SECURITY_VIOLATION)
                .unwrap_err()
                .status(),
            efi::Status::SECURITY_VIOLATION
        );
    }

    #[test]
    fn only_a_missing_loaded_image_allows_guest_fallback() {
        assert!(Error::Firmware("LoadImage", efi::Status::NOT_FOUND.as_usize()).is_missing_image());
        assert!(
            !Error::Firmware("LoadImage", efi::Status::DEVICE_ERROR.as_usize()).is_missing_image()
        );
        assert!(
            !Error::Firmware("LocateHandleBuffer", efi::Status::NOT_FOUND.as_usize())
                .is_missing_image()
        );
    }
}
