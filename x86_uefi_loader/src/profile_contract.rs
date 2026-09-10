//! Disposable QEMU firmware-backed profile contract, including real resets.
//! The runner loads this as a runtime driver on a fresh, private variable store.
//! No VMX, ExitBootServices, installed OS or security-variable write occurs.

#![cfg_attr(not(test), no_main)]
#![cfg_attr(not(test), no_std)]

mod chainload;
mod runtime_variables;

use core::fmt::Write;
use core::ptr;
use mutex::SpinLock;
use r_efi::efi;
use uefi_variable_overlay::UefiProfile;
use uefi_variable_overlay::is_profile_private;

/// NV boot entries are also visible to the OS runtime variable API.
const ATTR: u32 =
    efi::VARIABLE_NON_VOLATILE | efi::VARIABLE_BOOTSERVICE_ACCESS | efi::VARIABLE_RUNTIME_ACCESS;
/// Test progress is persistent but only accessed before EBS.
const PHASE_ATTR: u32 = efi::VARIABLE_NON_VOLATILE | efi::VARIABLE_BOOTSERVICE_ACCESS;
/// Independent fixture phase, never a firmware boot selector.
const PHASE_NAME: &[u8] = b"ProfileContractPhase";
/// The same real firmware namespaces used by the production hooks.
const GLOBAL: efi::Guid = efi::Guid::from_fields(
    0x8be4_df61,
    0x93ca,
    0x11d2,
    0xaa,
    0x0d,
    &[0, 0xe0, 0x98, 0x03, 0x2b, 0x8c],
);
const PROJECT: efi::Guid = efi::Guid::from_fields(
    0xd7e7_166a,
    0x574a,
    0x4c70,
    0xa3,
    0xd0,
    &[0x55, 0xd8, 0xd6, 0x6d, 0x3a, 0x42],
);
const SECURITY: efi::Guid = efi::Guid::from_fields(
    0xd719_b2cb,
    0x3d3a,
    0x4596,
    0xa3,
    0xbc,
    &[0xda, 0xd0, 0x0e, 0x67, 0x65, 0x6f],
);
/// Bounded scratch avoids a large UEFI stack or allocating confidential payloads.
static SECURITY_SCRATCH: SpinLock<[u8; 65_536]> = SpinLock::new([0; 65_536]);

/// Polling output is outside VMX and never emits firmware variable contents.
struct Serial;
impl core::fmt::Write for Serial {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for byte in text.bytes() {
            let mut ready = false;
            for _ in 0..65_536 {
                let status: u8;
                // SAFETY: this x86 UEFI fixture runs at CPL0; COM1 is reserved by
                // the QEMU runner. The finite polling loop tolerates missing UARTs.
                unsafe {
                    core::arch::asm!("in al, dx", in("dx") 0x3fdu16, out("al") status, options(nomem, nostack, preserves_flags))
                };
                if status != 0xff && status & 0x20 != 0 {
                    ready = true;
                    break;
                }
            }
            if !ready {
                return Err(core::fmt::Error);
            }
            // SAFETY: COM1 transmitter is ready; the fixture owns this QEMU UART.
            unsafe {
                core::arch::asm!("out dx, al", in("dx") 0x3f8u16, in("al") byte, options(nomem, nostack, preserves_flags))
            };
        }
        Ok(())
    }
}

/// Fails a contract without panicking or changing any firmware error status.
#[track_caller]
fn require(condition: bool) -> Result<(), efi::Status> {
    if condition {
        Ok(())
    } else {
        let _ = writeln!(
            Serial,
            "thin-hv: profile contract FAIL assertion line={}",
            core::panic::Location::caller().line()
        );
        Err(efi::Status::COMPROMISED_DATA)
    }
}

/// Owns the firmware interfaces until reset; this fixture never calls EBS.
struct Fixture {
    system: *mut efi::SystemTable,
    runtime: *mut efi::RuntimeServices,
    image_base: u64,
    image_size: u64,
}

impl Fixture {
    /// Calls the same hooks as the monitor, restoring all pointers even on failure.
    fn profile(
        &self,
        profile: UefiProfile,
        test: impl FnOnce() -> Result<(), efi::Status>,
    ) -> Result<(), efi::Status> {
        let info = self.variable_info()?;
        let overlay = runtime_variables::install(
            self.system,
            profile.id(),
            self.image_base,
            self.image_size,
        )?;
        let _ = writeln!(
            Serial,
            "thin-hv: profile contract view={} mat_patches={}",
            profile.id().0,
            overlay.memory_attribute_patch_count()
        );
        let result = (|| {
            require(self.variable_info()? == info)?;
            test()
        })();
        let rollback = overlay.rollback();
        if rollback.is_err() {
            let _ = writeln!(Serial, "thin-hv: profile contract rollback FAIL");
        }
        result.and(rollback)
    }

    /// QueryVariableInfo stays the real shared firmware capacity, not a fake quota.
    fn variable_info(&self) -> Result<(u64, u64, u64), efi::Status> {
        let (mut maximum, mut remaining, mut value_maximum) = (0, 0, 0);
        // SAFETY: the live pre-EBS firmware table receives three aligned owned
        // output slots and a supported variable attribute combination.
        let status = unsafe {
            ((*self.runtime).query_variable_info)(
                ATTR,
                &mut maximum,
                &mut remaining,
                &mut value_maximum,
            )
        };
        if status.is_error() {
            return Err(status);
        }
        require(maximum > 0 && value_maximum > 0 && remaining <= maximum)?;
        Ok((maximum, remaining, value_maximum))
    }

    fn get(&self, ascii: &[u8], mut guid: efi::Guid, data: &mut [u8]) -> (efi::Status, usize, u32) {
        let mut name = [0u16; 64];
        if ascii.len() >= name.len() {
            return (efi::Status::INVALID_PARAMETER, 0, 0);
        }
        for (to, &from) in name.iter_mut().zip(ascii) {
            *to = u16::from(from);
        }
        let mut size = data.len();
        let mut attributes = 0;
        // SAFETY: this live table and owned buffers remain valid throughout the
        // synchronous call. Names are bounded and terminated, even for size probes.
        let status = unsafe {
            ((*self.runtime).get_variable)(
                name.as_mut_ptr(),
                &mut guid,
                &mut attributes,
                &mut size,
                data.as_mut_ptr().cast(),
            )
        };
        (status, size, attributes)
    }

    fn set(&self, ascii: &[u8], mut guid: efi::Guid, attributes: u32, data: &[u8]) -> efi::Status {
        let mut name = [0u16; 64];
        if ascii.len() >= name.len() {
            return efi::Status::INVALID_PARAMETER;
        }
        for (to, &from) in name.iter_mut().zip(ascii) {
            *to = u16::from(from);
        }
        // SAFETY: fixture-owned name/GUID/data live across this synchronous call;
        // the firmware table is valid before EBS, which this fixture never invokes.
        unsafe {
            ((*self.runtime).set_variable)(
                name.as_mut_ptr(),
                &mut guid,
                attributes,
                data.len(),
                data.as_ptr().cast_mut().cast(),
            )
        }
    }

    fn value(&self, name: &[u8], expected: &[u8]) -> Result<(), efi::Status> {
        let mut data = [0xaau8; 32];
        require(expected.len() <= data.len())?;
        let (status, size, attr) = self.get(name, GLOBAL, &mut data);
        require(
            status == efi::Status::SUCCESS
                && size == expected.len()
                && attr == ATTR
                && data[..size] == *expected,
        )
    }

    /// Compares security state without dumping keys, certificates or variable data.
    fn security(&self) -> Result<[(usize, usize, u32, u64); 9], efi::Status> {
        let mut result = [(0, 0, 0, 0); 9];
        let mut scratch = SECURITY_SCRATCH.lock();
        for (slot, (name, guid)) in result.iter_mut().zip([
            (b"SecureBoot".as_slice(), GLOBAL),
            (b"SetupMode".as_slice(), GLOBAL),
            (b"AuditMode".as_slice(), GLOBAL),
            (b"DeployedMode".as_slice(), GLOBAL),
            (b"PK".as_slice(), GLOBAL),
            (b"KEK".as_slice(), GLOBAL),
            (b"db".as_slice(), SECURITY),
            (b"dbx".as_slice(), SECURITY),
            (b"BootCurrent".as_slice(), GLOBAL),
        ]) {
            let (status, size, attr) = self.get(name, guid, &mut scratch[..]);
            require(status == efi::Status::SUCCESS || status == efi::Status::NOT_FOUND)?;
            let hash = if status == efi::Status::SUCCESS {
                require(size <= scratch.len())?;
                scratch[..size].iter().fold(0xcbf29ce484222325u64, |h, &b| {
                    (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
                })
            } else {
                0
            };
            *slot = (
                status.as_usize(),
                if status.is_error() { 0 } else { size },
                attr,
                hash,
            );
            scratch.fill(0);
        }
        Ok(result)
    }

    /// Enumerates the entire view twice with stable names/order and no raw keys.
    fn enumeration(&self, expected: &[&[u8]]) -> Result<(), efi::Status> {
        let mut hashes = [0u64; 2];
        for hash in &mut hashes {
            let mut name = [0u16; 2048];
            let mut guid = GLOBAL;
            let mut seen = 0u32;
            let mut ended = false;
            for _ in 0..512 {
                let previous = name;
                let previous_guid = guid;
                let mut size = name.len() * 2;
                // SAFETY: the live firmware table receives the previously returned
                // cursor in a bounded writable UTF-16 buffer, plus size/GUID outputs.
                let status = unsafe {
                    ((*self.runtime).get_next_variable_name)(
                        &mut size,
                        name.as_mut_ptr(),
                        &mut guid,
                    )
                };
                if status == efi::Status::NOT_FOUND {
                    ended = true;
                    break;
                }
                require(
                    status == efi::Status::SUCCESS
                        && size >= 2
                        && size <= name.len() * 2
                        && size % 2 == 0,
                )?;
                let len = size / 2 - 1;
                require(name[len] == 0 && !name[..len].contains(&0) && guid != PROJECT)?;
                if guid == GLOBAL
                    && is_profile_private(
                        uefi_variable_overlay::EFI_GLOBAL_VARIABLE_GUID,
                        &name[..len],
                    )
                {
                    let index = expected
                        .iter()
                        .position(|s| {
                            s.len() == len && s.iter().zip(&name).all(|(&a, &b)| u16::from(a) == b)
                        })
                        .ok_or(efi::Status::COMPROMISED_DATA)?;
                    require(index < 32 && seen & (1 << index) == 0)?;
                    seen |= 1 << index;
                }
                for &unit in &name[..len] {
                    *hash = hash.wrapping_mul(131).wrapping_add(u64::from(unit));
                }
                // Retry the same cursor with room for the input but not the output
                // whenever the next name is longer. Firmware must leave it intact.
                let prior_units = previous
                    .iter()
                    .position(|&x| x == 0)
                    .ok_or(efi::Status::COMPROMISED_DATA)?
                    + 1;
                if prior_units * 2 < size {
                    let mut retry = previous;
                    let mut retry_guid = previous_guid;
                    let mut short_size = prior_units * 2;
                    // SAFETY: the retry buffer contains the same terminated cursor;
                    // declared capacity fits its input and is within the real allocation.
                    let retry_status = unsafe {
                        ((*self.runtime).get_next_variable_name)(
                            &mut short_size,
                            retry.as_mut_ptr(),
                            &mut retry_guid,
                        )
                    };
                    require(
                        retry_status == efi::Status::BUFFER_TOO_SMALL
                            && short_size == size
                            && retry == previous
                            && retry_guid == previous_guid,
                    )?;
                }
            }
            require(ended && seen == (1u32 << expected.len()) - 1)?;
        }
        require(hashes[0] == hashes[1])
    }

    /// Exercises one atomic update failure and then releases all filler keys.
    fn full_store(&self) -> Result<(), efi::Status> {
        let mut inserted = 0usize;
        let mut write_error = Ok(());
        let mut full = false;
        let mut name = *b"Boot8000";
        for index in 0..256usize {
            for digit in 0..3 {
                name[7 - digit] = b"0123456789ABCDEF"[(index >> (digit * 4)) & 15];
            }
            let status = self.set(&name, GLOBAL, ATTR, &[0x6d; 1024]);
            if status == efi::Status::OUT_OF_RESOURCES {
                full = true;
                break;
            }
            if status.is_error() {
                write_error = Err(status);
                break;
            }
            inserted += 1;
        }
        // No budget increase or false PASS when this firmware has a larger store.
        let result = (|| {
            write_error?;
            require(full && inserted > 0)?;
            require(
                self.set(b"Boot0022", GLOBAL, ATTR, &[0x7d; 4096]) == efi::Status::OUT_OF_RESOURCES,
            )?;
            self.value(b"Boot0022", &[0x22, 0])?;
            let _ = writeln!(
                Serial,
                "thin-hv: profile contract storage-full PASS entries={inserted}"
            );
            Ok(())
        })();
        let mut cleanup = Ok(());
        for index in 0..inserted {
            for digit in 0..3 {
                name[7 - digit] = b"0123456789ABCDEF"[(index >> (digit * 4)) & 15];
            }
            let status = self.set(&name, GLOBAL, 0, &[]);
            if status.is_error() {
                cleanup = Err(status);
            }
        }
        result.and(cleanup)
    }

    fn run(&self) -> Result<(), efi::Status> {
        let security = self.security()?;
        let mut phase = [0u8];
        let (status, size, attr) = self.get(PHASE_NAME, PROJECT, &mut phase);
        if status == efi::Status::NOT_FOUND {
            phase[0] = 0;
        } else {
            require(
                status == efi::Status::SUCCESS && size == 1 && attr == PHASE_ATTR && phase[0] <= 2,
            )?;
        }
        let _ = writeln!(Serial, "thin-hv: profile contract phase={} begin", phase[0]);
        if phase[0] == 0 {
            require(runtime_variables::read_boot_profile(self.runtime)?.is_none())?;
            runtime_variables::write_boot_profile(self.runtime, UefiProfile::Windows)?;
            self.profile(UefiProfile::Windows, || {
                self.enumeration(&[])?;
                require(self.set(b"Boot0011", GLOBAL, ATTR, &[0x11, 0]) == efi::Status::SUCCESS)?;
                require(self.set(b"BootOrder", GLOBAL, ATTR, &[0x11, 0]) == efi::Status::SUCCESS)?;
                require(self.set(b"BootNext", GLOBAL, ATTR, &[2, 0]) == efi::Status::SUCCESS)?;
                require(
                    runtime_variables::read_boot_profile(self.runtime)?
                        == Some(UefiProfile::Windows),
                )?;
                self.enumeration(&[b"Boot0011", b"BootOrder", b"BootNext"])?;
                require(self.security()? == security)
            })?;
            self.profile(UefiProfile::Linux, || {
                self.enumeration(&[])?;
                require(self.get(b"Boot0011", GLOBAL, &mut []).0 == efi::Status::NOT_FOUND)?;
                require(self.set(b"Boot0022", GLOBAL, ATTR, &[0x22, 0]) == efi::Status::SUCCESS)?;
                require(self.set(b"BootOrder", GLOBAL, ATTR, &[0x22, 0]) == efi::Status::SUCCESS)?;
                self.enumeration(&[b"Boot0022", b"BootOrder"])?;
                require(self.security()? == security)
            })?;
        }
        self.profile(UefiProfile::Windows, || {
            self.value(b"Boot0011", &[0x11, 0])?;
            self.value(
                b"BootOrder",
                if phase[0] == 2 {
                    &[0x11, 0, 0x33, 0]
                } else {
                    &[0x11, 0]
                },
            )?;
            require(self.security()? == security)
        })?;
        self.profile(UefiProfile::Linux, || {
            self.value(b"Boot0022", &[0x22, 0])?;
            self.value(b"BootOrder", &[0x22, 0])?;
            require(self.security()? == security)
        })?;
        require(
            runtime_variables::read_boot_profile(self.runtime)?
                == Some(if phase[0] == 1 {
                    UefiProfile::Linux
                } else {
                    UefiProfile::Windows
                }),
        )?;
        if phase[0] == 1 {
            self.profile(UefiProfile::Linux, || self.full_store())?;
            self.profile(UefiProfile::Windows, || {
                require(
                    self.set(
                        b"BootOrder",
                        GLOBAL,
                        ATTR | efi::VARIABLE_APPEND_WRITE,
                        &[0x33, 0],
                    ) == efi::Status::SUCCESS,
                )?;
                require(
                    self.set(b"BootOrder", GLOBAL, ATTR | efi::VARIABLE_APPEND_WRITE, &[])
                        == efi::Status::SUCCESS,
                )?;
                self.value(b"BootOrder", &[0x11, 0, 0x33, 0])?;
                let mut byte = [0xaa];
                require(
                    self.get(b"BootOrder", GLOBAL, &mut byte)
                        == (efi::Status::BUFFER_TOO_SMALL, 4, ATTR)
                        && byte == [0xaa],
                )?;
                require(self.set(b"BootNext", GLOBAL, 0, &[]) == efi::Status::SUCCESS)?;
                require(self.get(b"BootNext", GLOBAL, &mut []).0 == efi::Status::NOT_FOUND)?;
                self.enumeration(&[b"Boot0011", b"BootOrder"])
            })?;
        }
        if phase[0] == 2 {
            self.profile(UefiProfile::Windows, || {
                require(self.get(b"BootNext", GLOBAL, &mut []).0 == efi::Status::NOT_FOUND)?;
                self.enumeration(&[b"Boot0011", b"BootOrder"])
            })?;
            let _ = writeln!(
                Serial,
                "thin-hv: profile contract PASS profiles=2 resets=2 persistence=firmware security=unchanged"
            );
            // SAFETY: the fixture owns the disposable VM; hooks are rolled back,
            // all nonvolatile writes returned and no locks or active calls remain.
            unsafe {
                ((*self.runtime).reset_system)(
                    efi::RESET_SHUTDOWN,
                    efi::Status::SUCCESS,
                    0,
                    ptr::null_mut(),
                )
            };
        } else {
            runtime_variables::write_boot_profile(
                self.runtime,
                if phase[0] == 0 {
                    UefiProfile::Linux
                } else {
                    UefiProfile::Windows
                },
            )?;
            require(
                self.set(PHASE_NAME, PROJECT, PHASE_ATTR, &[phase[0] + 1]) == efi::Status::SUCCESS,
            )?;
            let _ = writeln!(Serial, "thin-hv: profile contract reset={}", phase[0] + 1);
            // SAFETY: only this disposable firmware store is written; the phase
            // is durable, hooks restored, and no fixture lock or call is active.
            unsafe {
                ((*self.runtime).reset_system)(
                    if phase[0] == 0 {
                        efi::RESET_COLD
                    } else {
                        efi::RESET_WARM
                    },
                    efi::Status::SUCCESS,
                    0,
                    ptr::null_mut(),
                )
            };
        }
        Err(efi::Status::DEVICE_ERROR)
    }
}

/// Boot application loads the runtime copy on its own disposable ESP.
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub extern "efiapi" fn efi_main(image: efi::Handle, system: *mut efi::SystemTable) -> efi::Status {
    let result = (|| {
        let loaded =
            chainload::loaded_image_protocol(image, system).map_err(chainload::Error::status)?;
        // SAFETY: firmware returned this live LoadedImage protocol; system is
        // validated by the shared helper and remains available before EBS.
        let (base, size, code, data, runtime, device) = unsafe {
            (
                (*loaded).image_base as u64,
                (*loaded).image_size,
                (*loaded).image_code_type,
                (*loaded).image_data_type,
                (*system).runtime_services,
                (*loaded).device_handle,
            )
        };
        if code == efi::LOADER_CODE && data == efi::LOADER_DATA {
            let utilities = chainload::device_path_utilities_protocol(system)
                .map_err(chainload::Error::status)?;
            let driver = chainload::load_image_on_device(
                image,
                system,
                device,
                utilities,
                &chainload::ascii_uefi_path(b"\\EFI\\BOOT\\MONITORX64.EFI\0"),
            )
            .map_err(chainload::Error::status)?;
            let metadata = (|| {
                let loaded = chainload::loaded_image_protocol(driver, system)
                    .map_err(chainload::Error::status)?;
                // SAFETY: LoadImage created this unstarted, live driver; reading
                // its copied memory types precedes StartImage and any hook install.
                let (code, data) =
                    unsafe { ((*loaded).image_code_type, (*loaded).image_data_type) };
                require(code == efi::RUNTIME_SERVICES_CODE && data == efi::RUNTIME_SERVICES_DATA)
            })();
            if let Err(status) = metadata {
                let services =
                    chainload::boot_services(system).map_err(chainload::Error::status)?;
                chainload::unload_image(services, driver).map_err(chainload::Error::status)?;
                return Err(status);
            }
            // Runtime copy resets the machine or returns an error; shared
            // StartImage retirement handles firmware errors and ExitData.
            return chainload::start_image(driver, system)
                .map(|_| ())
                .map_err(chainload::Error::status);
        }
        let _ = writeln!(
            Serial,
            "thin-hv: uefi entry\nthin-hv: backend=uefi-profile-contract project_vmx=0"
        );
        require(
            code == efi::RUNTIME_SERVICES_CODE
                && data == efi::RUNTIME_SERVICES_DATA
                && !runtime.is_null(),
        )?;
        let size = size
            .checked_add(4095)
            .ok_or(efi::Status::OUT_OF_RESOURCES)?
            & !4095;
        Fixture {
            system,
            runtime,
            image_base: base,
            image_size: size,
        }
        .run()
    })();
    let status = result.err().unwrap_or(efi::Status::DEVICE_ERROR);
    let _ = writeln!(
        Serial,
        "thin-hv: profile contract FAIL status={:#x}",
        status.as_usize()
    );
    status
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    let _ = writeln!(Serial, "thin-hv: profile contract FAIL panic");
    loop {
        core::hint::spin_loop();
    }
}
