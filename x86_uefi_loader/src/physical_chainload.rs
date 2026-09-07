//! Nonresident, non-VMX chainloading on the current image's physical ESP.
//!
//! Empty load options select the original Windows Boot Manager. Otherwise the
//! options are one NUL-terminated UTF-16 absolute ASCII path on that same ESP.
//! No filesystem enumeration, staged QEMU guest, or outer-KVM fallback occurs.

use crate::SerialPort;
use crate::chainload;
use crate::chainload::Error;
use core::fmt::Write;
use core::ptr;
use core::slice;
use r_efi::efi;

/// Bounded configuration storage, including the terminal NUL.
const MAX_PATH_UNITS: usize = 260;
/// Upper bound on the firmware's file-path portion, including node headers.
const MAX_DEVICE_PATH_BYTES: usize = 4096;
/// Firmware-owned Windows Boot Manager; this loader never replaces that file.
const WINDOWS_PATH: [efi::Char16; 33] =
    chainload::ascii_uefi_path(b"\\EFI\\Microsoft\\Boot\\bootmgfw.efi\0");

/// An absolute path whose termination, components, and length have been checked.
#[derive(Debug)]
struct ImagePath {
    /// Inline storage avoids a configuration-time allocator.
    units: [efi::Char16; MAX_PATH_UNITS],
    /// Number of initialized units, including the terminal NUL.
    len: usize,
}

impl ImagePath {
    /// Checks a complete path; only unambiguous ASCII FAT names are accepted.
    fn from_units(units: &[efi::Char16]) -> Result<Self, Error> {
        if units.len() < 3
            || units.len() > MAX_PATH_UNITS
            || units[0] != u16::from(b'\\')
            || units.last() != Some(&0)
        {
            return Err(invalid("physical image path length/root/termination"));
        }
        let content = &units[1..units.len() - 1];
        if content
            .iter()
            .any(|&unit| !(0x20..=0x7e).contains(&unit) || b"/:*?\"<>|".contains(&(unit as u8)))
        {
            return Err(invalid("physical image path encoding/characters"));
        }
        for component in content.split(|&unit| unit == u16::from(b'\\')) {
            if component.is_empty()
                || component == [u16::from(b'.')]
                || component == [u16::from(b'.'), u16::from(b'.')]
                || matches!(component.last(), Some(0x20 | 0x2e))
            {
                return Err(invalid("physical image path component"));
            }
        }
        let mut path = Self {
            units: [0; MAX_PATH_UNITS],
            len: units.len(),
        };
        path.units[..units.len()].copy_from_slice(units);
        Ok(path)
    }

    /// Returns the checked, terminated path required by firmware image services.
    fn as_slice(&self) -> &[efi::Char16] {
        &self.units[..self.len]
    }

    /// FAT paths in this backend are ASCII and case insensitive.
    fn same_file(&self, other: &Self) -> bool {
        self.len == other.len
            && self
                .as_slice()
                .iter()
                .zip(other.as_slice())
                .all(|(&left, &right)| (left as u8).eq_ignore_ascii_case(&(right as u8)))
    }
}

/// Converts a rejected configuration or firmware file-path layout to a UEFI error.
fn invalid(reason: &'static str) -> Error {
    Error::Firmware(reason, efi::Status::INVALID_PARAMETER.as_usize())
}

/// Parses binary UEFI load options without assuming alignment or a C-string length.
fn selected_path(options: &[u8]) -> Result<ImagePath, Error> {
    if options.is_empty() {
        return ImagePath::from_units(&WINDOWS_PATH);
    }
    if options.len() % 2 != 0 || options.len() > MAX_PATH_UNITS * 2 {
        return Err(invalid("physical load options byte length"));
    }
    let mut units = [0; MAX_PATH_UNITS];
    for (destination, source) in units.iter_mut().zip(options.chunks_exact(2)) {
        *destination = u16::from_le_bytes([source[0], source[1]]);
    }
    ImagePath::from_units(&units[..options.len() / 2])
}

/// Decodes only the file-path portion defined by EFI_LOADED_IMAGE_PROTOCOL.
fn image_file_path(bytes: &[u8]) -> Result<ImagePath, Error> {
    let mut units = [0; MAX_PATH_UNITS];
    let mut count = 0_usize;
    let mut offset = 0_usize;
    while let Some(header) = bytes.get(offset..offset.saturating_add(4)) {
        let size = usize::from(u16::from_le_bytes([header[2], header[3]]));
        let end = offset
            .checked_add(size)
            .ok_or(invalid("image device path overflow"))?;
        if size < 4 || end > bytes.len() {
            return Err(invalid("image device path node size"));
        }
        if header[0] == 0x7f && header[1] == 0xff {
            if size != 4 || end != bytes.len() || count == 0 {
                return Err(invalid("image device path end node"));
            }
            return ImagePath::from_units(&units[..count + 1]);
        }
        if header[0] != 4 || header[1] != 4 || size < 6 || size % 2 != 0 {
            return Err(invalid("unsupported image file-path node"));
        }
        let payload = &bytes[offset + 4..end];
        if payload[payload.len() - 2..] != [0, 0] {
            return Err(invalid("image file-path node termination"));
        }
        for word in payload[..payload.len() - 2].chunks_exact(2) {
            if count >= MAX_PATH_UNITS - 1 {
                return Err(invalid("image file path too long"));
            }
            units[count] = u16::from_le_bytes([word[0], word[1]]);
            count += 1;
        }
        offset = end;
    }
    Err(invalid("image device path missing end node"))
}

/// Starts exactly one selected image from the firmware-identified current ESP.
pub(crate) fn run(
    image: efi::Handle,
    system_table: *mut efi::SystemTable,
    serial: &mut SerialPort,
) -> Result<efi::Status, Error> {
    serial.init();
    let _ = writeln!(serial, "thin-hv: uefi entry");
    let _ = writeln!(
        serial,
        "thin-hv: backend=physical-chainload project_vmx=0 resident_runtime=0"
    );
    let loaded = chainload::loaded_image_protocol(image, system_table)?;
    // SAFETY: HandleProtocol succeeded with a non-null, live LoadedImage interface.
    let (device, current_path, options, options_size) = unsafe {
        (
            (*loaded).device_handle,
            (*loaded).file_path,
            (*loaded).load_options,
            (*loaded).load_options_size as usize,
        )
    };
    if device.is_null() || current_path.is_null() {
        return Err(invalid("physical loader needs an existing ESP file path"));
    }
    if options_size > MAX_PATH_UNITS * 2
        || options_size % 2 != 0
        || (options_size != 0 && options.is_null())
        || (options as usize).checked_add(options_size).is_none()
    {
        return Err(invalid("physical load options buffer"));
    }
    let bytes = if options_size == 0 {
        &[]
    } else {
        // SAFETY: firmware owns LoadOptionsSize readable bytes until this application
        // returns; null, wraparound, and the bounded byte length were checked above.
        unsafe { slice::from_raw_parts(options.cast::<u8>(), options_size) }
    };
    let target = selected_path(bytes)?;
    let utilities = chainload::device_path_utilities_protocol(system_table)?;
    // SAFETY: LoadedImage.FilePath is a live firmware device path; the validated
    // DevicePathUtilities interface describes its allocation without modifying it.
    let current_size = unsafe { ((*utilities).get_device_path_size)(current_path) };
    if !(4..=MAX_DEVICE_PATH_BYTES).contains(&current_size)
        || (current_path as usize).checked_add(current_size).is_none()
    {
        return Err(invalid("physical loader file-path size"));
    }
    // SAFETY: firmware supplied this FilePath allocation and its byte length via
    // DevicePathUtilities; the non-null range and bounded size were checked above.
    let current_bytes = unsafe { slice::from_raw_parts(current_path.cast::<u8>(), current_size) };
    if target.same_file(&image_file_path(current_bytes)?) {
        return Err(Error::Firmware(
            "physical chainload self-reference",
            efi::Status::ACCESS_DENIED.as_usize(),
        ));
    }

    let boot_services = chainload::boot_services(system_table)?;
    let mut filesystem_guid = efi::protocols::simple_file_system::PROTOCOL_GUID;
    let mut filesystem = ptr::null_mut();
    // SAFETY: the live image supplied `device`; the GUID and output slot are valid
    // for this synchronous read-only protocol lookup while Boot Services are active.
    let status = unsafe {
        ((*boot_services).handle_protocol)(device, &mut filesystem_guid, &mut filesystem)
    };
    if status.is_error() {
        return Err(Error::Firmware(
            "physical ESP SimpleFileSystem",
            status.as_usize(),
        ));
    }
    if filesystem.is_null() {
        return Err(invalid("physical ESP null SimpleFileSystem"));
    }
    let _ = writeln!(
        serial,
        "thin-hv: physical chainload scope=current-esp explicit_path={}",
        u8::from(!bytes.is_empty())
    );
    let guest =
        chainload::load_image_on_device(image, system_table, device, utilities, target.as_slice())?;
    let status = chainload::start_image(guest, system_table)?;
    let _ = writeln!(serial, "thin-hv: physical chainload PASS");
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(path: &str) -> Vec<u8> {
        path.encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn file_node(path: &str) -> Vec<u8> {
        let payload = options(path);
        let size = u16::try_from(payload.len() + 4).unwrap().to_le_bytes();
        let mut node = vec![4, 4, size[0], size[1]];
        node.extend(payload);
        node.extend([0x7f, 0xff, 4, 0]);
        node
    }

    #[test]
    fn defaults_to_original_windows_path_not_staged_guest() {
        assert_eq!(selected_path(&[]).unwrap().as_slice(), WINDOWS_PATH);
    }

    #[test]
    fn explicit_linux_path_uses_the_same_checked_mechanism() {
        let path = selected_path(&options("\\EFI\\ubuntu\\shimx64.efi")).unwrap();
        let current = image_file_path(&file_node("\\EFI\\BOOT\\BOOTX64.EFI")).unwrap();
        assert!(!path.same_file(&current));
        assert!(path.same_file(&selected_path(&options("\\efi\\UBUNTU\\SHIMX64.EFI")).unwrap()));
    }

    #[test]
    fn rejects_malformed_ambiguous_and_unsupported_load_options() {
        for path in [
            "",
            "\\",
            "relative.efi",
            "\\EFI\\..\\BOOTX64.EFI",
            "\\EFI\\.\\x",
            "\\EFI\\\\x",
            "\\EFI\\x\\",
            "\\EFI\\x.",
            "\\EFI\\x ",
            "\\EFI/x",
            "\\EFI\\x:y",
            "\\EFI\\\u{d7ff}",
            "\\EFI\\x\0extra",
        ] {
            assert!(selected_path(&options(path)).is_err(), "{path:?}");
        }
        assert!(selected_path(&[0]).is_err());
        assert!(selected_path(&[b'\\', 0, b'x', 0]).is_err());
        assert!(selected_path(&vec![0; MAX_PATH_UNITS * 2 + 2]).is_err());
        assert!(selected_path(&options(&format!("\\{}", "a".repeat(MAX_PATH_UNITS - 2)))).is_ok());
        assert!(selected_path(&options(&format!("\\{}", "a".repeat(MAX_PATH_UNITS - 1)))).is_err());
    }

    #[test]
    fn detects_self_chainload_case_insensitively_and_rejects_broken_device_paths() {
        let target = selected_path(&options("\\efi\\boot\\bootx64.efi")).unwrap();
        assert!(
            target.same_file(&image_file_path(&file_node("\\EFI\\BOOT\\BOOTX64.EFI")).unwrap())
        );
        for bytes in [
            vec![],
            vec![4, 4, 0, 0],
            vec![4, 4, 255, 255],
            vec![0x7f, 0xff, 4, 0],
            vec![4, 4, 7, 0, 1, 0, 0],
            vec![4, 4, 6, 0, 1, 0],
        ] {
            assert!(image_file_path(&bytes).is_err());
        }
        let mut extra = file_node("\\EFI\\BOOT\\BOOTX64.EFI");
        extra.push(0);
        assert!(image_file_path(&extra).is_err());
    }
}
