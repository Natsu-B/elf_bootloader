//! Firmware image-loading services shared by non-resident UEFI backends.

use core::ffi::c_void;
use core::fmt;
use core::ptr;
use r_efi::efi;

/// Failure from loading or starting an existing firmware image.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Error {
    /// A UEFI service failed or returned an invalid successful result.
    Firmware(&'static str, usize),
}

impl Error {
    /// Preserves the firmware status when returning to the parent image.
    pub(crate) fn status(self) -> efi::Status {
        match self {
            Self::Firmware(_, value) => efi::Status::from_usize(value),
        }
    }

    /// Only a missing image permits a caller's explicit selection fallback.
    pub(crate) fn is_missing_image(self) -> bool {
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

/// Converts a compile-time ASCII path, including its NUL, to firmware characters.
pub(crate) const fn ascii_uefi_path<const N: usize>(ascii: &[u8; N]) -> [efi::Char16; N] {
    let mut path = [0; N];
    let mut index = 0;
    while index < N {
        path[index] = ascii[index] as efi::Char16;
        index += 1;
    }
    path
}

/// Returns the live boot-services table supplied to this UEFI application.
pub(crate) fn boot_services(
    system_table: *mut efi::SystemTable,
) -> Result<*mut efi::BootServices, Error> {
    if system_table.is_null() {
        return Err(Error::Firmware(
            "SystemTable",
            efi::Status::INVALID_PARAMETER.as_usize(),
        ));
    }
    // SAFETY: The UEFI entry point supplies a live system table until ExitBootServices;
    // this non-resident chainloader only calls the helper before that transition.
    let services = unsafe { (*system_table).boot_services };
    if services.is_null() {
        return Err(Error::Firmware(
            "BootServices",
            efi::Status::UNSUPPORTED.as_usize(),
        ));
    }
    Ok(services)
}

/// Starts one loaded application and releases only firmware-owned exit data.
pub(crate) fn start_image(
    guest_image: efi::Handle,
    system_table: *mut efi::SystemTable,
) -> Result<efi::Status, Error> {
    let services = boot_services(system_table)?;
    if guest_image.is_null() {
        return Err(Error::Firmware(
            "StartImage(handle)",
            efi::Status::INVALID_PARAMETER.as_usize(),
        ));
    }
    let mut exit_data_size = 0;
    let mut exit_data = ptr::null_mut();
    // SAFETY: LoadImage supplied this live image handle; both output pointers refer
    // to writable locals, and Boot Services remain available if the child returns.
    let status =
        unsafe { ((*services).start_image)(guest_image, &mut exit_data_size, &mut exit_data) };
    let cleanup = if exit_data.is_null() {
        Ok(())
    } else {
        free_pool(services, exit_data.cast())
    };
    let status = start_image_result(status)?;
    cleanup?;
    if exit_data_size != 0 && exit_data.is_null() {
        return Err(Error::Firmware(
            "StartImage(ExitData)",
            efi::Status::DEVICE_ERROR.as_usize(),
        ));
    }
    // A returned UEFI application is already unloaded by StartImage. Unloading
    // this handle again would use a handle whose lifetime has ended.
    Ok(status)
}

/// Preserves both firmware errors and successful warning statuses.
pub(crate) fn start_image_result(status: efi::Status) -> Result<efi::Status, Error> {
    if status.is_error() {
        Err(Error::Firmware("StartImage", status.as_usize()))
    } else {
        Ok(status)
    }
}

/// Validates an absolute NUL-terminated path and its 16-bit device-node length.
fn file_path_node_size(image_path: &[efi::Char16]) -> Option<u16> {
    if image_path.len() < 3
        || image_path.first() != Some(&u16::from(b'\\'))
        || image_path.last() != Some(&0)
        || image_path[..image_path.len() - 1].contains(&0)
    {
        return None;
    }
    image_path
        .len()
        .checked_mul(core::mem::size_of::<efi::Char16>())
        .and_then(|path_size| {
            core::mem::size_of::<efi::protocols::device_path::Protocol>().checked_add(path_size)
        })
        .and_then(|size| u16::try_from(size).ok())
}

/// Loads one existing image through a complete device path rooted at its ESP.
pub(crate) fn load_image_on_device(
    parent_image: efi::Handle,
    system_table: *mut efi::SystemTable,
    device_handle: efi::Handle,
    utilities: *mut efi::protocols::device_path_utilities::Protocol,
    image_path: &[efi::Char16],
) -> Result<efi::Handle, Error> {
    let services = boot_services(system_table)?;
    if parent_image.is_null() || device_handle.is_null() || utilities.is_null() {
        return Err(Error::Firmware(
            "LoadImage(protocol arguments)",
            efi::Status::INVALID_PARAMETER.as_usize(),
        ));
    }
    let node_size = file_path_node_size(image_path).ok_or(Error::Firmware(
        "CreateDeviceNode(FilePath)",
        efi::Status::INVALID_PARAMETER.as_usize(),
    ))?;
    let mut device_path_guid = efi::protocols::device_path::PROTOCOL_GUID;
    let mut device_path = ptr::null_mut();
    // SAFETY: The caller's live filesystem handle is queried through live Boot
    // Services; the GUID and returned-interface slot are valid writable locals.
    let status = unsafe {
        ((*services).handle_protocol)(device_handle, &mut device_path_guid, &mut device_path)
    };
    if status.is_error() {
        return Err(Error::Firmware(
            "HandleProtocol(DevicePath)",
            status.as_usize(),
        ));
    }
    if device_path.is_null() {
        return Err(Error::Firmware(
            "HandleProtocol(DevicePath)",
            efi::Status::DEVICE_ERROR.as_usize(),
        ));
    }
    // SAFETY: LocateProtocol supplied this live utility interface; node_size was
    // checked to include the header and the complete NUL-terminated path.
    let file_node = unsafe {
        ((*utilities).create_device_node)(
            efi::protocols::device_path::TYPE_MEDIA,
            efi::protocols::device_path::Media::SUBTYPE_FILE_PATH,
            node_size,
        )
    };
    if file_node.is_null() {
        return Err(Error::Firmware(
            "CreateDeviceNode(FilePath)",
            efi::Status::OUT_OF_RESOURCES.as_usize(),
        ));
    }
    // SAFETY: CreateDeviceNode returned a readable device-node header.
    let returned_size = unsafe { u16::from_le_bytes((*file_node).length) };
    if returned_size != node_size {
        let _ = free_pool(services, file_node.cast());
        return Err(Error::Firmware(
            "CreateDeviceNode(FilePath length)",
            efi::Status::DEVICE_ERROR.as_usize(),
        ));
    }
    // SAFETY: The allocated node has the checked header-plus-path size and is
    // disjoint from the borrowed source slice; firmware pool alignment supports u16.
    unsafe {
        ptr::copy_nonoverlapping(
            image_path.as_ptr(),
            file_node
                .cast::<u8>()
                .add(core::mem::size_of::<efi::protocols::device_path::Protocol>())
                .cast::<efi::Char16>(),
            image_path.len(),
        );
    }
    // SAFETY: Both paths are firmware-provided live nodes; the utility copies
    // them into its own pool allocation, so the temporary node can then be freed.
    let complete_path = unsafe { ((*utilities).append_device_node)(device_path.cast(), file_node) };
    let node_cleanup = free_pool(services, file_node.cast());
    if let Err(error) = node_cleanup {
        if !complete_path.is_null() {
            let _ = free_pool(services, complete_path.cast());
        }
        return Err(error);
    }
    if complete_path.is_null() {
        return Err(Error::Firmware(
            "AppendDeviceNode(FilePath)",
            efi::Status::OUT_OF_RESOURCES.as_usize(),
        ));
    }
    let mut guest_image = ptr::null_mut();
    // SAFETY: The constructed complete path is live for this call; parent_image
    // is the running UEFI application and the output handle slot is writable.
    let status = unsafe {
        ((*services).load_image)(
            efi::Boolean::FALSE,
            parent_image,
            complete_path,
            ptr::null_mut(),
            0,
            &mut guest_image,
        )
    };
    let path_cleanup = free_pool(services, complete_path.cast());
    if status.is_error() {
        if status == efi::Status::SECURITY_VIOLATION && !guest_image.is_null() {
            // SAFETY: SECURITY_VIOLATION may return a registered image that cannot
            // be started; release that specific LoadImage-owned handle once.
            let _ = unsafe { ((*services).unload_image)(guest_image) };
        }
        return Err(Error::Firmware("LoadImage", status.as_usize()));
    }
    if guest_image.is_null() {
        return Err(Error::Firmware(
            "LoadImage(handle)",
            efi::Status::DEVICE_ERROR.as_usize(),
        ));
    }
    if let Err(error) = path_cleanup {
        // SAFETY: The successful LoadImage handle has not been started or passed
        // to another owner; release it because cleanup prevents returning it.
        let _ = unsafe { ((*services).unload_image)(guest_image) };
        return Err(error);
    }
    Ok(guest_image)
}

/// Returns the firmware's shared device-path helper protocol.
pub(crate) fn device_path_utilities_protocol(
    system_table: *mut efi::SystemTable,
) -> Result<*mut efi::protocols::device_path_utilities::Protocol, Error> {
    let services = boot_services(system_table)?;
    let mut guid = efi::protocols::device_path_utilities::PROTOCOL_GUID;
    let mut interface = ptr::null_mut();
    // SAFETY: Boot Services are live; the protocol GUID and output slot are
    // writable locals and a null registration requests any matching instance.
    let status =
        unsafe { ((*services).locate_protocol)(&mut guid, ptr::null_mut(), &mut interface) };
    if status.is_error() {
        return Err(Error::Firmware(
            "LocateProtocol(DevicePathUtilities)",
            status.as_usize(),
        ));
    }
    if interface.is_null() {
        return Err(Error::Firmware(
            "LocateProtocol(DevicePathUtilities)",
            efi::Status::DEVICE_ERROR.as_usize(),
        ));
    }
    Ok(interface.cast())
}

/// Returns the firmware's metadata for one loaded image handle.
pub(crate) fn loaded_image_protocol(
    image: efi::Handle,
    system_table: *mut efi::SystemTable,
) -> Result<*mut efi::protocols::loaded_image::Protocol, Error> {
    let services = boot_services(system_table)?;
    if image.is_null() {
        return Err(Error::Firmware(
            "HandleProtocol(LoadedImage handle)",
            efi::Status::INVALID_PARAMETER.as_usize(),
        ));
    }
    let mut guid = efi::protocols::loaded_image::PROTOCOL_GUID;
    let mut interface = ptr::null_mut();
    // SAFETY: image is a live UEFI handle supplied by the firmware or LoadImage;
    // the requested GUID and returned-interface slot are valid writable locals.
    let status = unsafe { ((*services).handle_protocol)(image, &mut guid, &mut interface) };
    if status.is_error() {
        return Err(Error::Firmware(
            "HandleProtocol(LoadedImage)",
            status.as_usize(),
        ));
    }
    if interface.is_null() {
        return Err(Error::Firmware(
            "HandleProtocol(LoadedImage)",
            efi::Status::DEVICE_ERROR.as_usize(),
        ));
    }
    Ok(interface.cast())
}

/// Releases a pool buffer allocated by this firmware or one of its protocols.
pub(crate) fn free_pool(
    services: *mut efi::BootServices,
    buffer: *mut c_void,
) -> Result<(), Error> {
    if services.is_null() || buffer.is_null() {
        return Err(Error::Firmware(
            "FreePool(arguments)",
            efi::Status::INVALID_PARAMETER.as_usize(),
        ));
    }
    // SAFETY: Every caller passes an outstanding firmware pool allocation and
    // the still-live Boot Services table; ownership ends at this call.
    let status = unsafe { ((*services).free_pool)(buffer) };
    if status.is_error() {
        Err(Error::Firmware("FreePool", status.as_usize()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ascii_uefi_path;
    use super::boot_services;
    use super::file_path_node_size;
    use super::start_image_result;
    use core::ptr;
    use r_efi::efi;

    #[test]
    fn file_path_nodes_reject_invalid_paths_and_length_overflow() {
        assert_eq!(file_path_node_size(&ascii_uefi_path(b"\\a\0")), Some(10));
        for path in [
            &[][..],
            &[0],
            &[92, 0],
            &[97, 0],
            &[92, 97],
            &[92, 0, 97, 0],
        ] {
            assert_eq!(file_path_node_size(path), None);
        }
        let mut path = vec![u16::from(b'a'); 32765];
        path[0] = u16::from(b'\\');
        path[32764] = 0;
        assert_eq!(file_path_node_size(&path), Some(65534));
        path.insert(1, u16::from(b'a'));
        assert_eq!(file_path_node_size(&path), None);
    }

    #[test]
    fn firmware_status_and_null_table_failures_are_preserved() {
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
        assert_eq!(
            boot_services(ptr::null_mut()).unwrap_err().status(),
            efi::Status::INVALID_PARAMETER
        );
    }
}
