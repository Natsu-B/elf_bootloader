//! Trusted-outer-KVM UEFI chainloader without the direct VMX backend.

use crate::SerialPort;
use core::ffi::c_void;
use core::fmt;
use core::fmt::Write;
use core::ptr;
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

const fn ascii_uefi_path<const N: usize>(ascii: &[u8; N]) -> [efi::Char16; N] {
    let mut path = [0; N];
    let mut index = 0;
    while index < N {
        path[index] = ascii[index] as efi::Char16;
        index += 1;
    }
    path
}

/// Failure from loading or starting the selected guest.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Error {
    /// A UEFI service used to load the guest failed.
    Firmware(&'static str, usize),
}

impl Error {
    /// Returns the original firmware status for the parent UEFI image.
    pub(super) fn status(self) -> efi::Status {
        match self {
            Self::Firmware(_, value) => efi::Status::from_usize(value),
        }
    }

    fn is_missing_image(self) -> bool {
        matches!(
            self,
            Self::Firmware("LoadImage", value) if value == efi::Status::NOT_FOUND.as_usize()
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Firmware(service, status) => write!(formatter, "{service} status={status:#x}"),
        }
    }
}

/// Loads and starts the selected guest as a plain UEFI application.
pub(crate) fn run(
    parent_image: efi::Handle,
    system_table: *mut efi::SystemTable,
    serial: &mut SerialPort,
) -> Result<efi::Status, Error> {
    let loaded_image = loaded_image_protocol(parent_image, system_table)?;
    let parent_device = unsafe { (*loaded_image).device_handle };
    serial.init();
    let _ = writeln!(serial, "thin-hv: uefi entry");

    let utilities = device_path_utilities_protocol(system_table)?;
    let (guest_image, profile) =
        load_selected_guest(parent_image, parent_device, system_table, utilities)?;
    let _ = writeln!(
        serial,
        "thin-hv: trusted outer KVM direct chainload profile={profile} resident_runtime=0"
    );

    let mut exit_data_size = 0;
    let mut exit_data = ptr::null_mut();
    let status = unsafe {
        ((*(*system_table).boot_services).start_image)(
            guest_image,
            &mut exit_data_size,
            &mut exit_data,
        )
    };
    if !exit_data.is_null() {
        free_pool(unsafe { (*system_table).boot_services }, exit_data.cast());
    }
    let result = start_image_result(status);
    if result.is_err() {
        let _ = unsafe { ((*(*system_table).boot_services).unload_image)(guest_image) };
    }
    let status = result?;
    let _ = writeln!(serial, "thin-hv: trusted outer KVM guest PASS");
    Ok(status)
}

fn start_image_result(status: efi::Status) -> Result<efi::Status, Error> {
    if status.is_error() {
        Err(Error::Firmware(
            "StartImage(trusted outer KVM guest)",
            status.as_usize(),
        ))
    } else {
        Ok(status)
    }
}

/// Selects the staged test/Linux image before an installed Windows loader.
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
        GUEST_IMAGE_PATH,
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
        WINDOWS_BOOT_IMAGE_PATH,
    ) {
        Ok(image) => Ok((image, WINDOWS_PROFILE)),
        Err(error) if error.is_missing_image() => Ok((
            load_image_from_other_filesystem(
                parent_image,
                parent_device,
                system_table,
                utilities,
                WINDOWS_BOOT_IMAGE_PATH,
            )?,
            WINDOWS_PROFILE,
        )),
        Err(error) => Err(error),
    }
}

/// Loads one image from a filesystem other than the loader's own ESP.
fn load_image_from_other_filesystem<const PATH_SIZE: usize>(
    parent_image: efi::Handle,
    parent_device: efi::Handle,
    system_table: *mut efi::SystemTable,
    utilities: *mut efi::protocols::device_path_utilities::Protocol,
    image_path: [efi::Char16; PATH_SIZE],
) -> Result<efi::Handle, Error> {
    let boot_services = unsafe { (*system_table).boot_services };
    let mut filesystem_guid = efi::protocols::simple_file_system::PROTOCOL_GUID;
    let mut handle_count = 0;
    let mut handles = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).locate_handle_buffer)(
            efi::BY_PROTOCOL,
            &mut filesystem_guid,
            ptr::null_mut(),
            &mut handle_count,
            &mut handles,
        )
    };
    if status.is_error() {
        return Err(Error::Firmware(
            "LocateHandleBuffer(SimpleFileSystem)",
            status.as_usize(),
        ));
    }
    if handles.is_null() {
        return Err(Error::Firmware(
            "LocateHandleBuffer(SimpleFileSystem)",
            efi::Status::INVALID_PARAMETER.as_usize(),
        ));
    }

    let mut result = Err(Error::Firmware(
        "LoadImage(other filesystem)",
        efi::Status::NOT_FOUND.as_usize(),
    ));
    // ponytail: firmware order selects the first non-parent Windows ESP;
    // select by profile partition GUID when multiple Windows installs matter.
    for index in 0..handle_count {
        let device_handle = unsafe { *handles.add(index) };
        if device_handle != parent_device {
            match load_image_on_device(
                parent_image,
                system_table,
                device_handle,
                utilities,
                image_path,
            ) {
                Ok(image) => {
                    result = Ok(image);
                    break;
                }
                Err(error) if error.is_missing_image() => {}
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
    }
    free_pool(boot_services, handles.cast());
    result
}

/// Loads one image using a complete path rooted at `device_handle`.
fn load_image_on_device<const PATH_SIZE: usize>(
    parent_image: efi::Handle,
    system_table: *mut efi::SystemTable,
    device_handle: efi::Handle,
    utilities: *mut efi::protocols::device_path_utilities::Protocol,
    image_path: [efi::Char16; PATH_SIZE],
) -> Result<efi::Handle, Error> {
    let boot_services = unsafe { (*system_table).boot_services };

    let mut device_path_guid = efi::protocols::device_path::PROTOCOL_GUID;
    let mut parent_device_path = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).handle_protocol)(
            device_handle,
            &mut device_path_guid,
            &mut parent_device_path,
        )
    };
    if status.is_error() {
        return Err(Error::Firmware(
            "HandleProtocol(DevicePath)",
            status.as_usize(),
        ));
    }

    let node_size = PATH_SIZE
        .checked_mul(core::mem::size_of::<efi::Char16>())
        .and_then(|path_size| {
            core::mem::size_of::<efi::protocols::device_path::Protocol>().checked_add(path_size)
        })
        .and_then(|size| u16::try_from(size).ok())
        .ok_or(Error::Firmware(
            "CreateDeviceNode(FilePath)",
            efi::Status::INVALID_PARAMETER.as_usize(),
        ))?;
    let file_path_node = unsafe {
        ((*utilities).create_device_node)(
            efi::protocols::device_path::TYPE_MEDIA,
            efi::protocols::device_path::Media::SUBTYPE_FILE_PATH,
            node_size,
        )
    };
    if file_path_node.is_null() {
        return Err(Error::Firmware(
            "CreateDeviceNode(FilePath)",
            efi::Status::OUT_OF_RESOURCES.as_usize(),
        ));
    }
    unsafe {
        ptr::copy_nonoverlapping(
            image_path.as_ptr(),
            file_path_node
                .cast::<u8>()
                .add(core::mem::size_of::<efi::protocols::device_path::Protocol>())
                .cast::<efi::Char16>(),
            PATH_SIZE,
        );
    }

    let complete_path = unsafe {
        ((*utilities).append_device_node)(parent_device_path.cast(), file_path_node.cast())
    };
    free_pool(boot_services, file_path_node.cast());
    if complete_path.is_null() {
        return Err(Error::Firmware(
            "AppendDeviceNode(FilePath)",
            efi::Status::OUT_OF_RESOURCES.as_usize(),
        ));
    }

    let mut guest_image = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).load_image)(
            efi::Boolean::FALSE,
            parent_image,
            complete_path,
            ptr::null_mut(),
            0,
            &mut guest_image,
        )
    };
    free_pool(boot_services, complete_path.cast());
    if status.is_error() {
        if status == efi::Status::SECURITY_VIOLATION && !guest_image.is_null() {
            let _ = unsafe { ((*boot_services).unload_image)(guest_image) };
        }
        return Err(Error::Firmware("LoadImage", status.as_usize()));
    }
    Ok(guest_image)
}

/// Returns the firmware's shared device-path helper protocol.
fn device_path_utilities_protocol(
    system_table: *mut efi::SystemTable,
) -> Result<*mut efi::protocols::device_path_utilities::Protocol, Error> {
    let mut guid = efi::protocols::device_path_utilities::PROTOCOL_GUID;
    let mut interface = ptr::null_mut();
    let status = unsafe {
        ((*(*system_table).boot_services).locate_protocol)(
            &mut guid,
            ptr::null_mut(),
            &mut interface,
        )
    };
    if status.is_error() {
        Err(Error::Firmware(
            "LocateProtocol(DevicePathUtilities)",
            status.as_usize(),
        ))
    } else {
        Ok(interface.cast())
    }
}

/// Returns the firmware's metadata for one loaded image handle.
fn loaded_image_protocol(
    image: efi::Handle,
    system_table: *mut efi::SystemTable,
) -> Result<*mut efi::protocols::loaded_image::Protocol, Error> {
    let mut guid = efi::protocols::loaded_image::PROTOCOL_GUID;
    let mut interface = ptr::null_mut();
    let status = unsafe {
        ((*(*system_table).boot_services).handle_protocol)(image, &mut guid, &mut interface)
    };
    if status.is_error() {
        Err(Error::Firmware(
            "HandleProtocol(LoadedImage)",
            status.as_usize(),
        ))
    } else {
        Ok(interface.cast())
    }
}

fn free_pool(boot_services: *mut efi::BootServices, buffer: *mut c_void) {
    // SAFETY: `buffer` was allocated by this firmware or one of its protocols.
    let _ = unsafe { ((*boot_services).free_pool)(buffer) };
}

#[cfg(test)]
mod tests {
    use super::Error;
    use super::start_image_result;
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
