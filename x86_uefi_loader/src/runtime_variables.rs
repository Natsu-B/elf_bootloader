//! Runtime-services hooks for profile-private UEFI boot variables.

use core::ffi::c_void;
use core::mem;
use core::ptr;
use core::slice;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use mutex::SpinLock;
use r_efi::efi;
use uefi_variable_overlay::BACKEND_NAME_CAPACITY;
use uefi_variable_overlay::Guid;
use uefi_variable_overlay::MONITOR_VENDOR_GUID;
use uefi_variable_overlay::PROFILE_SELECTOR_NAME;
use uefi_variable_overlay::ProfileId;
use uefi_variable_overlay::UefiProfile;
use uefi_variable_overlay::is_profile_private;
use uefi_variable_overlay::map_private_variable;
use uefi_variable_overlay::unmap_private_variable;

/// Longest logical name selected by the boot-variable policy.
const POLICY_NAME_CAPACITY: usize = 11;
/// Maximum physical name handled while filtering firmware enumeration.
// ponytail: one page covers OVMF names; grow this only if a measured firmware
// returns a longer name from GetNextVariableName.
const ENUM_NAME_CAPACITY: usize = 2048;
/// Maximum MAT descriptors intersected by one late-loaded monitor image.
// ponytail: OVMF currently needs one descriptor; keep this bounded until a
// measured firmware proves that a larger late-image split is necessary.
const MEMORY_ATTRIBUTE_PATCH_CAPACITY: usize = 16;
/// Architectural page size used by UEFI memory descriptors.
const PAGE_SIZE: u64 = 4096;
/// Sentinel kept as initialized data so the runtime PE retains its `.data` section.
const UNINSTALLED_PROFILE: u32 = u32::MAX;

static PROFILE: AtomicU32 = AtomicU32::new(UNINSTALLED_PROFILE);
static ORIGINAL_GET_VARIABLE: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_GET_NEXT_VARIABLE_NAME: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_SET_VARIABLE: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_CONVERT_POINTER: AtomicUsize = AtomicUsize::new(0);
static ENUM_NAME: SpinLock<[efi::Char16; ENUM_NAME_CAPACITY]> =
    SpinLock::new([0; ENUM_NAME_CAPACITY]);

/// Original runtime entries restored when VM launch fails.
#[derive(Clone, Copy)]
struct OriginalEntries {
    get_variable: efi::RuntimeGetVariable,
    get_next_variable_name: efi::RuntimeGetNextVariableName,
    set_variable: efi::RuntimeSetVariable,
}

/// The boot UI owns this selector. It survives reboot but is not writable via
/// OS runtime calls after ExitBootServices; guest BootNext remains independent.
const SELECTOR_ATTRIBUTES: u32 = efi::VARIABLE_NON_VOLATILE | efi::VARIABLE_BOOTSERVICE_ACCESS;

/// Reads the primary-OS selector before installing any overlay. Missing state is
/// distinct from corruption and is resolved only by explicit boot selection.
pub(crate) fn read_boot_profile(
    runtime: *mut efi::RuntimeServices,
) -> Result<Option<UefiProfile>, efi::Status> {
    if runtime.is_null() {
        return Err(efi::Status::INVALID_PARAMETER);
    }
    let mut name = [0; 16];
    name[..PROFILE_SELECTOR_NAME.len()].copy_from_slice(PROFILE_SELECTOR_NAME);
    let mut guid = to_efi_guid(MONITOR_VENDOR_GUID);
    let mut record = [0; 8];
    let mut size = record.len();
    let mut attributes = 0;
    // SAFETY: the caller holds a live physical RuntimeServices table before EBS;
    // bounded NUL-terminated name, GUID, payload and outputs live across the call.
    let status = unsafe {
        ((*runtime).get_variable)(
            name.as_mut_ptr(),
            &mut guid,
            &mut attributes,
            &mut size,
            record.as_mut_ptr().cast(),
        )
    };
    if status == efi::Status::NOT_FOUND {
        return Ok(None);
    }
    if status == efi::Status::BUFFER_TOO_SMALL {
        return Err(efi::Status::COMPROMISED_DATA);
    }
    if status.is_error() {
        return Err(status);
    }
    if attributes != SELECTOR_ATTRIBUTES || size != record.len() {
        return Err(efi::Status::COMPROMISED_DATA);
    }
    UefiProfile::from_selection_record(&record)
        .map(Some)
        .ok_or(efi::Status::COMPROMISED_DATA)
}

/// Commits an explicit boot-UI selection in one firmware-atomic write, without
/// changing the live overlay. Only a subsequent boot consumes this selector.
pub(crate) fn write_boot_profile(
    runtime: *mut efi::RuntimeServices,
    profile: UefiProfile,
) -> Result<(), efi::Status> {
    if runtime.is_null() {
        return Err(efi::Status::INVALID_PARAMETER);
    }
    let mut name = [0; 16];
    name[..PROFILE_SELECTOR_NAME.len()].copy_from_slice(PROFILE_SELECTOR_NAME);
    let mut guid = to_efi_guid(MONITOR_VENDOR_GUID);
    let record = profile.selection_record();
    // SAFETY: caller owns this boot-time selection and a live firmware table;
    // firmware reads the terminated name and exact eight-byte record synchronously.
    let status = unsafe {
        ((*runtime).set_variable)(
            name.as_mut_ptr(),
            &mut guid,
            SELECTOR_ATTRIBUTES,
            record.len(),
            record.as_ptr().cast_mut().cast(),
        )
    };
    if status.is_error() {
        Err(status)
    } else {
        Ok(())
    }
}

/// One in-place EFI Memory Attributes Table edit retained for rollback.
#[derive(Clone, Copy)]
struct MemoryAttributePatch {
    attribute: *mut u64,
    original: u64,
}

const EMPTY_MEMORY_ATTRIBUTE_PATCH: MemoryAttributePatch = MemoryAttributePatch {
    attribute: ptr::null_mut(),
    original: 0,
};

/// Bounded edits that keep this late-loaded runtime image executable.
struct MemoryAttributePatches {
    entries: [MemoryAttributePatch; MEMORY_ATTRIBUTE_PATCH_CAPACITY],
    len: usize,
    system_table: *mut efi::SystemTable,
    table: *mut c_void,
}

impl MemoryAttributePatches {
    const fn new() -> Self {
        Self {
            entries: [EMPTY_MEMORY_ATTRIBUTE_PATCH; MEMORY_ATTRIBUTE_PATCH_CAPACITY],
            len: 0,
            system_table: ptr::null_mut(),
            table: ptr::null_mut(),
        }
    }

    fn restore(&mut self) {
        // A runtime allocation can make OVMF replace the configuration-table
        // buffer. Never write through retained pointers into a superseded MAT.
        let table_is_current =
            unsafe { current_memory_attributes_table(self.system_table) == Some(self.table) };
        if table_is_current {
            for patch in self.entries[..self.len].iter().rev() {
                // SAFETY: the current MAT still owns this validated field.
                unsafe { ptr::write_unaligned(patch.attribute, patch.original) };
            }
        }
        self.len = 0;
    }

    fn push(&mut self, attribute: *mut u64, original: u64) -> Result<(), efi::Status> {
        if self.len == self.entries.len() {
            return Err(efi::Status::OUT_OF_RESOURCES);
        }
        self.entries[self.len] = MemoryAttributePatch {
            attribute,
            original,
        };
        self.len += 1;
        Ok(())
    }
}

/// An installed in-place runtime-services hook.
pub(crate) struct InstalledOverlay {
    runtime_services: *mut efi::RuntimeServices,
    boot_services: *mut efi::BootServices,
    event: efi::Event,
    original: OriginalEntries,
    original_crc: u32,
    memory_attributes: MemoryAttributePatches,
    active: bool,
}

impl InstalledOverlay {
    /// Number of MAT descriptors changed for the late-loaded runtime image.
    pub(crate) fn memory_attribute_patch_count(&self) -> usize {
        self.memory_attributes.len
    }

    /// Restores the firmware table and closes the address-change event.
    pub(crate) fn rollback(mut self) -> Result<(), efi::Status> {
        let result = self.restore();
        self.active = false;
        result
    }

    fn restore(&mut self) -> Result<(), efi::Status> {
        // SAFETY: installation retained both firmware tables until guest
        // ExitBootServices; rollback is reached only when VM entry failed.
        let crc_status = unsafe {
            let runtime = &mut *self.runtime_services;
            runtime.get_variable = self.original.get_variable;
            runtime.get_next_variable_name = self.original.get_next_variable_name;
            runtime.set_variable = self.original.set_variable;
            update_runtime_crc(self.runtime_services, self.boot_services)
        };
        self.memory_attributes.restore();
        // SAFETY: `event` was returned by this boot-services table and has not
        // been closed yet.
        let close_status = unsafe { ((*self.boot_services).close_event)(self.event) };
        clear_saved_entries();
        if crc_status.is_error() {
            // Keep the original CRC when CalculateCrc32 itself is unavailable.
            unsafe { (*self.runtime_services).hdr.crc32 = self.original_crc };
            Err(crc_status)
        } else if close_status.is_error() {
            Err(close_status)
        } else {
            Ok(())
        }
    }
}

impl Drop for InstalledOverlay {
    fn drop(&mut self) {
        if self.active {
            let _ = self.restore();
        }
    }
}

/// Installs the three profile-sensitive variable services in place.
///
/// `QueryVariableInfo`, capsule services, and every unrelated runtime entry
/// remain the firmware's original functions.
pub(crate) fn install(
    system_table: *mut efi::SystemTable,
    profile: ProfileId,
    image_base: u64,
    image_size: u64,
) -> Result<InstalledOverlay, efi::Status> {
    if system_table.is_null()
        || profile.0 == 0
        || image_base % PAGE_SIZE != 0
        || image_size == 0
        || image_size % PAGE_SIZE != 0
    {
        return Err(efi::Status::INVALID_PARAMETER);
    }
    if ORIGINAL_GET_VARIABLE.load(Ordering::Acquire) != 0 {
        return Err(efi::Status::ALREADY_STARTED);
    }

    // SAFETY: the UEFI entry point supplied `system_table`, and Boot Services
    // are live until the subsequently launched guest exits them.
    let (runtime_services, boot_services) = unsafe {
        (
            (*system_table).runtime_services,
            (*system_table).boot_services,
        )
    };
    if runtime_services.is_null() || boot_services.is_null() {
        return Err(efi::Status::INVALID_PARAMETER);
    }
    // SAFETY: both pointers were validated above and belong to firmware.
    let runtime = unsafe { &mut *runtime_services };
    if runtime.hdr.signature != efi::RUNTIME_SERVICES_SIGNATURE
        || (runtime.hdr.header_size as usize) < mem::size_of::<efi::RuntimeServices>()
    {
        return Err(efi::Status::INCOMPATIBLE_VERSION);
    }
    let original = OriginalEntries {
        get_variable: runtime.get_variable,
        get_next_variable_name: runtime.get_next_variable_name,
        set_variable: runtime.set_variable,
    };
    if original.get_variable as usize == get_variable as usize
        || original.get_next_variable_name as usize == get_next_variable_name as usize
        || original.set_variable as usize == set_variable as usize
    {
        return Err(efi::Status::ALREADY_STARTED);
    }

    let mut event = ptr::null_mut();
    let virtual_address_change = efi::EVENT_GROUP_VIRTUAL_ADDRESS_CHANGE;
    // SAFETY: arguments follow CreateEventEx and the callback is part of the
    // firmware-loaded runtime image.
    let status = unsafe {
        ((*boot_services).create_event_ex)(
            efi::EVT_NOTIFY_SIGNAL,
            efi::TPL_NOTIFY,
            Some(virtual_address_change_notify),
            ptr::null(),
            &virtual_address_change,
            &mut event,
        )
    };
    if status.is_error() {
        return Err(status);
    }

    // A runtime driver loaded by BDS is too late for OVMF's EndOfDxe image
    // record. OVMF consequently publishes its runtime allocation as RO+XP in
    // the MAT even though the PE subsystem and section flags are valid. Limit
    // the workaround to MAT descriptors intersecting this one trusted image.
    let mut memory_attributes =
        match unsafe { patch_monitor_memory_attributes(system_table, image_base, image_size) } {
            Ok(patches) => patches,
            Err(status) => {
                // SAFETY: event creation succeeded above.
                let _ = unsafe { ((*boot_services).close_event)(event) };
                return Err(status);
            }
        };

    PROFILE.store(profile.0, Ordering::Release);
    ORIGINAL_GET_VARIABLE.store(original.get_variable as usize, Ordering::Release);
    ORIGINAL_GET_NEXT_VARIABLE_NAME
        .store(original.get_next_variable_name as usize, Ordering::Release);
    ORIGINAL_SET_VARIABLE.store(original.set_variable as usize, Ordering::Release);
    ORIGINAL_CONVERT_POINTER.store(runtime.convert_pointer as usize, Ordering::Release);

    let original_crc = runtime.hdr.crc32;
    runtime.get_variable = get_variable;
    runtime.get_next_variable_name = get_next_variable_name;
    runtime.set_variable = set_variable;
    // SAFETY: the table remains writable runtime data under OVMF and Boot
    // Services are still active.
    let crc_status = unsafe { update_runtime_crc(runtime_services, boot_services) };
    if crc_status.is_error() {
        runtime.get_variable = original.get_variable;
        runtime.get_next_variable_name = original.get_next_variable_name;
        runtime.set_variable = original.set_variable;
        runtime.hdr.crc32 = original_crc;
        memory_attributes.restore();
        clear_saved_entries();
        // SAFETY: event creation succeeded above.
        let _ = unsafe { ((*boot_services).close_event)(event) };
        return Err(crc_status);
    }

    Ok(InstalledOverlay {
        runtime_services,
        boot_services,
        event,
        original,
        original_crc,
        memory_attributes,
        active: true,
    })
}

/// Clears RO/XP only on MAT descriptors intersecting the loaded monitor.
///
/// OVMF cannot add an image-properties record after EndOfDxe, so its generic
/// fallback otherwise labels the containing runtime-code allocation RO+XP.
/// The trusted-L1 threat model permits this late image to remain RWX; all MAT
/// descriptors outside its allocation are left byte-for-byte unchanged.
unsafe fn patch_monitor_memory_attributes(
    system_table: *mut efi::SystemTable,
    image_base: u64,
    image_size: u64,
) -> Result<MemoryAttributePatches, efi::Status> {
    let image_end = image_base
        .checked_add(image_size)
        .ok_or(efi::Status::INVALID_PARAMETER)?;
    // SAFETY: install validated the firmware-supplied System Table.
    let system = unsafe { &*system_table };
    if system.number_of_table_entries == 0 {
        return Ok(MemoryAttributePatches::new());
    }
    if system.number_of_table_entries > 4096 || system.configuration_table.is_null() {
        return Err(efi::Status::COMPROMISED_DATA);
    }
    // SAFETY: the System Table declares this many configuration entries.
    let tables = unsafe {
        slice::from_raw_parts(system.configuration_table, system.number_of_table_entries)
    };
    let Some(table) = tables
        .iter()
        .find(|table| table.vendor_guid == efi::MEMORY_ATTRIBUTES_TABLE_GUID)
    else {
        // Firmware without a MAT maps runtime code from the ordinary memory
        // map, where this image is already EfiRuntimeServicesCode.
        return Ok(MemoryAttributePatches::new());
    };
    if table.vendor_table.is_null() {
        return Err(efi::Status::COMPROMISED_DATA);
    }
    let header = table.vendor_table.cast::<efi::MemoryAttributesTable<0>>();
    // SAFETY: the matching configuration-table entry owns a MAT header.
    let (version, count, descriptor_size) = unsafe {
        (
            (*header).version,
            (*header).number_of_entries as usize,
            (*header).descriptor_size as usize,
        )
    };
    // Version 2 only renamed the fourth header word from Reserved to Flags;
    // the descriptor vector layout used here is unchanged from version 1.
    if !matches!(version, 1 | 2)
        || count > 4096
        || descriptor_size < mem::size_of::<efi::MemoryDescriptor>()
        || descriptor_size > PAGE_SIZE as usize
    {
        return Err(efi::Status::COMPROMISED_DATA);
    }
    let entries = (table.vendor_table as usize)
        .checked_add(mem::size_of::<efi::MemoryAttributesTable<0>>())
        .ok_or(efi::Status::COMPROMISED_DATA)?;
    descriptor_size
        .checked_mul(count)
        .and_then(|size| entries.checked_add(size))
        .ok_or(efi::Status::COMPROMISED_DATA)?;

    let mut patches = MemoryAttributePatches::new();
    patches.system_table = system_table;
    patches.table = table.vendor_table;
    let mut covered_until = image_base;
    for index in 0..count {
        let address = entries
            .checked_add(
                index
                    .checked_mul(descriptor_size)
                    .ok_or(efi::Status::COMPROMISED_DATA)?,
            )
            .ok_or(efi::Status::COMPROMISED_DATA)?;
        let descriptor = address as *mut efi::MemoryDescriptor;
        // SAFETY: descriptor stride and table bounds were validated above.
        let value = unsafe { ptr::read_unaligned(descriptor) };
        let length = value
            .number_of_pages
            .checked_mul(PAGE_SIZE)
            .ok_or(efi::Status::COMPROMISED_DATA)?;
        let descriptor_end = value
            .physical_start
            .checked_add(length)
            .ok_or(efi::Status::COMPROMISED_DATA)?;
        if descriptor_end <= image_base || value.physical_start >= image_end {
            continue;
        }
        if value.r#type != efi::RUNTIME_SERVICES_CODE
            || value.attribute & efi::MEMORY_RUNTIME == 0
            || value.physical_start > covered_until
        {
            patches.restore();
            return Err(efi::Status::COMPROMISED_DATA);
        }
        covered_until = covered_until.max(descriptor_end.min(image_end));
        let patched = value.attribute & !(efi::MEMORY_RO | efi::MEMORY_XP);
        if patched != value.attribute {
            // SAFETY: attribute is within the validated descriptor header.
            let attribute = unsafe { ptr::addr_of_mut!((*descriptor).attribute) };
            if let Err(status) = patches.push(attribute, value.attribute) {
                patches.restore();
                return Err(status);
            }
            // SAFETY: the firmware-owned MAT is writable before EBS and has no
            // checksum. This changes only the selected descriptor's flags.
            unsafe { ptr::write_unaligned(attribute, patched) };
        }
    }
    if covered_until < image_end {
        patches.restore();
        return Err(efi::Status::COMPROMISED_DATA);
    }
    Ok(patches)
}

/// Returns the current MAT configuration-table pointer without retaining a
/// reference to firmware memory.
unsafe fn current_memory_attributes_table(
    system_table: *mut efi::SystemTable,
) -> Option<*mut c_void> {
    if system_table.is_null() {
        return None;
    }
    // SAFETY: callers use this only before a failed launch can exit Boot
    // Services; the System Table therefore remains physical and accessible.
    let system = unsafe { &*system_table };
    if system.number_of_table_entries == 0 {
        return None;
    }
    if system.number_of_table_entries > 4096 || system.configuration_table.is_null() {
        return None;
    }
    // SAFETY: the System Table declares this configuration-table array.
    unsafe { slice::from_raw_parts(system.configuration_table, system.number_of_table_entries) }
        .iter()
        .find(|table| table.vendor_guid == efi::MEMORY_ATTRIBUTES_TABLE_GUID)
        .map(|table| table.vendor_table)
}

/// Recomputes the CRC of the firmware-owned Runtime Services table.
unsafe fn update_runtime_crc(
    runtime_services: *mut efi::RuntimeServices,
    boot_services: *mut efi::BootServices,
) -> efi::Status {
    // SAFETY: caller owns temporary mutation of the live firmware table.
    let runtime = unsafe { &mut *runtime_services };
    runtime.hdr.crc32 = 0;
    let mut crc = 0;
    // SAFETY: the table is valid for its firmware-provided header size.
    let status = unsafe {
        ((*boot_services).calculate_crc32)(
            runtime_services.cast(),
            runtime.hdr.header_size as usize,
            &mut crc,
        )
    };
    if !status.is_error() {
        runtime.hdr.crc32 = crc;
    }
    status
}

/// Converts saved firmware entry points that are no longer present in `gRT`.
unsafe extern "efiapi" fn virtual_address_change_notify(_event: efi::Event, _context: *mut c_void) {
    let address = ORIGINAL_CONVERT_POINTER.load(Ordering::Acquire);
    if address == 0 {
        clear_saved_entries();
        return;
    }
    // SAFETY: the saved address is the physical ConvertPointer entry captured
    // from the live Runtime Services table.
    let convert: efi::RuntimeConvertPointer = unsafe { mem::transmute(address) };
    convert_saved_entry(convert, &ORIGINAL_GET_VARIABLE);
    convert_saved_entry(convert, &ORIGINAL_GET_NEXT_VARIABLE_NAME);
    convert_saved_entry(convert, &ORIGINAL_SET_VARIABLE);
}

fn convert_saved_entry(convert: efi::RuntimeConvertPointer, entry: &AtomicUsize) {
    let mut pointer = entry.load(Ordering::Acquire) as *mut c_void;
    if pointer.is_null() {
        return;
    }
    // SAFETY: called only by the virtual-address-change notification while
    // ConvertPointer is valid in physical mode.
    let status = unsafe { convert(0, &mut pointer) };
    entry.store(
        if status.is_error() {
            0
        } else {
            pointer as usize
        },
        Ordering::Release,
    );
}

fn clear_saved_entries() {
    PROFILE.store(UNINSTALLED_PROFILE, Ordering::Release);
    ORIGINAL_GET_VARIABLE.store(0, Ordering::Release);
    ORIGINAL_GET_NEXT_VARIABLE_NAME.store(0, Ordering::Release);
    ORIGINAL_SET_VARIABLE.store(0, Ordering::Release);
    ORIGINAL_CONVERT_POINTER.store(0, Ordering::Release);
}

unsafe extern "efiapi" fn get_variable(
    variable_name: *mut efi::Char16,
    vendor_guid: *mut efi::Guid,
    attributes: *mut u32,
    data_size: *mut usize,
    data: *mut c_void,
) -> efi::Status {
    let Some(original) = saved_get_variable() else {
        return efi::Status::DEVICE_ERROR;
    };
    // SAFETY: this UEFI service received the firmware-defined name/GUID pair.
    let Some((name, guid)) = (unsafe { policy_key(variable_name, vendor_guid) }) else {
        // SAFETY: preserve the firmware's validation and status for shared or
        // malformed inputs.
        return unsafe { original(variable_name, vendor_guid, attributes, data_size, data) };
    };
    let Some(mapped) = map_private_variable(selected_profile(), guid, name) else {
        return unsafe { original(variable_name, vendor_guid, attributes, data_size, data) };
    };
    let mut backend_name = [0; BACKEND_NAME_CAPACITY + 1];
    backend_name[..mapped.name().len()].copy_from_slice(mapped.name());
    let mut backend_guid = to_efi_guid(mapped.guid());
    // SAFETY: mapped storage is NUL terminated and lives through the call.
    unsafe {
        original(
            backend_name.as_mut_ptr(),
            &mut backend_guid,
            attributes,
            data_size,
            data,
        )
    }
}

unsafe extern "efiapi" fn set_variable(
    variable_name: *mut efi::Char16,
    vendor_guid: *mut efi::Guid,
    attributes: u32,
    data_size: usize,
    data: *mut c_void,
) -> efi::Status {
    let Some(original) = saved_set_variable() else {
        return efi::Status::DEVICE_ERROR;
    };
    // SAFETY: this UEFI service received the firmware-defined name/GUID pair.
    let Some((name, guid)) = (unsafe { policy_key(variable_name, vendor_guid) }) else {
        return unsafe { original(variable_name, vendor_guid, attributes, data_size, data) };
    };
    let Some(mapped) = map_private_variable(selected_profile(), guid, name) else {
        return unsafe { original(variable_name, vendor_guid, attributes, data_size, data) };
    };
    let mut backend_name = [0; BACKEND_NAME_CAPACITY + 1];
    backend_name[..mapped.name().len()].copy_from_slice(mapped.name());
    let mut backend_guid = to_efi_guid(mapped.guid());
    // SAFETY: mapped storage is NUL terminated and lives through the call.
    unsafe {
        original(
            backend_name.as_mut_ptr(),
            &mut backend_guid,
            attributes,
            data_size,
            data,
        )
    }
}

unsafe extern "efiapi" fn get_next_variable_name(
    variable_name_size: *mut usize,
    variable_name: *mut efi::Char16,
    vendor_guid: *mut efi::Guid,
) -> efi::Status {
    let Some(original) = saved_get_next_variable_name() else {
        return efi::Status::DEVICE_ERROR;
    };
    if variable_name_size.is_null() || variable_name.is_null() || vendor_guid.is_null() {
        // SAFETY: let firmware produce its native invalid-parameter result.
        return unsafe { original(variable_name_size, variable_name, vendor_guid) };
    }
    // Normal UEFI callers may not reenter GetNextVariableName or its
    // variable-services reentry group while one call is busy. A blocking lock
    // models that ABI and avoids weak-CAS spurious failures. The MCE/INIT/NMI
    // reentry exception is not supported by this single-vCPU smoke.
    let mut scratch = ENUM_NAME.lock();
    // SAFETY: validated non-NULL above; UEFI owns the caller's buffer.
    let caller_size = unsafe { *variable_name_size };
    if caller_size < mem::size_of::<efi::Char16>() {
        return efi::Status::INVALID_PARAMETER;
    }
    let caller_units = caller_size / mem::size_of::<efi::Char16>();
    // SAFETY: VariableNameSize declares the accessible caller buffer.
    let caller_name = unsafe { slice::from_raw_parts(variable_name, caller_units) };
    let Some(cursor_len) = caller_name.iter().position(|&unit| unit == 0) else {
        return efi::Status::INVALID_PARAMETER;
    };
    if cursor_len + 1 > ENUM_NAME_CAPACITY {
        return efi::Status::DEVICE_ERROR;
    }

    let logical_guid = from_efi_guid(unsafe { &*vendor_guid });
    let mut backend_guid = unsafe { *vendor_guid };
    if let Some(mapped) =
        map_private_variable(selected_profile(), logical_guid, &caller_name[..cursor_len])
    {
        scratch[..mapped.name().len()].copy_from_slice(mapped.name());
        scratch[mapped.name().len()] = 0;
        backend_guid = to_efi_guid(mapped.guid());
    } else {
        scratch[..=cursor_len].copy_from_slice(&caller_name[..=cursor_len]);
    }

    loop {
        let mut backend_size = ENUM_NAME_CAPACITY * mem::size_of::<efi::Char16>();
        // SAFETY: scratch and GUID are writable for the duration of the call.
        let status =
            unsafe { original(&mut backend_size, scratch.as_mut_ptr(), &mut backend_guid) };
        if status == efi::Status::BUFFER_TOO_SMALL {
            return efi::Status::DEVICE_ERROR;
        }
        if status.is_error() {
            return status;
        }
        if backend_size < mem::size_of::<efi::Char16>()
            || backend_size > ENUM_NAME_CAPACITY * mem::size_of::<efi::Char16>()
            || backend_size % mem::size_of::<efi::Char16>() != 0
        {
            return efi::Status::DEVICE_ERROR;
        }
        let backend_units = backend_size / mem::size_of::<efi::Char16>();
        let Some(backend_len) = scratch[..backend_units].iter().position(|&unit| unit == 0) else {
            return efi::Status::DEVICE_ERROR;
        };
        let physical_guid = from_efi_guid(&backend_guid);
        let physical_name = &scratch[..backend_len];
        let logical = if physical_guid == MONITOR_VENDOR_GUID {
            unmap_private_variable(selected_profile(), physical_guid, physical_name)
                .map(|logical| (logical.guid(), logical.name()))
        } else if is_profile_private(physical_guid, physical_name) {
            None
        } else {
            Some((physical_guid, physical_name))
        };
        let Some((logical_guid, logical_name)) = logical else {
            continue;
        };

        let required_size = (logical_name.len() + 1) * mem::size_of::<efi::Char16>();
        if caller_size < required_size {
            unsafe { *variable_name_size = required_size };
            return efi::Status::BUFFER_TOO_SMALL;
        }
        // SAFETY: caller_size was validated against the required logical name.
        unsafe {
            ptr::copy_nonoverlapping(logical_name.as_ptr(), variable_name, logical_name.len());
            *variable_name.add(logical_name.len()) = 0;
            *vendor_guid = to_efi_guid(logical_guid);
            *variable_name_size = required_size;
        }
        return efi::Status::SUCCESS;
    }
}

/// Returns a policy name only when it can be one of the bounded private names.
unsafe fn policy_key<'a>(
    variable_name: *mut efi::Char16,
    vendor_guid: *mut efi::Guid,
) -> Option<(&'a [u16], Guid)> {
    if variable_name.is_null() || vendor_guid.is_null() {
        return None;
    }
    for len in 0..=POLICY_NAME_CAPACITY {
        // SAFETY: UEFI requires a valid NUL-terminated name; bounded reading
        // stops as soon as the name cannot match this policy.
        if unsafe { *variable_name.add(len) } == 0 {
            // SAFETY: the loop just found the terminator after `len` units.
            let name = unsafe { slice::from_raw_parts(variable_name, len) };
            // SAFETY: NULL was rejected above.
            return Some((name, from_efi_guid(unsafe { &*vendor_guid })));
        }
    }
    None
}

fn selected_profile() -> ProfileId {
    ProfileId(PROFILE.load(Ordering::Acquire))
}

fn from_efi_guid(guid: &efi::Guid) -> Guid {
    let (data1, data2, data3, byte0, byte1, node) = guid.as_fields();
    Guid::new(
        data1,
        data2,
        data3,
        [
            byte0, byte1, node[0], node[1], node[2], node[3], node[4], node[5],
        ],
    )
}

fn to_efi_guid(guid: Guid) -> efi::Guid {
    efi::Guid::from_fields(
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        &[
            guid.data4[2],
            guid.data4[3],
            guid.data4[4],
            guid.data4[5],
            guid.data4[6],
            guid.data4[7],
        ],
    )
}

fn saved_get_variable() -> Option<efi::RuntimeGetVariable> {
    let address = ORIGINAL_GET_VARIABLE.load(Ordering::Acquire);
    // SAFETY: nonzero values were captured from a Runtime Services function
    // pointer and may have been updated only by ConvertPointer.
    (address != 0).then(|| unsafe { mem::transmute(address) })
}

fn saved_get_next_variable_name() -> Option<efi::RuntimeGetNextVariableName> {
    let address = ORIGINAL_GET_NEXT_VARIABLE_NAME.load(Ordering::Acquire);
    // SAFETY: see `saved_get_variable`.
    (address != 0).then(|| unsafe { mem::transmute(address) })
}

fn saved_set_variable() -> Option<efi::RuntimeSetVariable> {
    let address = ORIGINAL_SET_VARIABLE.load(Ordering::Acquire);
    // SAFETY: see `saved_get_variable`.
    (address != 0).then(|| unsafe { mem::transmute(address) })
}
