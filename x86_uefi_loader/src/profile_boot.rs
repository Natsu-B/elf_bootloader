//! Boot-time selection for the persistent primary-OS Direct backend.
//!
//! LoadOptions is either empty (keep the durable selection) or the exact
//! NUL-terminated UTF-16 word `windows` / `linux`. It is not guest BootNext.
//! Paths belong to this build/current ESP, not firmware enumeration order.

use crate::SerialPort;
use crate::chainload;
use crate::physical_chainload;
use crate::runtime_variables;
use core::ffi::c_void;
use core::fmt::Write;
use core::ptr;
use core::slice;
use r_efi::efi;
use uefi_variable_overlay::UefiProfile;

/// Bounded EFI_LOAD_OPTION view; descriptive text is neither logged nor copied.
struct BootOption<'a> {
    attributes: u32,
    path: &'a [u8],
    optional: &'a [u8],
}

impl<'a> BootOption<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, efi::Status> {
        if bytes.len() < 12 || bytes.len() > 8192 {
            return Err(efi::Status::COMPROMISED_DATA);
        }
        let attributes = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let path_size = usize::from(u16::from_le_bytes([bytes[4], bytes[5]]));
        // Description is terminated UTF-16, potentially unaligned in the store.
        let end = bytes[6..]
            .chunks_exact(2)
            .position(|word| word == [0, 0])
            .ok_or(efi::Status::COMPROMISED_DATA)?;
        let path_start = 6 + (end + 1) * 2;
        let path_end = path_start
            .checked_add(path_size)
            .ok_or(efi::Status::COMPROMISED_DATA)?;
        if path_size < 4 || path_size > 4096 || path_end > bytes.len() {
            return Err(efi::Status::COMPROMISED_DATA);
        }
        Ok(Self {
            attributes,
            path: &bytes[path_start..path_end],
            optional: &bytes[path_end..],
        })
    }

    /// BootNext is explicit; BootOrder automatically considers only active boot
    /// category entries, not application/recovery categories or inactive entries.
    fn automatic(&self) -> bool {
        self.attributes & 1 != 0 && self.attributes & 0x1f00 == 0
    }
}

/// One loaded, unstarted image; only its validated explicit selection is written.
pub(crate) struct SelectedBoot {
    /// Firmware-loaded primary OS image, still unstarted.
    pub(crate) image: efi::Handle,
    /// Namespace installed by the resident monitor after validating its handoff.
    pub(crate) profile: UefiProfile,
    runtime: *mut efi::RuntimeServices,
    services: *mut efi::BootServices,
    options: *mut c_void,
    boot_number: Option<u16>,
    saved_current: Option<Option<u16>>,
    explicit: bool,
}

impl SelectedBoot {
    /// Commit only after both target and runtime-monitor image validation.
    /// No periodic writes or volatile-memory default are needed on normal reboot.
    pub(crate) fn commit(&mut self) -> Result<(), chainload::Error> {
        if let Some(number) = self.boot_number {
            let previous = read_boot_current(self.runtime)?;
            write_boot_current(self.runtime, Some(number))?;
            self.saved_current = Some(previous);
        }
        if self.explicit {
            runtime_variables::write_boot_profile(self.runtime, self.profile)
                .map_err(|s| firmware("commit boot profile", s))?;
        }
        Ok(())
    }

    /// Only after the unstarted target has been unloaded on a failed launch.
    /// A failed unload must retain the option buffer the live image may reference.
    pub(crate) fn release(&mut self) -> Result<(), chainload::Error> {
        let restored = if let Some(previous) = self.saved_current.take() {
            write_boot_current(self.runtime, previous)
        } else {
            Ok(())
        };
        let freed = if self.options.is_null() {
            Ok(())
        } else {
            let options = core::mem::replace(&mut self.options, ptr::null_mut());
            chainload::free_pool(self.services, options)
        };
        restored.and(freed)
    }
}

/// BootCurrent is mutable boot-manager metadata, not machine/security identity.
/// It stays firmware-provided except when this manager actually selects Boot####.
fn read_boot_current(runtime: *mut efi::RuntimeServices) -> Result<Option<u16>, chainload::Error> {
    let mut name = chainload::ascii_uefi_path(b"BootCurrent\0");
    let mut guid = global_guid();
    let (mut value, mut size, mut attributes) = (0u16, 2usize, 0u32);
    // SAFETY: caller retained the live pre-EBS runtime table; exact terminated
    // key and aligned two-byte value/size/attribute slots remain owned locals.
    let status = unsafe {
        ((*runtime).get_variable)(
            name.as_mut_ptr(),
            &mut guid,
            &mut attributes,
            &mut size,
            ptr::addr_of_mut!(value).cast(),
        )
    };
    if status == efi::Status::NOT_FOUND {
        return Ok(None);
    }
    if status.is_error() {
        return Err(firmware("read BootCurrent", status));
    }
    if size != 2 || attributes != efi::VARIABLE_BOOTSERVICE_ACCESS | efi::VARIABLE_RUNTIME_ACCESS {
        return Err(firmware(
            "BootCurrent metadata",
            efi::Status::COMPROMISED_DATA,
        ));
    }
    Ok(Some(value))
}

fn global_guid() -> efi::Guid {
    efi::Guid::from_fields(
        0x8be4_df61,
        0x93ca,
        0x11d2,
        0xaa,
        0x0d,
        &[0, 0xe0, 0x98, 0x03, 0x2b, 0x8c],
    )
}

fn write_boot_current(
    runtime: *mut efi::RuntimeServices,
    number: Option<u16>,
) -> Result<(), chainload::Error> {
    let mut name = chainload::ascii_uefi_path(b"BootCurrent\0");
    let mut guid = global_guid();
    let mut value = number.unwrap_or(0);
    let (attributes, size) = if number.is_some() {
        (
            efi::VARIABLE_BOOTSERVICE_ACCESS | efi::VARIABLE_RUNTIME_ACCESS,
            2,
        )
    } else {
        (0, 0)
    };
    // SAFETY: this boot manager owns temporary BootCurrent publication before
    // EBS; the synchronous firmware call receives the exact key and live value.
    let status = unsafe {
        ((*runtime).set_variable)(
            name.as_mut_ptr(),
            &mut guid,
            attributes,
            size,
            ptr::addr_of_mut!(value).cast(),
        )
    };
    if status.is_error() {
        Err(firmware("write BootCurrent", status))
    } else {
        Ok(())
    }
}

/// Read only the selected namespace. No hooks, firmware-global BootOrder or
/// other profile participates in this boot-order decision.
fn read_variable(
    runtime: *mut efi::RuntimeServices,
    profile: UefiProfile,
    name: &[u16],
    data: &mut [u8],
) -> Result<Option<usize>, chainload::Error> {
    runtime_variables::read_profile_boot_variable(runtime, profile, name, data)
        .map_err(|status| firmware("profile boot variable", status))
}

/// Load one existing EFI_LOAD_OPTION. Unsupported layouts fail before entry;
/// only a missing/inactive ordered entry permits the next same-profile attempt.
fn load_option(
    image: efi::Handle,
    system: *mut efi::SystemTable,
    runtime: *mut efi::RuntimeServices,
    profile: UefiProfile,
    number: u16,
    automatic: bool,
    serial: &mut SerialPort,
) -> Result<Option<(efi::Handle, *mut c_void)>, chainload::Error> {
    let mut name = chainload::ascii_uefi_path(b"Boot0000\0");
    for digit in 0..4 {
        name[7 - digit] = u16::from(b"0123456789ABCDEF"[((number >> (digit * 4)) & 15) as usize]);
    }
    let mut data = [0u8; 8192];
    let Some(size) = read_variable(runtime, profile, &name[..8], &mut data)? else {
        return Ok(None);
    };
    let option =
        BootOption::parse(&data[..size]).map_err(|s| firmware("profile EFI_LOAD_OPTION", s))?;
    if automatic && !option.automatic() {
        return Ok(None);
    }
    let services = chainload::boot_services(system)?;
    let guest =
        match physical_chainload::load_profile_device_path(image, system, serial, option.path) {
            Ok(guest) => guest,
            Err(error) if error.is_missing_image() => return Ok(None),
            Err(error) => return Err(error),
        };
    let prepare = (|| {
        let loaded = chainload::loaded_image_protocol(guest, system)?;
        if option.optional.is_empty() {
            return Ok(ptr::null_mut());
        }
        let mut copy = ptr::null_mut();
        // SAFETY: Boot Services are live and return exclusively owned LoaderData
        // storage. Size is bounded by the checked 8-KiB load option, not guest RAM.
        let status = unsafe {
            ((*services).allocate_pool)(efi::LOADER_DATA, option.optional.len(), &mut copy)
        };
        if status.is_error() {
            return Err(firmware("boot optional-data allocation", status));
        }
        if copy.is_null() {
            return Err(firmware(
                "null boot optional-data allocation",
                efi::Status::DEVICE_ERROR,
            ));
        }
        // SAFETY: firmware returned sufficient, disjoint writable pool storage;
        // the unstarted child borrows that owned copy, never this stack scratch.
        // The caller retains it until target unload, or successful OS handoff.
        unsafe {
            ptr::copy_nonoverlapping(
                option.optional.as_ptr(),
                copy.cast::<u8>(),
                option.optional.len(),
            );
            (*loaded).load_options = copy;
            (*loaded).load_options_size = option.optional.len() as u32;
        }
        Ok(copy)
    })();
    match prepare {
        Ok(options) => Ok(Some((guest, options))),
        Err(error) => {
            chainload::unload_image(services, guest)?;
            Err(error)
        }
    }
}

/// BootNext is consumed before attempting it, then BootOrder is considered in
/// its recorded order. No selected entry ever changes the primary-OS selector.
fn load_ordered(
    image: efi::Handle,
    system: *mut efi::SystemTable,
    runtime: *mut efi::RuntimeServices,
    profile: UefiProfile,
    serial: &mut SerialPort,
) -> Result<Option<(efi::Handle, *mut c_void, u16)>, chainload::Error> {
    let next_name = chainload::ascii_uefi_path(b"BootNext\0");
    let mut next = [0u8; 2];
    if let Some(size) = read_variable(runtime, profile, &next_name[..8], &mut next)? {
        if size != 2 {
            return Err(firmware(
                "profile BootNext size",
                efi::Status::COMPROMISED_DATA,
            ));
        }
        runtime_variables::consume_profile_boot_next(runtime, profile)
            .map_err(|s| firmware("consume profile BootNext", s))?;
        let number = u16::from_le_bytes(next);
        if let Some((guest, options)) =
            load_option(image, system, runtime, profile, number, false, serial)?
        {
            let _ = writeln!(
                serial,
                "thin-hv: boot variable source=BootNext index={number:04X}"
            );
            return Ok(Some((guest, options, number)));
        }
    }
    let order_name = chainload::ascii_uefi_path(b"BootOrder\0");
    let mut order = [0u8; 512];
    let Some(size) = read_variable(runtime, profile, &order_name[..9], &mut order)? else {
        return Ok(None);
    };
    if size == 0 || size % 2 != 0 {
        return Err(firmware(
            "profile BootOrder size",
            efi::Status::COMPROMISED_DATA,
        ));
    }
    for value in order[..size].chunks_exact(2) {
        let number = u16::from_le_bytes([value[0], value[1]]);
        if let Some((guest, options)) =
            load_option(image, system, runtime, profile, number, true, serial)?
        {
            let _ = writeln!(
                serial,
                "thin-hv: boot variable source=BootOrder index={number:04X}"
            );
            return Ok(Some((guest, options, number)));
        }
    }
    Err(firmware(
        "profile BootOrder exhausted",
        efi::Status::NOT_FOUND,
    ))
}

fn firmware(operation: &'static str, status: efi::Status) -> chainload::Error {
    chainload::Error::Firmware(operation, status.as_usize())
}

/// Decode only the explicit boot UI command; malformed data never selects an OS.
fn explicit_profile(bytes: &[u8]) -> Result<Option<UefiProfile>, efi::Status> {
    if bytes.is_empty() {
        return Ok(None);
    }
    for (name, profile) in [
        (b"windows".as_slice(), UefiProfile::Windows),
        (b"linux".as_slice(), UefiProfile::Linux),
    ] {
        if bytes.len() == (name.len() + 1) * 2
            && bytes[..name.len() * 2]
                .chunks_exact(2)
                .zip(name)
                .all(|(word, &ascii)| word == [ascii, 0])
            && bytes[name.len() * 2..] == [0, 0]
        {
            return Ok(Some(profile));
        }
    }
    Err(efi::Status::INVALID_PARAMETER)
}

/// Missing Linux configuration is an error, never another distro/ESP/profile.
fn configured_path(profile: UefiProfile, linux: Option<&str>) -> Result<&str, efi::Status> {
    match profile {
        UefiProfile::Windows => Ok("\\EFI\\Microsoft\\Boot\\bootmgfw.efi"),
        UefiProfile::Linux => linux.ok_or(efi::Status::NOT_FOUND),
    }
}

/// Select and load, but do not commit the selector or install runtime hooks yet.
pub(crate) fn load_selected(
    image: efi::Handle,
    system: *mut efi::SystemTable,
    serial: &mut SerialPort,
) -> Result<SelectedBoot, chainload::Error> {
    let loaded = chainload::loaded_image_protocol(image, system)?;
    // SAFETY: shared lookup validated the live system and LoadedImage interface.
    // LoadOptions is borrowed only before launching any child or exiting firmware.
    let (size, options, runtime) = unsafe {
        (
            (*loaded).load_options_size as usize,
            (*loaded).load_options,
            (*system).runtime_services,
        )
    };
    if size > 16
        || size % 2 != 0
        || (size != 0 && options.is_null())
        || (options as usize).checked_add(size).is_none()
        || runtime.is_null()
    {
        return Err(firmware(
            "boot profile options/table",
            efi::Status::INVALID_PARAMETER,
        ));
    }
    let bytes = if size == 0 {
        &[]
    } else {
        // SAFETY: firmware owns this bounded, non-null LoadOptions allocation;
        // byte reads require no alignment and the pointer range cannot wrap.
        unsafe { slice::from_raw_parts(options.cast::<u8>(), size) }
    };
    let explicit = explicit_profile(bytes).map_err(|s| firmware("boot profile options", s))?;
    let profile = match explicit {
        Some(profile) => profile,
        None => runtime_variables::read_boot_profile(runtime)
            .map_err(|s| firmware("read boot profile", s))?
            .ok_or(firmware(
                "boot profile requires explicit selection",
                efi::Status::NOT_FOUND,
            ))?,
    };
    let services = chainload::boot_services(system)?;
    let ordered = if explicit.is_none() {
        load_ordered(image, system, runtime, profile, serial)?
    } else {
        None
    };
    let (guest, options, boot_number) = if let Some((guest, options, number)) = ordered {
        (guest, options, Some(number))
    } else {
        let path = configured_path(profile, option_env!("THIN_HV_LINUX_EFI_PATH"))
            .map_err(|s| firmware("configured profile path", s))?;
        (
            physical_chainload::load_profile_path(image, system, serial, path)?,
            ptr::null_mut(),
            None,
        )
    };
    let _ = writeln!(
        serial,
        "thin-hv: boot profile={} source={} scope=current-esp",
        profile.id().0,
        if explicit.is_some() {
            "explicit"
        } else {
            "persistent"
        }
    );
    Ok(SelectedBoot {
        image: guest,
        profile,
        runtime,
        services,
        options,
        boot_number,
        saved_current: None,
        explicit: explicit.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_option_bounds_preserve_optional_bytes_and_automatic_category() {
        let mut bytes = std::vec![1, 0, 0, 0, 4, 0, 0, 0, 0x7f, 0xff, 4, 0, 0xa5, 0];
        let option = BootOption::parse(&bytes).unwrap();
        assert!(option.automatic());
        assert_eq!(option.path, &[0x7f, 0xff, 4, 0]);
        assert_eq!(option.optional, &[0xa5, 0]);
        for flags in [0u32, 0x101, 0x1f01] {
            bytes[..4].copy_from_slice(&flags.to_le_bytes());
            assert!(!BootOption::parse(&bytes).unwrap().automatic());
        }
        for size in [0u16, 3, 4097, u16::MAX] {
            bytes[4..6].copy_from_slice(&size.to_le_bytes());
            assert!(BootOption::parse(&bytes).is_err());
        }
        for size in 0..12 {
            assert!(BootOption::parse(&bytes[..size]).is_err());
        }
        assert!(BootOption::parse(&[0xff; 8192]).is_err());
        bytes[4..6].copy_from_slice(&4u16.to_le_bytes());
        bytes.resize(8192, 0x5a);
        assert_eq!(BootOption::parse(&bytes).unwrap().optional.len(), 8180);
        bytes.push(0);
        assert!(BootOption::parse(&bytes).is_err());
    }

    #[test]
    fn selection_is_explicit_or_persistent_never_boot_next_or_path_guessing() {
        let encode = |s: &str| {
            s.encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect::<std::vec::Vec<_>>()
        };
        assert_eq!(explicit_profile(&[]), Ok(None));
        for (name, profile) in [
            ("windows", UefiProfile::Windows),
            ("linux", UefiProfile::Linux),
        ] {
            let bytes = encode(name);
            assert_eq!(explicit_profile(&bytes), Ok(Some(profile)));
            for len in 1..bytes.len() {
                assert!(explicit_profile(&bytes[..len]).is_err());
            }
            let mut longer = bytes.clone();
            longer.push(0);
            assert!(explicit_profile(&longer).is_err());
        }
        for name in [
            "",
            "Windows",
            "windows ",
            "linux\0windows",
            "BootNext",
            "2",
            "\\EFI\\BOOT\\BOOTX64.EFI",
        ] {
            assert!(explicit_profile(&encode(name)).is_err());
        }
        assert_eq!(
            configured_path(UefiProfile::Windows, None),
            Ok("\\EFI\\Microsoft\\Boot\\bootmgfw.efi")
        );
        assert_eq!(
            configured_path(UefiProfile::Linux, None),
            Err(efi::Status::NOT_FOUND)
        );
        assert_eq!(
            configured_path(UefiProfile::Linux, Some("\\EFI\\ubuntu\\shimx64.efi")),
            Ok("\\EFI\\ubuntu\\shimx64.efi")
        );
    }
}
