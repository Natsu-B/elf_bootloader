//! Boot-time selection for the persistent primary-OS Direct backend.
//!
//! LoadOptions is either empty (keep the durable selection) or the exact
//! NUL-terminated UTF-16 word `windows` / `linux`. It is not guest BootNext.
//! Paths belong to this build/current ESP, not firmware enumeration order.

use crate::SerialPort;
use crate::chainload;
use crate::physical_chainload;
use crate::runtime_variables;
use core::fmt::Write;
use core::slice;
use r_efi::efi;
use uefi_variable_overlay::UefiProfile;

/// One loaded, unstarted image; only its validated explicit selection is written.
pub(crate) struct SelectedBoot {
    /// Firmware-loaded primary OS image, still unstarted.
    pub(crate) image: efi::Handle,
    /// Namespace installed by the resident monitor after validating its handoff.
    pub(crate) profile: UefiProfile,
    runtime: *mut efi::RuntimeServices,
    explicit: bool,
}

impl SelectedBoot {
    /// Commit only after both target and runtime-monitor image validation.
    /// No periodic writes or volatile-memory default are needed on normal reboot.
    pub(crate) fn commit(&self) -> Result<(), chainload::Error> {
        if self.explicit {
            runtime_variables::write_boot_profile(self.runtime, self.profile)
                .map_err(|s| firmware("commit boot profile", s))?;
        }
        Ok(())
    }
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
    let path = configured_path(profile, option_env!("THIN_HV_LINUX_EFI_PATH"))
        .map_err(|s| firmware("configured profile path", s))?;
    let guest = physical_chainload::load_profile_path(image, system, serial, path)?;
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
        explicit: explicit.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
