//! Exact firmware completion boundary for physical Direct CPU handoff.
//!
//! An ExitBootServices event can run before the service has finished. This
//! retained runtime-image wrapper instead calls the original service first.
//! Failed attempts retain native retry semantics and never notify L0. No Boot
//! Service is called after success. AP takeover is not implemented here.

use crate::chainload;
use core::ptr;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use r_efi::efi;

pub(crate) const REQUEST: u64 = u64::from_le_bytes(*b"THVEBS01");
pub(crate) const ACKNOWLEDGED: u64 = !REQUEST;
type ExitBootServices = unsafe extern "efiapi" fn(efi::Handle, usize) -> efi::Status;

// Firmware entry pointer, not VMX state. Only the retained registered runtime
// image uses it; the independent L0 copy never calls firmware through this slot.
static ORIGINAL_EXIT: AtomicUsize = AtomicUsize::new(0);

/// Installed only while preparing the initial carrier, before L1 runs. The
/// caller never returns after successful VM entry; Drop therefore runs only on
/// pre-entry rollback, while Boot Services and this table are still live.
pub(crate) struct Installed {
    table: *mut efi::BootServices,
    original: ExitBootServices,
    crc: u32,
}

impl Drop for Installed {
    fn drop(&mut self) {
        // SAFETY: this private guard is scoped to pre-entry preparation. No L1
        // ran on any returning path and no other table hook was installed in
        // that scope. Restore the exact two changed fields before image unload.
        unsafe {
            (*self.table).exit_boot_services = self.original;
            (*self.table).hdr.crc32 = self.crc;
        }
        ORIGINAL_EXIT.store(0, Ordering::Release);
    }
}

pub(crate) fn install(system: *mut efi::SystemTable) -> Result<Installed, chainload::Error> {
    let table = chainload::boot_services(system)?;
    // SAFETY: firmware owns this live table through pre-entry rollback. Read
    // only its complete header before bounding the function fields used below.
    let header = unsafe { ptr::addr_of!((*table).hdr).read() };
    let size = header.header_size as usize;
    let minimum =
        core::mem::offset_of!(efi::BootServices, calculate_crc32) + core::mem::size_of::<usize>();
    if size < minimum || size > core::mem::size_of::<efi::BootServices>() {
        return Err(chainload::Error::Firmware(
            "handoff Boot Services header",
            efi::Status::UNSUPPORTED.as_usize(),
        ));
    }
    // SAFETY: the validated header covers both fields; firmware supplied valid
    // function pointers, borrowed only before any ExitBootServices attempt.
    let original = unsafe { ptr::addr_of!((*table).exit_boot_services).read() };
    if original as usize == exit_boot_services as *const () as usize
        || ORIGINAL_EXIT
            .compare_exchange(0, original as usize, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return Err(chainload::Error::Firmware(
            "handoff already installed",
            efi::Status::ALREADY_STARTED.as_usize(),
        ));
    }
    let guard = Installed {
        table,
        original,
        crc: header.crc32,
    };
    let mut crc = 0;
    // SAFETY: this BSP exclusively installs this one table hook before L1
    // execution. The bounded header is readable with CRC temporarily zeroed;
    // CalculateCrc32 is the original live firmware function, not a hooked call.
    let status = unsafe {
        (*table).exit_boot_services = exit_boot_services;
        (*table).hdr.crc32 = 0;
        ((*table).calculate_crc32)(table.cast(), size, &mut crc)
    };
    if status.is_error() {
        // guard restores the previous entry and CRC even if calculation fails.
        return Err(chainload::Error::Firmware(
            "handoff table CRC",
            status.as_usize(),
        ));
    }
    // SAFETY: the guarded, live table still belongs to this pre-entry install.
    unsafe { (*table).hdr.crc32 = crc };
    Ok(guard)
}

fn after_firmware(status: efi::Status, notify: impl FnOnce()) -> efi::Status {
    if status == efi::Status::SUCCESS {
        notify();
    }
    status
}

unsafe extern "efiapi" fn exit_boot_services(image: efi::Handle, key: usize) -> efi::Status {
    let address = ORIGINAL_EXIT.load(Ordering::Acquire);
    if address == 0 {
        return efi::Status::NOT_READY;
    }
    // SAFETY: install saved this exact firmware ABI pointer before publishing
    // the hook. Firmware supplied the handle/map key arguments. On failure its
    // own status is returned without allocation, cleanup or an extra BS call.
    let status = unsafe {
        let original: ExitBootServices = core::mem::transmute(address);
        original(image, key)
    };
    after_firmware(status, || {
        let mut acknowledgement = REQUEST;
        // SAFETY: this wrapper executes only in the L1 of the physical Direct
        // carrier, in retained runtime code at CPL0. L0 recognizes this exact
        // one-shot request and preserves the guest context. No BS pointer is
        // accessed after the original service succeeded. asm is a memory barrier.
        unsafe { core::arch::asm!("vmcall", inout("rax") acknowledgement, options(nostack)) };
        if acknowledgement != ACKNOWLEDGED {
            crate::SerialPort.write_bytes(b"thin-hv: firmware handoff FAIL acknowledgement\n");
            // SAFETY: a failed internal handoff after EBS cannot safely return
            // an EFI retry status. Remain stopped at CPL0 without firmware calls.
            unsafe {
                core::arch::asm!(
                    "cli",
                    "2:",
                    "hlt",
                    "jmp 2b",
                    options(noreturn, nomem, nostack)
                )
            };
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "efiapi" fn failed_exit(image: efi::Handle, key: usize) -> efi::Status {
        // SAFETY: this test passes an exclusive aligned usize slot as the mock
        // handle. Recording the exact map key proves the hook forwards it once.
        unsafe { image.cast::<usize>().write(key) };
        efi::Status::INVALID_PARAMETER
    }

    unsafe extern "efiapi" fn good_crc(
        _data: *mut core::ffi::c_void,
        _size: usize,
        crc: *mut u32,
    ) -> efi::Status {
        // SAFETY: install passes its owned aligned output slot to this mock.
        unsafe { crc.write(0x12345678) };
        efi::Status::SUCCESS
    }

    unsafe extern "efiapi" fn bad_crc(
        _data: *mut core::ffi::c_void,
        _size: usize,
        _crc: *mut u32,
    ) -> efi::Status {
        efi::Status::DEVICE_ERROR
    }

    #[test]
    fn failed_firmware_exit_and_crc_installation_restore_exact_original_table() {
        let mut bs = core::mem::MaybeUninit::<efi::BootServices>::zeroed();
        let mut st = core::mem::MaybeUninit::<efi::SystemTable>::zeroed();
        let table = bs.as_mut_ptr();
        let system = st.as_mut_ptr();
        // SAFETY: complete aligned test allocations exist; initialize every
        // field install/Drop calls or reads. Never assume_init the other invalid
        // zeroed function-pointer slots or create a whole-table reference.
        unsafe {
            ptr::addr_of_mut!((*system).boot_services).write(table);
            ptr::addr_of_mut!((*table).hdr.header_size)
                .write(core::mem::size_of::<efi::BootServices>() as u32);
            ptr::addr_of_mut!((*table).hdr.crc32).write(42);
            ptr::addr_of_mut!((*table).exit_boot_services).write(failed_exit);
            ptr::addr_of_mut!((*table).calculate_crc32).write(bad_crc);
        }
        assert!(install(system).is_err());
        assert_eq!(ORIGINAL_EXIT.load(Ordering::Acquire), 0);
        // SAFETY: the initialized header/entry fields remain live after rollback.
        unsafe {
            assert_eq!((*table).hdr.crc32, 42);
            assert_eq!(
                (*table).exit_boot_services as usize,
                failed_exit as *const () as usize
            );
            ptr::addr_of_mut!((*table).calculate_crc32).write(good_crc);
        }
        let guard = install(system).unwrap();
        assert!(install(system).is_err());
        let mut key_seen = 0usize;
        // SAFETY: this installed mock table is live; failed_exit receives our
        // owned handle slot and returns failure, so no privileged VMCALL occurs.
        unsafe {
            assert_eq!((*table).hdr.crc32, 0x12345678);
            assert_eq!(
                ((*table).exit_boot_services)(ptr::addr_of_mut!(key_seen).cast(), 1234),
                efi::Status::INVALID_PARAMETER
            );
        }
        assert_eq!(key_seen, 1234);
        drop(guard);
        // SAFETY: Drop restored initialized fields in our still-live allocation.
        unsafe {
            assert_eq!((*table).hdr.crc32, 42);
            assert_eq!(
                (*table).exit_boot_services as usize,
                failed_exit as *const () as usize
            );
        }
        assert_eq!(ORIGINAL_EXIT.load(Ordering::Acquire), 0);
    }

    #[test]
    fn failed_exit_never_transfers_cpu_ownership_and_preserves_firmware_status() {
        for status in [
            efi::Status::INVALID_PARAMETER,
            efi::Status::DEVICE_ERROR,
            efi::Status::OUT_OF_RESOURCES,
            efi::Status::UNSUPPORTED,
        ] {
            assert_eq!(
                after_firmware(status, || panic!("failed firmware call notified L0")),
                status
            );
        }
        let mut notifications = 0;
        assert_eq!(
            after_firmware(efi::Status::SUCCESS, || notifications += 1),
            efi::Status::SUCCESS
        );
        assert_eq!(notifications, 1);
        assert_ne!(REQUEST, ACKNOWLEDGED);
    }
}
