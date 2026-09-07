//! Disposable QEMU-only physical-chainload policy fixtures.
//!
//! The driver and payload are separate builds of this existing test package.
//! Neither uses variable services, modifies disks, or enables VMX. The driver
//! connects only mass-storage PCI controllers through the firmware Driver Model
//! in its disposable QEMU VM; those driver bindings are volatile fixture state.
//! starts the unchanged project loader with five finite LoadOptions cases. The
//! runner owns the two temporary ESPs and verifies the ordered serial transcript.

#![cfg_attr(not(test), no_main)]
#![cfg_attr(not(test), no_std)]

#[cfg(any(
    all(
        feature = "physical-policy-driver",
        feature = "physical-policy-payload"
    ),
    not(any(
        feature = "physical-policy-driver",
        feature = "physical-policy-payload"
    ))
))]
compile_error!("select exactly one physical policy fixture role");

use core::fmt;
use core::fmt::Write;
use core::ptr;
use r_efi::efi;
use x86_64_hal::cpu;

/// Finite COM1 wait: an absent UART must not hang this test image.
const SERIAL_POLLS: usize = 100_000;
/// Serial output dedicated to the disposable test VM.
struct Serial;

impl Serial {
    /// Initializes only the test VM's COM1 interface.
    fn init(&mut self) {
        // SAFETY: the QEMU fixture runs at CPL0 and exclusively owns COM1.
        unsafe {
            cpu::outb(0x3f9, 0);
            cpu::outb(0x3fb, 0x80);
            cpu::outb(0x3f8, 1);
            cpu::outb(0x3f9, 0);
            cpu::outb(0x3fb, 3);
            cpu::outb(0x3fa, 0xc7);
            cpu::outb(0x3fc, 0x0b);
        }
    }

    /// Writes one byte or reports a bounded UART failure.
    fn byte(&mut self, byte: u8) -> fmt::Result {
        for _ in 0..SERIAL_POLLS {
            // SAFETY: the QEMU fixture runs at CPL0 and owns this UART port.
            let ready = unsafe { cpu::inb(0x3fd) };
            if ready != 0xff && ready & 0x20 != 0 {
                // SAFETY: COM1 reported a ready transmitter in this iteration.
                unsafe { cpu::outb(0x3f8, byte) };
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(fmt::Error)
    }
}

impl Write for Serial {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        for byte in value.bytes() {
            if byte == b'\n' {
                self.byte(b'\r')?;
            }
            self.byte(byte)?;
        }
        Ok(())
    }
}

/// A firmware failure with only a fixed operation name, never firmware data.
type Failure = (&'static str, efi::Status);

/// Returns a live table without transitioning away from Boot Services.
fn services(table: *mut efi::SystemTable) -> Result<*mut efi::BootServices, Failure> {
    if table.is_null() {
        return Err(("SystemTable", efi::Status::INVALID_PARAMETER));
    }
    // SAFETY: the firmware entry point supplies a live SystemTable to this image;
    // no fixture path calls ExitBootServices or retains the pointer after return.
    let boot = unsafe { (*table).boot_services };
    if boot.is_null() {
        return Err(("BootServices", efi::Status::UNSUPPORTED));
    }
    Ok(boot)
}

/// Looks up a live image interface, rejecting null successful results.
fn loaded_image(
    boot: *mut efi::BootServices,
    handle: efi::Handle,
) -> Result<*mut efi::protocols::loaded_image::Protocol, Failure> {
    if handle.is_null() {
        return Err(("image handle", efi::Status::INVALID_PARAMETER));
    }
    let mut guid = efi::protocols::loaded_image::PROTOCOL_GUID;
    let mut result = ptr::null_mut();
    // SAFETY: boot is the checked live table; the handle belongs to this running
    // image tree and the GUID/output slots remain valid for the synchronous call.
    let status = unsafe { ((*boot).handle_protocol)(handle, &mut guid, &mut result) };
    if status.is_error() {
        return Err(("LoadedImage", status));
    }
    if result.is_null() {
        return Err(("null LoadedImage", efi::Status::DEVICE_ERROR));
    }
    Ok(result.cast())
}

/// Converts a bounded, fixed fixture path to terminated UTF-16 storage.
fn path_units(path: &[u8]) -> Result<([efi::Char16; 64], usize), Failure> {
    if path.len() < 2
        || path.len() >= 64
        || path[0] != b'\\'
        || path.iter().any(|byte| !(0x20..=0x7e).contains(byte))
    {
        return Err(("fixture path", efi::Status::INVALID_PARAMETER));
    }
    let mut units = [0; 64];
    for (destination, source) in units.iter_mut().zip(path) {
        *destination = u16::from(*source);
    }
    Ok((units, path.len() + 1))
}

#[cfg(feature = "physical-policy-driver")]
mod driver {
    use super::*;

    /// Actual physical-chainload artifact, distinct from the driver itself.
    const PROJECT_PATH: &[u8] = b"\\EFI\\Test\\PHYSICAL.EFI";
    /// A fixture image present only on the runner's secondary ESP.
    const OTHER_ONLY_PATH: &[u8] = b"\\EFI\\Test\\OTHERONLY.EFI";
    /// The fixture has two ESPs; reject unexpectedly broad firmware enumeration.
    const MAX_FILESYSTEMS: usize = 32;
    /// Bounds generic PCI storage discovery without assuming q35 device paths.
    const MAX_PCI_CONTROLLERS: usize = 64;
    /// Fixed cases; no configuration file, boot variable, or user input parser.
    const CASES: [Case; 5] = [
        Case {
            name: "default-windows",
            options: Options::Empty,
            expected: efi::Status::SUCCESS,
        },
        Case {
            name: "explicit-linux",
            options: Options::Path(b"\\EFI\\ubuntu\\shimx64.efi"),
            expected: efi::Status::SUCCESS,
        },
        Case {
            name: "other-esp-only",
            options: Options::Path(OTHER_ONLY_PATH),
            expected: efi::Status::NOT_FOUND,
        },
        Case {
            name: "malformed-options",
            options: Options::OddLength,
            expected: efi::Status::INVALID_PARAMETER,
        },
        Case {
            name: "self-path",
            options: Options::Path(PROJECT_PATH),
            expected: efi::Status::ACCESS_DENIED,
        },
    ];

    /// Deliberately finite variants of the project's physical selection input.
    #[derive(Clone, Copy)]
    enum Options {
        Empty,
        Path(&'static [u8]),
        OddLength,
    }

    /// One exact application return-status assertion.
    #[derive(Clone, Copy)]
    struct Case {
        name: &'static str,
        options: Options,
        expected: efi::Status,
    }

    /// Releases one owned temporary firmware pool allocation.
    fn free(
        boot: *mut efi::BootServices,
        allocation: *mut core::ffi::c_void,
    ) -> Result<(), Failure> {
        // SAFETY: callers transfer a non-null pool allocation returned by firmware
        // to this helper exactly once, while the checked table remains live.
        let status = unsafe { ((*boot).free_pool)(allocation) };
        if status.is_error() {
            Err(("FreePool", status))
        } else {
            Ok(())
        }
    }

    /// Releases an image which has not been passed to StartImage.
    fn unload(boot: *mut efi::BootServices, image: efi::Handle) -> Result<(), Failure> {
        // SAFETY: LoadImage returned this live handle and the caller has not started
        // it or otherwise released ownership; this is its only unload operation.
        let status = unsafe { ((*boot).unload_image)(image) };
        if status.is_error() {
            Err(("UnloadImage", status))
        } else {
            Ok(())
        }
    }

    /// Loads a fixed fixture path from exactly the specified firmware ESP.
    fn load(
        boot: *mut efi::BootServices,
        parent: efi::Handle,
        device: efi::Handle,
        path: &[u8],
    ) -> Result<efi::Handle, Failure> {
        let mut path_guid = efi::protocols::device_path::PROTOCOL_GUID;
        let mut base = ptr::null_mut();
        // SAFETY: device is a live LoadedImage device or an enumerated filesystem
        // handle; boot is live and the GUID/result slots are writable locals.
        let status = unsafe { ((*boot).handle_protocol)(device, &mut path_guid, &mut base) };
        if status.is_error() {
            return Err(("ESP DevicePath", status));
        }
        if base.is_null() {
            return Err(("null ESP DevicePath", efi::Status::DEVICE_ERROR));
        }
        let mut utility_guid = efi::protocols::device_path_utilities::PROTOCOL_GUID;
        let mut utilities = ptr::null_mut();
        // SAFETY: the checked table is live, output/GUID slots are valid and a
        // null registration requests an ordinary shared utility interface.
        let status = unsafe {
            ((*boot).locate_protocol)(&mut utility_guid, ptr::null_mut(), &mut utilities)
        };
        if status.is_error() {
            return Err(("DevicePathUtilities", status));
        }
        if utilities.is_null() {
            return Err(("null DevicePathUtilities", efi::Status::DEVICE_ERROR));
        }
        let utilities = utilities.cast::<efi::protocols::device_path_utilities::Protocol>();
        let (units, count) = path_units(path)?;
        let size = 4 + count * 2;
        let mut node = [0_u8; 4 + 64 * 2];
        node[..4].copy_from_slice(&[4, 4, size as u8, (size >> 8) as u8]);
        for (bytes, unit) in node[4..size].chunks_exact_mut(2).zip(&units[..count]) {
            bytes.copy_from_slice(&unit.to_le_bytes());
        }
        // SAFETY: base is a live complete firmware path and node contains a full
        // initialized file-path node; the utility copies both into its own pool.
        let complete =
            unsafe { ((*utilities).append_device_node)(base.cast(), node.as_ptr().cast()) };
        if complete.is_null() {
            return Err(("AppendDeviceNode", efi::Status::OUT_OF_RESOURCES));
        }
        let mut image = ptr::null_mut();
        // SAFETY: parent is the running fixture, complete remains allocated until
        // after LoadImage, and the image output slot is valid writable storage.
        let status = unsafe {
            ((*boot).load_image)(
                efi::Boolean::FALSE,
                parent,
                complete,
                ptr::null_mut(),
                0,
                &mut image,
            )
        };
        let cleanup = free(boot, complete.cast());
        if status.is_error() {
            if status == efi::Status::SECURITY_VIOLATION && !image.is_null() {
                unload(boot, image)?;
            }
            cleanup?;
            return Err(("LoadImage(fixture)", status));
        }
        if image.is_null() {
            cleanup?;
            return Err(("null fixture image", efi::Status::DEVICE_ERROR));
        }
        if let Err(error) = cleanup {
            unload(boot, image)?;
            return Err(error);
        }
        Ok(image)
    }

    /// Probes a fixture without executing it; only LoadImage NOT_FOUND is absence.
    fn present(
        boot: *mut efi::BootServices,
        parent: efi::Handle,
        device: efi::Handle,
        path: &[u8],
    ) -> Result<bool, Failure> {
        match load(boot, parent, device, path) {
            Ok(image) => {
                unload(boot, image)?;
                Ok(true)
            }
            Err(("LoadImage(fixture)", status)) if status == efi::Status::NOT_FOUND => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// An absent or ambiguous alternate ESP cannot validate cross-ESP policy.
    fn unique_secondary(count: usize) -> bool {
        count == 1
    }

    /// Standard PCI base-class 1 identifies storage, independently of topology.
    fn is_storage_class(class: u8) -> bool {
        class == 1
    }

    /// No-new-binding results are provisional: the strict ESP proof still follows.
    fn connection_may_be_complete(status: efi::Status) -> bool {
        status == efi::Status::SUCCESS
            || status == efi::Status::NOT_FOUND
            || status == efi::Status::ALREADY_STARTED
    }

    /// Connects fixture storage omitted by the boot-device-only BDS connection.
    /// This does not run in either production physical backend, nor does it set
    /// boot variables or write files. Both runner-owned disks remain read-only.
    fn connect_fixture_storage(boot: *mut efi::BootServices) -> Result<(), Failure> {
        let mut guid = efi::protocols::pci_io::PROTOCOL_GUID;
        let mut count = 0;
        let mut handles = ptr::null_mut();
        // SAFETY: the checked Boot Services table is live, the protocol GUID and
        // output slots remain valid, and success transfers one owned handle pool.
        let status = unsafe {
            ((*boot).locate_handle_buffer)(
                efi::BY_PROTOCOL,
                &mut guid,
                ptr::null_mut(),
                &mut count,
                &mut handles,
            )
        };
        if status.is_error() {
            return Err(("LocateHandleBuffer(PciIo)", status));
        }
        if handles.is_null() {
            return Err(("null PCI handle buffer", efi::Status::DEVICE_ERROR));
        }
        let result = (|| {
            if !(1..=MAX_PCI_CONTROLLERS).contains(&count)
                || count
                    .checked_mul(core::mem::size_of::<efi::Handle>())
                    .and_then(|bytes| (handles as usize).checked_add(bytes))
                    .is_none()
            {
                return Err(("PCI handle buffer bounds", efi::Status::COMPROMISED_DATA));
            }
            // SAFETY: firmware returned count initialized pool-aligned handles;
            // count and pointer arithmetic are bounded and this pool stays owned
            // until the closure completes, including on every connection error.
            let controllers = unsafe { core::slice::from_raw_parts(handles, count) };
            let mut storage_count = 0;
            for (index, &controller) in controllers.iter().enumerate() {
                if controller.is_null() || controllers[..index].contains(&controller) {
                    return Err(("invalid PCI handle set", efi::Status::COMPROMISED_DATA));
                }
                let mut interface = ptr::null_mut();
                // SAFETY: controller came from the live PCI_IO handle snapshot;
                // guid and interface are valid local protocol lookup arguments.
                let status =
                    unsafe { ((*boot).handle_protocol)(controller, &mut guid, &mut interface) };
                if status.is_error() {
                    return Err(("fixture PciIo", status));
                }
                if interface.is_null() {
                    return Err(("null fixture PciIo", efi::Status::DEVICE_ERROR));
                }
                let pci = interface.cast::<efi::protocols::pci_io::Protocol>();
                let mut class = 0_u8;
                // SAFETY: pci is a live validated interface; one byte of standard
                // configuration-space base-class data is read into writable local
                // storage. No direct MMIO or configuration writes are issued, and
                // no device identifiers are read or logged.
                let status = unsafe {
                    ((*pci).pci.read)(
                        pci,
                        efi::protocols::pci_io::WIDTH_UINT8,
                        0x0b,
                        1,
                        ptr::from_mut(&mut class).cast(),
                    )
                };
                if status.is_error() {
                    return Err(("fixture PCI storage class read", status));
                }
                if !is_storage_class(class) {
                    continue;
                }
                storage_count += 1;
                // SAFETY: this storage controller belongs only to the disposable
                // QEMU fixture. Normal firmware driver binding may recursively
                // expose its partition/filesystem children; no driver is supplied,
                // no image is started by the fixture, and disks are host-read-only.
                let status = unsafe {
                    ((*boot).connect_controller)(
                        controller,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        efi::Boolean::TRUE,
                    )
                };
                if !connection_may_be_complete(status) {
                    return Err(("ConnectController(fixture storage)", status));
                }
            }
            if storage_count == 0 {
                return Err(("no fixture storage controller", efi::Status::NOT_FOUND));
            }
            Ok(())
        })();
        free(boot, handles.cast())?;
        result
    }

    /// Proves fixture visibility before testing that the project ignores other ESPs.
    fn prove_secondary(
        boot: *mut efi::BootServices,
        parent: efi::Handle,
        primary: efi::Handle,
    ) -> Result<(), Failure> {
        if present(boot, parent, primary, OTHER_ONLY_PATH)? {
            return Err((
                "other-only fixture exists on primary ESP",
                efi::Status::ABORTED,
            ));
        }
        let mut guid = efi::protocols::simple_file_system::PROTOCOL_GUID;
        let mut count = 0;
        let mut handles = ptr::null_mut();
        // SAFETY: boot is the checked live table; the GUID and result slots remain
        // valid for LocateHandleBuffer, which returns an owned pool of handles.
        let status = unsafe {
            ((*boot).locate_handle_buffer)(
                efi::BY_PROTOCOL,
                &mut guid,
                ptr::null_mut(),
                &mut count,
                &mut handles,
            )
        };
        if status.is_error() {
            return Err(("LocateHandleBuffer(SimpleFileSystem)", status));
        }
        if handles.is_null() {
            return Err(("null filesystem handle buffer", efi::Status::DEVICE_ERROR));
        }
        let result = (|| {
            if !(1..=MAX_FILESYSTEMS).contains(&count)
                || count
                    .checked_mul(core::mem::size_of::<efi::Handle>())
                    .and_then(|bytes| (handles as usize).checked_add(bytes))
                    .is_none()
            {
                return Err((
                    "filesystem handle buffer bounds",
                    efi::Status::COMPROMISED_DATA,
                ));
            }
            // SAFETY: LocateHandleBuffer returned count initialized, pool-aligned
            // handles; count and byte-range arithmetic were bounded above, and the
            // owned allocation remains live until after this closure returns.
            let devices = unsafe { core::slice::from_raw_parts(handles, count) };
            let mut found = 0;
            for (index, &device) in devices.iter().enumerate() {
                if device.is_null() || devices[..index].contains(&device) {
                    return Err((
                        "invalid filesystem handle set",
                        efi::Status::COMPROMISED_DATA,
                    ));
                }
                if device == primary || !present(boot, parent, device, OTHER_ONLY_PATH)? {
                    continue;
                }
                found += 1;
                if !unique_secondary(found) {
                    return Err(("ambiguous secondary ESP", efi::Status::ABORTED));
                }
                for path in [
                    &b"\\EFI\\Microsoft\\Boot\\bootmgfw.efi"[..],
                    &b"\\EFI\\ubuntu\\shimx64.efi"[..],
                ] {
                    if !present(boot, parent, device, path)? {
                        return Err((
                            "secondary duplicate fixture missing",
                            efi::Status::NOT_FOUND,
                        ));
                    }
                }
            }
            if !unique_secondary(found) {
                return Err(("secondary ESP not firmware-visible", efi::Status::NOT_FOUND));
            }
            Ok(())
        })();
        free(boot, handles.cast())?;
        result
    }

    /// Runs one complete fixture and returns the project's exact status.
    fn run_case(
        boot: *mut efi::BootServices,
        parent: efi::Handle,
        device: efi::Handle,
        case: Case,
        serial: &mut Serial,
    ) -> Result<efi::Status, Failure> {
        let image = load(boot, parent, device, PROJECT_PATH)?;
        let loaded = match loaded_image(boot, image) {
            Ok(loaded) => loaded,
            Err(error) => {
                unload(boot, image)?;
                return Err(error);
            }
        };
        let options = match case.options {
            Options::Path(path) => path_units(path),
            Options::Empty | Options::OddLength => Ok(([0; 64], 0)),
        };
        let (mut units, count) = match options {
            Ok(options) => options,
            Err(error) => {
                unload(boot, image)?;
                return Err(error);
            }
        };
        let size = match case.options {
            Options::Empty => 0,
            Options::Path(_) => count * 2,
            Options::OddLength => 1,
        };
        // SAFETY: this unstarted image is exclusively owned by the driver. Its
        // bounded UTF-16 storage remains live until synchronous StartImage returns;
        // the one-byte variant intentionally exercises validation before decoding.
        unsafe {
            (*loaded).load_options = if size == 0 {
                ptr::null_mut()
            } else {
                units.as_mut_ptr().cast()
            };
            (*loaded).load_options_size = size as u32;
        }
        let _ = writeln!(serial, "thin-hv: physical policy case={} begin", case.name);
        let mut exit_size = 0;
        let mut exit_data = ptr::null_mut();
        // SAFETY: the checked image is loaded but not started, all options and
        // output storage remain live, and the fixture never leaves Boot Services.
        let status = unsafe { ((*boot).start_image)(image, &mut exit_size, &mut exit_data) };
        if !exit_data.is_null() {
            free(boot, exit_data.cast())?;
        }
        if exit_data.is_null() && exit_size != 0 {
            return Err(("null ExitData", efi::Status::DEVICE_ERROR));
        }
        // Returning UEFI applications are unloaded by StartImage; do not use the
        // ended image handle or the LoadedImage pointer again here.
        Ok(status)
    }

    /// Executes every finite case; the runner independently checks its transcript.
    pub(super) fn run(
        image: efi::Handle,
        table: *mut efi::SystemTable,
        serial: &mut Serial,
    ) -> Result<(), Failure> {
        let boot = services(table)?;
        let loaded = loaded_image(boot, image)?;
        // SAFETY: this is the running driver's live LoadedImage interface.
        let device = unsafe { (*loaded).device_handle };
        if device.is_null() {
            return Err(("driver ESP", efi::Status::INVALID_PARAMETER));
        }
        connect_fixture_storage(boot)?;
        prove_secondary(boot, image, device)?;
        let _ = writeln!(
            serial,
            "thin-hv: physical policy secondary_esp_visible=1 targets=windows,linux,other-only PASS"
        );
        for case in CASES {
            let status = run_case(boot, image, device, case, serial)?;
            if status != case.expected {
                let _ = writeln!(
                    serial,
                    "thin-hv: physical policy case={} FAIL status={:#x} expected={:#x}",
                    case.name,
                    status.as_usize(),
                    case.expected.as_usize()
                );
                return Err(("unexpected project status", efi::Status::ABORTED));
            }
            let _ = writeln!(
                serial,
                "thin-hv: physical policy case={} PASS status={:#x}",
                case.name,
                status.as_usize()
            );
        }
        let _ = writeln!(serial, "thin-hv: physical policy harness PASS");
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn fixture_connections_select_only_storage_and_preserve_errors() {
            assert!(is_storage_class(1));
            for class in [0, 2, 3, 6, 0xff] {
                assert!(!is_storage_class(class));
            }
            for status in [
                efi::Status::SUCCESS,
                efi::Status::NOT_FOUND,
                efi::Status::ALREADY_STARTED,
            ] {
                assert!(connection_may_be_complete(status));
            }
            for status in [
                efi::Status::INVALID_PARAMETER,
                efi::Status::DEVICE_ERROR,
                efi::Status::OUT_OF_RESOURCES,
                efi::Status::SECURITY_VIOLATION,
            ] {
                assert!(!connection_may_be_complete(status));
            }
        }

        #[test]
        fn secondary_fixture_must_be_present_and_unambiguous() {
            assert!(unique_secondary(1));
            for count in [0, 2, MAX_FILESYSTEMS, usize::MAX] {
                assert!(!unique_secondary(count));
            }
        }

        #[test]
        fn finite_cases_keep_exact_negative_statuses_and_self_path() {
            assert_eq!(CASES.len(), 5);
            assert_eq!(CASES[0].expected, efi::Status::SUCCESS);
            assert_eq!(CASES[1].expected, efi::Status::SUCCESS);
            assert_eq!(CASES[2].expected, efi::Status::NOT_FOUND);
            assert_eq!(CASES[3].expected, efi::Status::INVALID_PARAMETER);
            assert_eq!(CASES[4].expected, efi::Status::ACCESS_DENIED);
            assert!(matches!(CASES[4].options, Options::Path(PROJECT_PATH)));
        }
    }
}

#[cfg(feature = "physical-policy-payload")]
mod payload {
    use super::*;

    /// Decodes the fixture's file-path node after its total size is checked.
    fn path_matches(bytes: &[u8], expected: &[u8]) -> bool {
        let Ok((units, count)) = path_units(expected) else {
            return false;
        };
        let size = 4 + count * 2;
        bytes.len() == size + 4
            && bytes[..4] == [4, 4, size as u8, (size >> 8) as u8]
            && bytes[size..] == [0x7f, 0xff, 4, 0]
            && bytes[4..size]
                .chunks_exact(2)
                .zip(&units[..count])
                .all(|(bytes, unit)| u16::from_le_bytes([bytes[0], bytes[1]]) == *unit)
    }

    /// Proves that a real firmware child was loaded from its parent's ESP.
    pub(super) fn run(
        image: efi::Handle,
        table: *mut efi::SystemTable,
        serial: &mut Serial,
    ) -> Result<(), Failure> {
        let boot = services(table)?;
        let loaded = loaded_image(boot, image)?;
        // SAFETY: the successful protocol lookup returned this child's live image
        // interface, and its parent remains running throughout StartImage.
        let (device, parent_handle, path) = unsafe {
            (
                (*loaded).device_handle,
                (*loaded).parent_handle,
                (*loaded).file_path,
            )
        };
        let parent = loaded_image(boot, parent_handle)?;
        // SAFETY: the live parent is suspended inside StartImage for this child.
        let parent_device = unsafe { (*parent).device_handle };
        if device.is_null() || parent_device != device || path.is_null() {
            return Err(("payload current ESP", efi::Status::ACCESS_DENIED));
        }
        let mut guid = efi::protocols::device_path_utilities::PROTOCOL_GUID;
        let mut utilities = ptr::null_mut();
        // SAFETY: the checked Boot Services table is live, GUID/output pointers
        // are writable locals, and null registration is permitted by LocateProtocol.
        let status =
            unsafe { ((*boot).locate_protocol)(&mut guid, ptr::null_mut(), &mut utilities) };
        if status.is_error() {
            return Err(("payload DevicePathUtilities", status));
        }
        if utilities.is_null() {
            return Err(("payload null utilities", efi::Status::DEVICE_ERROR));
        }
        let utilities = utilities.cast::<efi::protocols::device_path_utilities::Protocol>();
        // SAFETY: the live LoadedImage FilePath belongs to firmware; the utility
        // returns its allocation length without modifying the path.
        let length = unsafe { ((*utilities).get_device_path_size)(path) };
        if !(10..=136).contains(&length) || (path as usize).checked_add(length).is_none() {
            return Err(("payload file path length", efi::Status::COMPROMISED_DATA));
        }
        // SAFETY: firmware owns this non-null FilePath allocation of the checked
        // bounded length until this application returns to its waiting parent.
        let bytes = unsafe { core::slice::from_raw_parts(path.cast::<u8>(), length) };
        let name = if path_matches(bytes, b"\\EFI\\Microsoft\\Boot\\bootmgfw.efi") {
            "windows"
        } else if path_matches(bytes, b"\\EFI\\ubuntu\\shimx64.efi") {
            "linux"
        } else {
            return Err(("payload unexpected path", efi::Status::ACCESS_DENIED));
        };
        let _ = writeln!(
            serial,
            "thin-hv: physical policy payload path={name} current_esp=1 PASS"
        );
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn payload_path_requires_exact_file_node_and_end() {
            let path = b"\\EFI\\ubuntu\\shimx64.efi";
            let (units, count) = path_units(path).unwrap();
            let size = 4 + count * 2;
            let mut bytes = vec![4, 4, size as u8, (size >> 8) as u8];
            for unit in &units[..count] {
                bytes.extend(unit.to_le_bytes());
            }
            bytes.extend([0x7f, 0xff, 4, 0]);
            assert!(path_matches(&bytes, path));
            assert!(!path_matches(
                &bytes,
                b"\\EFI\\Microsoft\\Boot\\bootmgfw.efi"
            ));
            bytes.push(0);
            assert!(!path_matches(&bytes, path));
            assert!(!path_matches(&[], path));
        }
    }
}

/// Starts exactly the selected disposable fixture role.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub extern "efiapi" fn efi_main(image: efi::Handle, table: *mut efi::SystemTable) -> efi::Status {
    let mut serial = Serial;
    serial.init();
    #[cfg(feature = "physical-policy-driver")]
    let result = driver::run(image, table, &mut serial);
    #[cfg(feature = "physical-policy-payload")]
    let result = payload::run(image, table, &mut serial);
    match result {
        Ok(()) => efi::Status::SUCCESS,
        Err((operation, status)) => {
            let _ = writeln!(
                serial,
                "thin-hv: physical policy harness FAIL operation={operation} status={:#x}",
                status.as_usize()
            );
            status
        }
    }
}

/// Makes a fixture panic observable without returning through a damaged stack.
#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    let mut serial = Serial;
    let _ = writeln!(serial, "thin-hv: physical policy harness FAIL panic");
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_paths_are_bounded_and_terminated() {
        let (units, count) = path_units(b"\\EFI\\Test\\PHYSICAL.EFI").unwrap();
        assert_eq!(units[count - 1], 0);
        for path in [&b""[..], &b"relative"[..], &b"\\x\0"[..], &[b'\\'; 64][..]] {
            assert!(path_units(path).is_err());
        }
    }
}
