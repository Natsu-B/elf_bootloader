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
#[cfg(feature = "physical-chainload")]
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
    let guest = load_selected(image, system_table, serial)?;
    let status = chainload::start_image(guest, system_table)?;
    let _ = writeln!(serial, "thin-hv: physical chainload PASS");
    Ok(status)
}

/// Loads only the checked existing path on the current image's ESP. The caller
/// owns the resulting handle until StartImage or an explicit failure cleanup.
/// Shared by the nonresident baseline and the no-overlay Direct launch path.
pub(crate) fn load_selected(
    image: efi::Handle,
    system_table: *mut efi::SystemTable,
    serial: &mut SerialPort,
) -> Result<efi::Handle, Error> {
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
    load_path(
        image,
        system_table,
        serial,
        device,
        current_path,
        &target,
        !bytes.is_empty(),
    )
}

/// Loads a configured profile path on the same firmware-identified ESP. No
/// filesystem enumeration or missing-image fallback is permitted.
#[cfg(feature = "profile-direct-vmx")]
pub(crate) fn load_profile_path(
    image: efi::Handle,
    system_table: *mut efi::SystemTable,
    serial: &mut SerialPort,
    path: &str,
) -> Result<efi::Handle, Error> {
    if path.len() >= MAX_PATH_UNITS {
        return Err(invalid("profile path length"));
    }
    let mut units = [0; MAX_PATH_UNITS];
    for (unit, byte) in units.iter_mut().zip(path.bytes()) {
        *unit = u16::from(byte);
    }
    let target = ImagePath::from_units(&units[..path.len() + 1])?;
    let loaded = chainload::loaded_image_protocol(image, system_table)?;
    // SAFETY: the checked LoadedImage interface is live before StartImage/EBS.
    // Its device handle and FilePath remain firmware-owned during this load.
    let (device, current_path) = unsafe { ((*loaded).device_handle, (*loaded).file_path) };
    if device.is_null() || current_path.is_null() {
        return Err(invalid("profile loader needs an existing ESP file path"));
    }
    load_path(
        image,
        system_table,
        serial,
        device,
        current_path,
        &target,
        true,
    )
}

/// Shared final path validation and firmware load, after source/target decoding.
fn load_path(
    image: efi::Handle,
    system_table: *mut efi::SystemTable,
    serial: &mut SerialPort,
    device: efi::Handle,
    current_path: *mut efi::protocols::device_path::Protocol,
    target: &ImagePath,
    explicit: bool,
) -> Result<efi::Handle, Error> {
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
        u8::from(explicit)
    );
    chainload::load_image_on_device(image, system_table, device, utilities, target.as_slice())
}

/// Separates a bounded, single-instance boot device path into device and file
/// portions. The existing ImagePath decoder validates the file nodes afterward.
#[cfg(feature = "profile-direct-vmx")]
fn boot_file_path(bytes: &[u8]) -> Result<(&[u8], ImagePath), Error> {
    if bytes.len() < 10 || bytes.len() > MAX_DEVICE_PATH_BYTES {
        return Err(invalid("profile boot device-path length"));
    }
    let mut offset = 0;
    while let Some(header) = bytes.get(offset..offset + 4) {
        let size = usize::from(u16::from_le_bytes([header[2], header[3]]));
        if size < 4 || size > bytes.len() - offset {
            return Err(invalid("profile boot device-path node"));
        }
        if header[..2] == [4, 4] {
            return Ok((&bytes[..offset], image_file_path(&bytes[offset..])?));
        }
        if header[0] == 0x7f {
            return Err(invalid("profile boot path has no unique file"));
        }
        offset += size;
    }
    Err(invalid("profile boot path missing file"))
}

/// Full device paths must match this ESP exactly. A short hard-drive path can
/// instead name this same partition by its firmware-provided signature. Nothing
/// searches, rewrites, or assumes a particular PCI/filesystem enumeration order.
#[cfg(feature = "profile-direct-vmx")]
fn same_boot_device(requested: &[u8], current: &[u8]) -> bool {
    if current.len() < 4 || !current.ends_with(&[0x7f, 0xff, 4, 0]) {
        return false;
    }
    // A file-only option is explicitly rooted at this backend's current ESP.
    if requested.is_empty() || requested == &current[..current.len() - 4] {
        return true;
    }
    // UEFI hard-drive short form: one 42-byte media node, MBR or GPT signature.
    if requested.len() != 42 || requested[..4] != [4, 1, 42, 0] {
        return false;
    }
    let mut offset = 0;
    while let Some(header) = current.get(offset..offset + 4) {
        let size = usize::from(u16::from_le_bytes([header[2], header[3]]));
        if size < 4 || size > current.len() - offset {
            return false;
        }
        let node = &current[offset..offset + size];
        if header[..2] == [4, 1] && size == 42 {
            return matches!((node[40], node[41]), (1, 1) | (2, 2))
                && node[40..42] == requested[40..42]
                && match requested[41] {
                    1 => node[4..8] == requested[4..8] && node[24..28] == requested[24..28],
                    2 => node[24..40] == requested[24..40],
                    _ => false,
                };
        }
        if header[0] == 0x7f {
            return false;
        }
        offset += size;
    }
    false
}

/// Loads a profile Boot#### file only after matching its device to the current
/// ESP. Firmware still performs PE/signature checks through normal LoadImage.
#[cfg(feature = "profile-direct-vmx")]
pub(crate) fn load_profile_device_path(
    image: efi::Handle,
    system: *mut efi::SystemTable,
    serial: &mut SerialPort,
    path: &[u8],
) -> Result<efi::Handle, Error> {
    let (requested, target) = boot_file_path(path)?;
    let loaded = chainload::loaded_image_protocol(image, system)?;
    let services = chainload::boot_services(system)?;
    let utilities = chainload::device_path_utilities_protocol(system)?;
    // SAFETY: checked LoadedImage owns live firmware metadata until child launch.
    let (device, current_path) = unsafe { ((*loaded).device_handle, (*loaded).file_path) };
    if device.is_null() || current_path.is_null() {
        return Err(invalid("profile ESP image"));
    }
    let mut guid = efi::protocols::device_path::PROTOCOL_GUID;
    let mut base = ptr::null_mut();
    // SAFETY: the live ESP handle is queried with owned GUID/output slots; this
    // lookup cannot enumerate or substitute another device.
    let status = unsafe { ((*services).handle_protocol)(device, &mut guid, &mut base) };
    if status.is_error() {
        return Err(Error::Firmware("profile ESP DevicePath", status.as_usize()));
    }
    if base.is_null() {
        return Err(invalid("profile ESP null DevicePath"));
    }
    // SAFETY: successful protocol lookup returned this complete firmware path.
    let size = unsafe { ((*utilities).get_device_path_size)(base.cast()) };
    if !(4..=MAX_DEVICE_PATH_BYTES).contains(&size) || (base as usize).checked_add(size).is_none() {
        return Err(invalid("profile ESP device-path bounds"));
    }
    // SAFETY: firmware supplied the live path allocation and its bounded size;
    // the path is read only before loading/starting any image.
    let current = unsafe { slice::from_raw_parts(base.cast::<u8>(), size) };
    if !same_boot_device(requested, current) {
        return Err(Error::Firmware(
            "profile boot option names another ESP",
            efi::Status::ACCESS_DENIED.as_usize(),
        ));
    }
    load_path(image, system, serial, device, current_path, &target, true)
}

/// Conservative qualification gate, not SMP support. Even disabled additional
/// processors are rejected: L1 must not later bring an unowned physical CPU up.
#[cfg(feature = "physical-direct-vmx")]
fn single_bsp(total: usize, enabled: usize, current: usize, flags: u32) -> bool {
    use efi::protocols::mp_services as mp;
    total == 1
        && enabled == 1
        && current == 0
        && flags
            == mp::PROCESSOR_AS_BSP_BIT
                | mp::PROCESSOR_ENABLED_BIT
                | mp::PROCESSOR_HEALTH_STATUS_BIT
}

/// Read-only MP Services inventory before loading an OS or entering project VMX.
/// No AP startup, disable, switch-BSP or firmware topology operation is issued.
#[cfg(feature = "physical-direct-vmx")]
pub(crate) fn require_single_cpu(
    system_table: *mut efi::SystemTable,
    serial: &mut SerialPort,
) -> Result<(), Error> {
    use efi::protocols::mp_services as mp;
    let services = chainload::boot_services(system_table)?;
    let mut guid = mp::PROTOCOL_GUID;
    let mut interface = ptr::null_mut();
    // SAFETY: the live Boot Services table and bounded writable output are
    // valid for this synchronous protocol lookup; no firmware state changes.
    let status =
        unsafe { ((*services).locate_protocol)(&mut guid, ptr::null_mut(), &mut interface) };
    if status.is_error() {
        return Err(Error::Firmware(
            "physical MP Services unavailable",
            status.as_usize(),
        ));
    }
    if interface.is_null() {
        return Err(invalid("physical MP Services null interface"));
    }
    let protocol = interface.cast::<mp::Protocol>();
    let mut total = 0;
    let mut enabled = 0;
    let mut current = usize::MAX;
    // SAFETY: firmware returned this live MP Services interface; outputs are
    // initialized stack scalars. Both functions only query this CPU/topology.
    let (count_status, who_status) = unsafe {
        (
            ((*protocol).get_number_of_processors)(protocol, &mut total, &mut enabled),
            ((*protocol).who_am_i)(protocol, &mut current),
        )
    };
    if count_status.is_error() {
        return Err(Error::Firmware(
            "physical GetNumberOfProcessors",
            count_status.as_usize(),
        ));
    }
    if who_status.is_error() {
        return Err(Error::Firmware("physical WhoAmI", who_status.as_usize()));
    }
    let mut flags = 0;
    if current < total {
        let mut info = core::mem::MaybeUninit::<mp::ProcessorInformation>::zeroed();
        // SAFETY: the validated current processor index and complete aligned
        // output storage satisfy GetProcessorInfo. Only its initialized legacy
        // status prefix is read, never an unrequested extended-information union.
        let status =
            unsafe { ((*protocol).get_processor_info)(protocol, current, info.as_mut_ptr()) };
        if status.is_error() {
            return Err(Error::Firmware(
                "physical GetProcessorInfo",
                status.as_usize(),
            ));
        }
        // SAFETY: successful firmware output initialized this u32 prefix field;
        // the allocation remains live, aligned, and unaliased on this stack.
        flags = unsafe { ptr::addr_of!((*info.as_ptr()).status_flag).read() };
    }
    if !single_bsp(total, enabled, current, flags) {
        let _ = writeln!(
            serial,
            "thin-hv: physical CPU ownership REJECT total={total} enabled={enabled} current={current} scope=bsp-only project_vmx=0"
        );
        return Err(Error::Firmware(
            "physical SMP ownership is not implemented",
            efi::Status::UNSUPPORTED.as_usize(),
        ));
    }
    serial.write_bytes(b"thin-hv: physical CPU ownership PASS total=1 enabled=1 current=0 scope=bsp-only physical_smp=0\n");
    Ok(())
}

#[cfg(all(test, feature = "physical-direct-vmx"))]
#[test]
fn physical_cpu_gate_never_treats_disabled_aps_as_owned() {
    assert!(single_bsp(1, 1, 0, 7));
    for (total, enabled, current, flags) in [
        (0, 0, 0, 7),
        (1, 0, 0, 7),
        (2, 1, 0, 7),
        (2, 2, 0, 7),
        (1, 2, 0, 7),
        (1, 1, 1, 7),
        (usize::MAX, 1, 0, 7),
        (1, 1, usize::MAX, 7),
        (1, 1, 0, 0),
        (1, 1, 0, 3),
        (1, 1, 0, 6),
        (1, 1, 0, 15),
    ] {
        assert!(!single_bsp(total, enabled, current, flags));
    }
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

    #[cfg(feature = "profile-direct-vmx")]
    #[test]
    fn boot_option_paths_resolve_only_current_esp_and_valid_file_nodes() {
        let mut hd = [0u8; 42];
        hd[..4].copy_from_slice(&[4, 1, 42, 0]);
        hd[24..40].fill(0x5a);
        hd[40..42].copy_from_slice(&[2, 2]);
        let prefix = [2, 1, 4, 0];
        let device = [prefix.as_slice(), &hd, &[0x7f, 0xff, 4, 0]].concat();
        let file = file_node("\\EFI\\Test\\NEXT.EFI");
        for path in [
            file.clone(),
            [&hd[..], &file].concat(),
            [&device[..device.len() - 4], &file].concat(),
        ] {
            let (requested, image) = super::boot_file_path(&path).unwrap();
            assert!(super::same_boot_device(requested, &device));
            assert!(image.same_file(&selected_path(&options("\\EFI\\Test\\NEXT.EFI")).unwrap()));
        }
        let mut other = hd;
        other[24] ^= 1;
        assert!(!super::same_boot_device(&other, &device));
        other = hd;
        other[40] = 3;
        assert!(!super::same_boot_device(&other, &device));
        other = hd;
        other[2] = 0;
        assert!(!super::same_boot_device(&other, &device));
        for malformed in [
            std::vec![],
            std::vec![0x7f, 0xff, 4, 0],
            std::vec![4, 1, 255, 255],
            [&[0x7f, 1, 4, 0][..], &file].concat(),
        ] {
            assert!(super::boot_file_path(&malformed).is_err());
        }
        let mut missing_end = file;
        missing_end.truncate(missing_end.len() - 4);
        assert!(super::boot_file_path(&missing_end).is_err());
        assert!(!super::same_boot_device(
            &hd,
            &[4, 1, 255, 255, 0x7f, 0xff, 4, 0]
        ));
        assert!(!super::same_boot_device(&hd, &[]));
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
