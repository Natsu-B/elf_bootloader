//! One-vCPU VMXON/VMLAUNCH/VMCALL validation.

use crate::SerialPort;
use core::ffi::c_void;
use core::fmt;
use core::fmt::Write;
use core::ptr;
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use nested_vmx::restrict_vmx_capability;
use r_efi::efi;
use x86_64_hal::addr::EptPhys;
use x86_64_hal::addr::VmcsPhys;
use x86_64_hal::addr::VmxonPhys;
use x86_64_hal::cpu;
use x86_64_hal::ept;
use x86_64_hal::vmcs;
use x86_64_hal::vmx;
use x86_64_hal::vmx::VmxStatus;

/// Pages allocated as one reserved monitor block.
const MONITOR_PAGES: usize = 77;
/// First of four page directories mapping the low four gibibytes.
const EPT_PD_FIRST_PAGE: u64 = 4;
/// L1 MSR bitmap, including conservative VMX capability interception.
const MSR_BITMAP_PAGE: u64 = 8;
/// First page used as the host stack.
const HOST_STACK_PAGE: u64 = 9;
/// First page used as the guest stack.
const GUEST_STACK_PAGE: u64 = 13;
/// Linux's EFI path uses more than the 128 KiB stack needed by small payloads.
const GUEST_STACK_PAGES: u64 = 64;
/// One architectural page.
const PAGE_SIZE: u64 = 4096;
/// VMCALL basic exit reason.
const EXIT_REASON_VMCALL: u64 = 18;
/// CPUID basic exit reason.
const EXIT_REASON_CPUID: u64 = 10;
/// XSETBV basic exit reason.
const EXIT_REASON_XSETBV: u64 = 55;
/// RDMSR basic exit reason.
const EXIT_REASON_RDMSR: u64 = 31;
/// Control-register-access basic exit reason.
const EXIT_REASON_CR_ACCESS: u64 = 28;
/// VM-entry interruption information for #GP with an error code.
const INJECT_GENERAL_PROTECTION: u64 = (1 << 31) | (1 << 11) | (3 << 8) | 13;
/// AMD-specific MSR range, which raises #GP when probed on this Intel target.
const AMD_MSR_RANGE: core::ops::RangeInclusive<u32> = 0xc001_0000..=0xc001_ffff;
/// VMX capability MSRs exposed through the conservative nested policy.
const VMX_CAPABILITY_MSR_RANGE: core::ops::RangeInclusive<u32> = 0x480..=0x492;
/// CR4.VMXE, required by the hardware VMCS but initially hidden from L1.
const CR4_VMX_ENABLE: u64 = 1 << 13;
/// CR4.OSXSAVE, required while L0 handles an unconditional XSETBV exit.
const CR4_OSXSAVE: u64 = 1 << 18;
/// Marker written in non-root mode before VMCALL.
const GUEST_MARKER: u64 = 0x7468_696e_6876_4d58;
/// Payload staged by `run-uefi-smoke.sh`.
const GUEST_IMAGE_PATH: [efi::Char16; 23] = [
    b'\\' as u16,
    b'E' as u16,
    b'F' as u16,
    b'I' as u16,
    b'\\' as u16,
    b'B' as u16,
    b'O' as u16,
    b'O' as u16,
    b'T' as u16,
    b'\\' as u16,
    b'G' as u16,
    b'U' as u16,
    b'E' as u16,
    b'S' as u16,
    b'T' as u16,
    b'X' as u16,
    b'6' as u16,
    b'4' as u16,
    b'.' as u16,
    b'E' as u16,
    b'F' as u16,
    b'I' as u16,
    0,
];

static GUEST_RAN: AtomicU64 = AtomicU64::new(0);
static GUEST_STATUS: AtomicUsize = AtomicUsize::new(usize::MAX);
static GUEST_IMAGE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static SYSTEM_TABLE: AtomicPtr<efi::SystemTable> = AtomicPtr::new(ptr::null_mut());
static ORIGINAL_CR0: AtomicU64 = AtomicU64::new(0);
static ORIGINAL_CR4: AtomicU64 = AtomicU64::new(0);
/// XCR0 restored when the bounded smoke leaves VMX operation.
static ORIGINAL_XCR0: AtomicU64 = AtomicU64::new(0);
static CPUID_EXIT_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Guest GPRs that are not stored in the VMCS on VM exit.
#[repr(C)]
struct GuestRegisters {
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
    rbp: u64,
    rsi: u64,
    rdi: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
}

const _: () = assert!(core::mem::size_of::<GuestRegisters>() == 15 * 8);

/// Failure from the bounded VMX smoke launch.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Error {
    /// A required VMX or EPT capability is missing.
    Capability(&'static str, u64),
    /// UEFI page allocation failed.
    Allocate(usize),
    /// A VMX instruction reported architectural failure flags.
    Instruction(&'static str, VmxStatus, u64),
    /// One VMCS field could not be written.
    Vmwrite(u32, VmxStatus, u64),
    /// The smoke-only 4 GiB EPT cannot cover the allocated block or code.
    OutsideIdentityMap(u64),
    /// A UEFI service used to load the nested payload failed.
    Firmware(&'static str, usize),
    /// The staged payload has an invalid or unrepresentable file size.
    GuestImageSize(u64),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Capability(name, value) => write!(formatter, "capability {name}={value:#x}"),
            Self::Allocate(status) => write!(formatter, "AllocatePages status={status:#x}"),
            Self::Instruction(name, status, vm_error) => write!(
                formatter,
                "{name} status={status:?} vm_instruction_error={vm_error:#x}"
            ),
            Self::Vmwrite(field, status, vm_error) => write!(
                formatter,
                "VMWRITE field={field:#x} status={status:?} vm_instruction_error={vm_error:#x}"
            ),
            Self::OutsideIdentityMap(address) => {
                write!(formatter, "smoke EPT does not cover {address:#x}")
            }
            Self::Firmware(service, status) => {
                write!(formatter, "{service} status={status:#x}")
            }
            Self::GuestImageSize(size) => write!(formatter, "invalid guest payload size {size}"),
        }
    }
}

/// Runs the VMX smoke test. Success transfers to `vmexit_entry` and does not return.
pub(crate) fn run(
    parent_image: efi::Handle,
    system_table: *mut efi::SystemTable,
) -> Result<(), Error> {
    let vmx_basic_raw = unsafe { cpu::rdmsr(vmx::IA32_VMX_BASIC) };
    let basic = vmx::VmxBasic::from_msr(vmx_basic_raw);
    if basic.region_size == 0 || usize::from(basic.region_size) > PAGE_SIZE as usize {
        return Err(Error::Capability(
            "VMCS region size",
            u64::from(basic.region_size),
        ));
    }
    if basic.memory_type != 6 {
        return Err(Error::Capability(
            "VMCS memory type",
            u64::from(basic.memory_type),
        ));
    }

    let ept_capability = unsafe { cpu::rdmsr(vmx::IA32_VMX_EPT_VPID_CAP) };
    if ept_capability & ept::REQUIRED_EPT_CAPS != ept::REQUIRED_EPT_CAPS {
        return Err(Error::Capability("EPT", ept_capability));
    }

    let guest_image = load_guest_image(parent_image, system_table)?;
    GUEST_IMAGE.store(guest_image, Ordering::Release);
    SYSTEM_TABLE.store(system_table, Ordering::Release);

    let feature_control = unsafe { cpu::rdmsr(cpu::IA32_FEATURE_CONTROL) };
    if feature_control & 1 == 0 {
        // SAFETY: unlocked IA32_FEATURE_CONTROL may be initialized exactly once at CPL0.
        unsafe { cpu::wrmsr(cpu::IA32_FEATURE_CONTROL, feature_control | 0b101) };
    } else if feature_control & (1 << 2) == 0 {
        return Err(Error::Capability("VMX outside SMX", feature_control));
    }

    let mut block = 0_u64;
    // SAFETY: the firmware owns `system_table`; its boot-services table remains live here.
    let status = unsafe {
        ((*(*system_table).boot_services).allocate_pages)(
            efi::ALLOCATE_ANY_PAGES,
            efi::RESERVED_MEMORY_TYPE,
            MONITOR_PAGES,
            &mut block,
        )
    };
    if status.is_error() {
        return Err(Error::Allocate(status.as_usize()));
    }
    let block_end = block + MONITOR_PAGES as u64 * PAGE_SIZE;
    for address in [
        block_end,
        guest_entry as usize as u64,
        vmexit_entry as usize as u64,
        cpu::read_cr3(),
    ] {
        if address >= 1 << 32 {
            return Err(Error::OutsideIdentityMap(address));
        }
    }

    // SAFETY: AllocatePages returned an exclusive, aligned block of this exact size.
    unsafe { ptr::write_bytes(block as *mut u8, 0, MONITOR_PAGES * PAGE_SIZE as usize) };
    // SAFETY: the first words belong to exclusive VMXON and VMCS pages.
    unsafe {
        ptr::write_volatile(block as *mut u32, basic.revision_id);
        ptr::write_volatile((block + PAGE_SIZE) as *mut u32, basic.revision_id);
        initialize_l1_msr_bitmap(block + MSR_BITMAP_PAGE * PAGE_SIZE);
    }

    let pml4_phys = EptPhys::new(block + 2 * PAGE_SIZE).unwrap();
    let pdpt_phys = EptPhys::new(block + 3 * PAGE_SIZE).unwrap();
    let pd_phys = [
        EptPhys::new(block + EPT_PD_FIRST_PAGE * PAGE_SIZE).unwrap(),
        EptPhys::new(block + (EPT_PD_FIRST_PAGE + 1) * PAGE_SIZE).unwrap(),
        EptPhys::new(block + (EPT_PD_FIRST_PAGE + 2) * PAGE_SIZE).unwrap(),
        EptPhys::new(block + (EPT_PD_FIRST_PAGE + 3) * PAGE_SIZE).unwrap(),
    ];
    // SAFETY: these six exclusive pages are aligned, zeroed, and the final
    // four are one contiguous `[EptPage; 4]` allocation.
    let ept_pointer = unsafe {
        ept::build_identity_4g(
            &mut *((block + 2 * PAGE_SIZE) as *mut ept::EptPage),
            pml4_phys,
            &mut *((block + 3 * PAGE_SIZE) as *mut ept::EptPage),
            pdpt_phys,
            &mut *((block + EPT_PD_FIRST_PAGE * PAGE_SIZE) as *mut [ept::EptPage; 4]),
            pd_phys,
        )
    };

    let original_cr0 = cpu::read_cr0();
    let original_cr4 = cpu::read_cr4();
    let fixed_cr0 = (original_cr0 | unsafe { cpu::rdmsr(vmx::IA32_VMX_CR0_FIXED0) })
        & unsafe { cpu::rdmsr(vmx::IA32_VMX_CR0_FIXED1) };
    let fixed_cr4 = (original_cr4 | (1 << 13) | unsafe { cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED0) })
        & unsafe { cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED1) };
    if cpu::cpuid(1, 0).ecx & (1 << 26) == 0 {
        return Err(Error::Capability("XSAVE", 0));
    }
    let host_cr4 = fixed_cr4 | CR4_OSXSAVE;
    ORIGINAL_CR0.store(original_cr0, Ordering::Relaxed);
    ORIGINAL_CR4.store(original_cr4, Ordering::Relaxed);
    // SAFETY: values were normalized with the CPU's VMX fixed-bit MSRs.
    unsafe {
        cpu::write_cr0(fixed_cr0);
        cpu::write_cr4(host_cr4);
    }
    // SAFETY: CPUID advertised XSAVE and host CR4.OSXSAVE is now set. XCR0 is
    // restored before the original CR4 is restored.
    ORIGINAL_XCR0.store(unsafe { cpu::xgetbv(0) }, Ordering::Relaxed);

    let vmxon = VmxonPhys::new(block).unwrap();
    let vmcs = VmcsPhys::new(block + PAGE_SIZE).unwrap();
    let vmxon_status = unsafe { vmx::vmxon(vmxon) };
    if vmxon_status != VmxStatus::Success {
        restore_control_registers();
        return Err(Error::Instruction("VMXON", vmxon_status, u64::MAX));
    }

    let result = configure_and_launch(
        vmcs,
        ept_pointer,
        block + MSR_BITMAP_PAGE * PAGE_SIZE,
        fixed_cr0,
        fixed_cr4,
        original_cr4,
        host_cr4,
        block + (HOST_STACK_PAGE + 4) * PAGE_SIZE - 8,
        block + (GUEST_STACK_PAGE + GUEST_STACK_PAGES) * PAGE_SIZE - 8,
        basic.true_controls,
    );

    // This is reached only when VM entry failed.
    let _ = unsafe { vmx::vmxoff() };
    restore_control_registers();
    // SAFETY: `guest_image` was returned by LoadImage and was never started on this path.
    let _ = unsafe { ((*(*system_table).boot_services).unload_image)(guest_image) };
    result
}

/// Reads the staged payload through firmware and asks firmware to relocate it.
fn load_guest_image(
    parent_image: efi::Handle,
    system_table: *mut efi::SystemTable,
) -> Result<efi::Handle, Error> {
    let boot_services = unsafe { (*system_table).boot_services };
    let mut loaded_image_guid = efi::protocols::loaded_image::PROTOCOL_GUID;
    let mut loaded_image_interface = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).handle_protocol)(
            parent_image,
            &mut loaded_image_guid,
            &mut loaded_image_interface,
        )
    };
    if status.is_error() {
        return Err(Error::Firmware(
            "HandleProtocol(LoadedImage)",
            status.as_usize(),
        ));
    }
    let loaded_image = loaded_image_interface.cast::<efi::protocols::loaded_image::Protocol>();

    let mut filesystem_guid = efi::protocols::simple_file_system::PROTOCOL_GUID;
    let mut filesystem_interface = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).handle_protocol)(
            (*loaded_image).device_handle,
            &mut filesystem_guid,
            &mut filesystem_interface,
        )
    };
    if status.is_error() {
        return Err(Error::Firmware(
            "HandleProtocol(SimpleFileSystem)",
            status.as_usize(),
        ));
    }
    let filesystem = filesystem_interface.cast::<efi::protocols::simple_file_system::Protocol>();

    let mut root = ptr::null_mut();
    let status = unsafe { ((*filesystem).open_volume)(filesystem, &mut root) };
    if status.is_error() {
        return Err(Error::Firmware("OpenVolume", status.as_usize()));
    }

    let mut guest_path = GUEST_IMAGE_PATH;
    let mut file = ptr::null_mut();
    let status = unsafe {
        ((*root).open)(
            root,
            &mut file,
            guest_path.as_mut_ptr(),
            efi::protocols::file::MODE_READ,
            0,
        )
    };
    if status.is_error() {
        // SAFETY: `root` was returned by OpenVolume and is still open.
        let _ = unsafe { ((*root).close)(root) };
        return Err(Error::Firmware("Open guest payload", status.as_usize()));
    }

    let payload_size = match guest_file_size(boot_services, file) {
        Ok(size) => size,
        Err(error) => {
            close_guest_file(root, file);
            return Err(error);
        }
    };
    let mut buffer = ptr::null_mut();
    let status =
        unsafe { ((*boot_services).allocate_pool)(efi::LOADER_DATA, payload_size, &mut buffer) };
    if status.is_error() {
        close_guest_file(root, file);
        return Err(Error::Firmware(
            "AllocatePool(guest payload)",
            status.as_usize(),
        ));
    }

    let mut offset = 0;
    while offset < payload_size {
        let mut chunk_size = payload_size - offset;
        let status = unsafe {
            ((*file).read)(
                file,
                &mut chunk_size,
                buffer.cast::<u8>().add(offset).cast(),
            )
        };
        if status.is_error() {
            free_guest_buffer(boot_services, buffer);
            close_guest_file(root, file);
            return Err(Error::Firmware("Read guest payload", status.as_usize()));
        }
        if chunk_size == 0 {
            free_guest_buffer(boot_services, buffer);
            close_guest_file(root, file);
            return Err(Error::GuestImageSize(offset as u64));
        }
        offset += chunk_size;
    }
    close_guest_file(root, file);

    let mut guest_image = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).load_image)(
            efi::Boolean::FALSE,
            parent_image,
            ptr::null_mut(),
            buffer,
            payload_size,
            &mut guest_image,
        )
    };
    free_guest_buffer(boot_services, buffer);
    if status.is_error() {
        return Err(Error::Firmware("LoadImage", status.as_usize()));
    }
    Ok(guest_image)
}

fn guest_file_size(
    boot_services: *mut efi::BootServices,
    file: *mut efi::protocols::file::Protocol,
) -> Result<usize, Error> {
    let mut info_guid = efi::protocols::file::INFO_ID;
    let mut info_size = 0_usize;
    let status =
        unsafe { ((*file).get_info)(file, &mut info_guid, &mut info_size, ptr::null_mut()) };
    if status != efi::Status::BUFFER_TOO_SMALL
        || info_size < core::mem::size_of::<efi::protocols::file::Info>()
    {
        return Err(Error::Firmware("GetInfo(size)", status.as_usize()));
    }

    let mut info_buffer = ptr::null_mut();
    let status =
        unsafe { ((*boot_services).allocate_pool)(efi::LOADER_DATA, info_size, &mut info_buffer) };
    if status.is_error() {
        return Err(Error::Firmware(
            "AllocatePool(file info)",
            status.as_usize(),
        ));
    }

    let status = unsafe { ((*file).get_info)(file, &mut info_guid, &mut info_size, info_buffer) };
    if status.is_error() {
        free_guest_buffer(boot_services, info_buffer);
        return Err(Error::Firmware("GetInfo", status.as_usize()));
    }
    let size = unsafe { (*info_buffer.cast::<efi::protocols::file::Info>()).file_size };
    free_guest_buffer(boot_services, info_buffer);
    if size == 0 || size > usize::MAX as u64 {
        return Err(Error::GuestImageSize(size));
    }
    Ok(size as usize)
}

fn close_guest_file(
    root: *mut efi::protocols::file::Protocol,
    file: *mut efi::protocols::file::Protocol,
) {
    // SAFETY: both handles were returned by their corresponding open calls.
    unsafe {
        let _ = ((*file).close)(file);
        let _ = ((*root).close)(root);
    }
}

fn free_guest_buffer(boot_services: *mut efi::BootServices, buffer: *mut c_void) {
    // SAFETY: `buffer` was returned by this table's AllocatePool call.
    let _ = unsafe { ((*boot_services).free_pool)(buffer) };
}

/// Configures the current VMCS and launches the non-root marker.
#[allow(clippy::too_many_arguments)]
fn configure_and_launch(
    vmcs_page: VmcsPhys,
    ept_pointer: u64,
    msr_bitmap: u64,
    host_cr0: u64,
    guest_cr4_hardware: u64,
    guest_cr4_shadow: u64,
    host_cr4: u64,
    host_rsp: u64,
    guest_rsp: u64,
    true_controls: bool,
) -> Result<(), Error> {
    require("VMCLEAR", unsafe { vmx::vmclear(vmcs_page) })?;
    require("VMPTRLD", unsafe { vmx::vmptrld(vmcs_page) })?;

    let pin_msr = if true_controls {
        vmx::IA32_VMX_TRUE_PINBASED_CTLS
    } else {
        vmx::IA32_VMX_PINBASED_CTLS
    };
    let primary_msr = if true_controls {
        vmx::IA32_VMX_TRUE_PROCBASED_CTLS
    } else {
        vmx::IA32_VMX_PROCBASED_CTLS
    };
    let exit_msr = if true_controls {
        vmx::IA32_VMX_TRUE_EXIT_CTLS
    } else {
        vmx::IA32_VMX_EXIT_CTLS
    };
    let entry_msr = if true_controls {
        vmx::IA32_VMX_TRUE_ENTRY_CTLS
    } else {
        vmx::IA32_VMX_ENTRY_CTLS
    };
    let pin = vmx::adjust_controls(0, unsafe { cpu::rdmsr(pin_msr) });
    let primary = vmx::adjust_controls(
        vmcs::PRIMARY_EXEC_ACTIVATE_SECONDARY_CONTROLS | vmcs::PRIMARY_EXEC_USE_MSR_BITMAPS,
        unsafe { cpu::rdmsr(primary_msr) },
    );
    let secondary = vmx::adjust_controls(
        vmcs::SECONDARY_EXEC_ENABLE_EPT
            | vmcs::SECONDARY_EXEC_ENABLE_RDTSCP
            | vmcs::SECONDARY_EXEC_ENABLE_INVPCID
            | vmcs::SECONDARY_EXEC_ENABLE_XSAVES
            | vmcs::SECONDARY_EXEC_ENABLE_USER_WAIT_PAUSE,
        unsafe { cpu::rdmsr(vmx::IA32_VMX_PROCBASED_CTLS2) },
    );
    let exit = vmx::adjust_controls(vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE, unsafe {
        cpu::rdmsr(exit_msr)
    });
    let entry = vmx::adjust_controls(vmcs::VM_ENTRY_IA32E_MODE, unsafe { cpu::rdmsr(entry_msr) });
    if primary & vmcs::PRIMARY_EXEC_ACTIVATE_SECONDARY_CONTROLS == 0
        || primary & vmcs::PRIMARY_EXEC_USE_MSR_BITMAPS == 0
        || secondary & vmcs::SECONDARY_EXEC_ENABLE_EPT == 0
        || secondary & vmcs::SECONDARY_EXEC_ENABLE_RDTSCP == 0
        || secondary & vmcs::SECONDARY_EXEC_ENABLE_INVPCID == 0
        || secondary & vmcs::SECONDARY_EXEC_ENABLE_XSAVES == 0
        || secondary & vmcs::SECONDARY_EXEC_ENABLE_USER_WAIT_PAUSE == 0
        || exit & vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE == 0
        || entry & vmcs::VM_ENTRY_IA32E_MODE == 0
    {
        return Err(Error::Capability(
            "VM-entry controls",
            (u64::from(primary) << 32) | u64::from(secondary),
        ));
    }

    for (field, value) in [
        (vmcs::PIN_BASED_VM_EXEC_CONTROL, u64::from(pin)),
        (vmcs::CPU_BASED_VM_EXEC_CONTROL, u64::from(primary)),
        (vmcs::SECONDARY_VM_EXEC_CONTROL, u64::from(secondary)),
        (vmcs::VM_EXIT_CONTROLS, u64::from(exit)),
        (vmcs::VM_ENTRY_CONTROLS, u64::from(entry)),
        (vmcs::EXCEPTION_BITMAP, 0),
        (vmcs::PAGE_FAULT_ERROR_CODE_MASK, 0),
        (vmcs::PAGE_FAULT_ERROR_CODE_MATCH, 0),
        (vmcs::CR3_TARGET_COUNT, 0),
        (vmcs::VM_EXIT_MSR_STORE_COUNT, 0),
        (vmcs::VM_EXIT_MSR_LOAD_COUNT, 0),
        (vmcs::VM_ENTRY_MSR_LOAD_COUNT, 0),
        (vmcs::VM_ENTRY_INTR_INFO_FIELD, 0),
        (vmcs::CR0_GUEST_HOST_MASK, 0),
        (vmcs::CR4_GUEST_HOST_MASK, CR4_VMX_ENABLE),
        (vmcs::CR0_READ_SHADOW, host_cr0),
        (vmcs::CR4_READ_SHADOW, guest_cr4_shadow),
        (vmcs::MSR_BITMAP, msr_bitmap),
        (vmcs::EPT_POINTER, ept_pointer),
    ] {
        write_vmcs(field, value)?;
    }

    write_guest_state(host_cr0, guest_cr4_hardware, guest_rsp)?;
    write_host_state(host_cr0, host_cr4, host_rsp)?;
    log_guest_state();

    GUEST_RAN.store(0, Ordering::Release);
    GUEST_STATUS.store(usize::MAX, Ordering::Release);
    CPUID_EXIT_COUNT.store(0, Ordering::Relaxed);
    let launch = unsafe { vmx::vmlaunch() };
    Err(Error::Instruction(
        "VMLAUNCH",
        launch,
        vm_instruction_error(),
    ))
}

/// Writes current long-mode state as the initial guest state.
fn write_guest_state(cr0: u64, cr4: u64, stack: u64) -> Result<(), Error> {
    let gdtr = cpu::sgdt();
    let idtr = cpu::sidt();
    let es = guest_segment(cpu::read_es(), 0);
    let cs = guest_segment(cpu::read_cs(), 0);
    let ss = guest_segment(cpu::read_ss(), 0);
    let ds = guest_segment(cpu::read_ds(), 0);
    let fs = guest_segment(cpu::read_fs(), unsafe { cpu::rdmsr(cpu::IA32_FS_BASE) });
    let gs = guest_segment(cpu::read_gs(), unsafe { cpu::rdmsr(cpu::IA32_GS_BASE) });
    let ldtr_selector = cpu::read_ldtr();
    let ldtr = guest_system_segment(gdtr, ldtr_selector, 0, vmcs::GUEST_SEGMENT_UNUSABLE);
    let tr_selector = cpu::read_tr();
    let tr = guest_system_segment(gdtr, tr_selector, 0x67, 0x8b);

    for (field, value) in [
        (vmcs::GUEST_ES_SELECTOR, u64::from(es.selector)),
        (vmcs::GUEST_CS_SELECTOR, u64::from(cs.selector)),
        (vmcs::GUEST_SS_SELECTOR, u64::from(ss.selector)),
        (vmcs::GUEST_DS_SELECTOR, u64::from(ds.selector)),
        (vmcs::GUEST_FS_SELECTOR, u64::from(fs.selector)),
        (vmcs::GUEST_GS_SELECTOR, u64::from(gs.selector)),
        (vmcs::GUEST_LDTR_SELECTOR, u64::from(ldtr.selector)),
        (vmcs::GUEST_TR_SELECTOR, u64::from(tr.selector)),
        (vmcs::GUEST_ES_LIMIT, u64::from(es.limit)),
        (vmcs::GUEST_CS_LIMIT, u64::from(cs.limit)),
        (vmcs::GUEST_SS_LIMIT, u64::from(ss.limit)),
        (vmcs::GUEST_DS_LIMIT, u64::from(ds.limit)),
        (vmcs::GUEST_FS_LIMIT, u64::from(fs.limit)),
        (vmcs::GUEST_GS_LIMIT, u64::from(gs.limit)),
        (vmcs::GUEST_LDTR_LIMIT, u64::from(ldtr.limit)),
        (vmcs::GUEST_TR_LIMIT, u64::from(tr.limit)),
        (vmcs::GUEST_GDTR_LIMIT, u64::from(gdtr.limit)),
        (vmcs::GUEST_IDTR_LIMIT, u64::from(idtr.limit)),
        (vmcs::GUEST_ES_AR_BYTES, u64::from(es.access_rights)),
        (vmcs::GUEST_CS_AR_BYTES, u64::from(cs.access_rights)),
        (vmcs::GUEST_SS_AR_BYTES, u64::from(ss.access_rights)),
        (vmcs::GUEST_DS_AR_BYTES, u64::from(ds.access_rights)),
        (vmcs::GUEST_FS_AR_BYTES, u64::from(fs.access_rights)),
        (vmcs::GUEST_GS_AR_BYTES, u64::from(gs.access_rights)),
        (vmcs::GUEST_LDTR_AR_BYTES, u64::from(ldtr.access_rights)),
        (vmcs::GUEST_TR_AR_BYTES, u64::from(tr.access_rights)),
        (vmcs::GUEST_CR0, cr0),
        (vmcs::GUEST_CR3, cpu::read_cr3()),
        (vmcs::GUEST_CR4, cr4),
        (vmcs::GUEST_ES_BASE, es.base),
        (vmcs::GUEST_CS_BASE, cs.base),
        (vmcs::GUEST_SS_BASE, ss.base),
        (vmcs::GUEST_DS_BASE, ds.base),
        (vmcs::GUEST_FS_BASE, fs.base),
        (vmcs::GUEST_GS_BASE, gs.base),
        (vmcs::GUEST_LDTR_BASE, ldtr.base),
        (vmcs::GUEST_TR_BASE, tr.base),
        (vmcs::GUEST_GDTR_BASE, gdtr.base),
        (vmcs::GUEST_IDTR_BASE, idtr.base),
        (vmcs::GUEST_DR7, cpu::read_dr7()),
        (vmcs::GUEST_RSP, stack),
        (vmcs::GUEST_RIP, guest_entry as usize as u64),
        (vmcs::GUEST_RFLAGS, 2),
        (vmcs::GUEST_PENDING_DBG_EXCEPTIONS, 0),
        (vmcs::GUEST_INTERRUPTIBILITY_INFO, 0),
        (vmcs::GUEST_ACTIVITY_STATE, 0),
        (vmcs::VMCS_LINK_POINTER, u64::MAX),
        (vmcs::GUEST_IA32_DEBUGCTL, 0),
        (vmcs::GUEST_IA32_PAT, unsafe { cpu::rdmsr(cpu::IA32_PAT) }),
        (vmcs::GUEST_IA32_EFER, unsafe { cpu::rdmsr(cpu::IA32_EFER) }),
        (vmcs::GUEST_SYSENTER_CS, unsafe {
            cpu::rdmsr(cpu::IA32_SYSENTER_CS)
        }),
        (vmcs::GUEST_SYSENTER_ESP, unsafe {
            cpu::rdmsr(cpu::IA32_SYSENTER_ESP)
        }),
        (vmcs::GUEST_SYSENTER_EIP, unsafe {
            cpu::rdmsr(cpu::IA32_SYSENTER_EIP)
        }),
    ] {
        write_vmcs(field, value)?;
    }
    Ok(())
}

/// Writes the host state used for the first VM exit.
fn write_host_state(cr0: u64, cr4: u64, stack: u64) -> Result<(), Error> {
    let gdtr = cpu::sgdt();
    let idtr = cpu::sidt();
    let current_tr = cpu::read_tr();
    let tr_selector = if current_tr & !7 == 0 {
        8
    } else {
        current_tr & !7
    };
    let tr_base = unsafe { cpu::gdt_segment_base(gdtr, current_tr) }.unwrap_or(0);

    for (field, value) in [
        (vmcs::HOST_ES_SELECTOR, u64::from(cpu::read_es() & !7)),
        (vmcs::HOST_CS_SELECTOR, u64::from(cpu::read_cs() & !7)),
        (vmcs::HOST_SS_SELECTOR, u64::from(cpu::read_ss() & !7)),
        (vmcs::HOST_DS_SELECTOR, u64::from(cpu::read_ds() & !7)),
        (vmcs::HOST_FS_SELECTOR, u64::from(cpu::read_fs() & !7)),
        (vmcs::HOST_GS_SELECTOR, u64::from(cpu::read_gs() & !7)),
        (vmcs::HOST_TR_SELECTOR, u64::from(tr_selector)),
        (vmcs::HOST_CR0, cr0),
        (vmcs::HOST_CR3, cpu::read_cr3()),
        (vmcs::HOST_CR4, cr4),
        (vmcs::HOST_FS_BASE, unsafe { cpu::rdmsr(cpu::IA32_FS_BASE) }),
        (vmcs::HOST_GS_BASE, unsafe { cpu::rdmsr(cpu::IA32_GS_BASE) }),
        (vmcs::HOST_TR_BASE, tr_base),
        (vmcs::HOST_GDTR_BASE, gdtr.base),
        (vmcs::HOST_IDTR_BASE, idtr.base),
        (vmcs::HOST_IA32_SYSENTER_CS, unsafe {
            cpu::rdmsr(cpu::IA32_SYSENTER_CS)
        }),
        (vmcs::HOST_IA32_SYSENTER_ESP, unsafe {
            cpu::rdmsr(cpu::IA32_SYSENTER_ESP)
        }),
        (vmcs::HOST_IA32_SYSENTER_EIP, unsafe {
            cpu::rdmsr(cpu::IA32_SYSENTER_EIP)
        }),
        (vmcs::HOST_IA32_PAT, unsafe { cpu::rdmsr(cpu::IA32_PAT) }),
        (vmcs::HOST_IA32_EFER, unsafe { cpu::rdmsr(cpu::IA32_EFER) }),
        (vmcs::HOST_RSP, stack),
        (vmcs::HOST_RIP, vmexit_entry as usize as u64),
    ] {
        write_vmcs(field, value)?;
    }
    Ok(())
}

/// Compact guest segment state.
#[derive(Clone, Copy)]
struct GuestSegment {
    selector: u16,
    limit: u32,
    access_rights: u32,
    base: u64,
}

fn guest_segment(selector: u16, base: u64) -> GuestSegment {
    match (
        cpu::segment_limit(selector),
        cpu::segment_access_rights(selector),
    ) {
        (Some(limit), Some(access_rights)) => GuestSegment {
            selector,
            limit,
            access_rights,
            base,
        },
        _ => GuestSegment {
            selector: 0,
            limit: 0,
            access_rights: vmcs::GUEST_SEGMENT_UNUSABLE,
            base,
        },
    }
}

fn guest_system_segment(
    gdtr: cpu::DescriptorTable,
    selector: u16,
    fallback_limit: u32,
    fallback_access_rights: u32,
) -> GuestSegment {
    let base = unsafe { cpu::gdt_segment_base(gdtr, selector) };
    match (
        base,
        cpu::segment_limit(selector),
        cpu::segment_access_rights(selector),
    ) {
        (Some(base), Some(limit), Some(access_rights)) => GuestSegment {
            selector,
            limit,
            access_rights,
            base,
        },
        _ if fallback_access_rights == vmcs::GUEST_SEGMENT_UNUSABLE => GuestSegment {
            selector: 0,
            limit: 0,
            access_rights: vmcs::GUEST_SEGMENT_UNUSABLE,
            base: 0,
        },
        _ => GuestSegment {
            selector: 8,
            limit: fallback_limit,
            access_rights: fallback_access_rights,
            base: 0,
        },
    }
}

fn write_vmcs(field: u32, value: u64) -> Result<(), Error> {
    let status = unsafe { vmx::vmwrite(field, value) };
    if status == VmxStatus::Success {
        Ok(())
    } else {
        Err(Error::Vmwrite(field, status, vm_instruction_error()))
    }
}

fn require(instruction: &'static str, status: VmxStatus) -> Result<(), Error> {
    if status == VmxStatus::Success {
        Ok(())
    } else {
        Err(Error::Instruction(
            instruction,
            status,
            vm_instruction_error(),
        ))
    }
}

fn vm_instruction_error() -> u64 {
    unsafe { vmx::vmread(vmcs::VM_INSTRUCTION_ERROR) }.unwrap_or(u64::MAX)
}

fn log_guest_state() {
    let read = |field| unsafe { vmx::vmread(field) }.unwrap_or(u64::MAX);
    let mut serial = SerialPort;
    serial.init();
    let _ = writeln!(
        serial,
        "thin-hv: guest cr0={:#x} cr3={:#x} cr4={:#x} efer={:#x} rip={:#x} rsp={:#x}",
        read(vmcs::GUEST_CR0),
        read(vmcs::GUEST_CR3),
        read(vmcs::GUEST_CR4),
        read(vmcs::GUEST_IA32_EFER),
        read(vmcs::GUEST_RIP),
        read(vmcs::GUEST_RSP),
    );
    let _ = writeln!(
        serial,
        "thin-hv: guest cs={:#x}/{:#x} ss={:#x}/{:#x} tr={:#x}/{:#x}/{:#x} ldtr={:#x}/{:#x}",
        read(vmcs::GUEST_CS_SELECTOR),
        read(vmcs::GUEST_CS_AR_BYTES),
        read(vmcs::GUEST_SS_SELECTOR),
        read(vmcs::GUEST_SS_AR_BYTES),
        read(vmcs::GUEST_TR_SELECTOR),
        read(vmcs::GUEST_TR_BASE),
        read(vmcs::GUEST_TR_AR_BYTES),
        read(vmcs::GUEST_LDTR_SELECTOR),
        read(vmcs::GUEST_LDTR_AR_BYTES),
    );
}

fn restore_control_registers() {
    // SAFETY: these are the exact values captured before enabling VMX.
    unsafe {
        cpu::xsetbv(0, ORIGINAL_XCR0.load(Ordering::Relaxed));
        cpu::write_cr4(ORIGINAL_CR4.load(Ordering::Relaxed));
        cpu::write_cr0(ORIGINAL_CR0.load(Ordering::Relaxed));
    }
}

/// First non-root instruction stream.
extern "C" fn guest_entry() -> ! {
    GUEST_RAN.store(GUEST_MARKER, Ordering::Release);
    let system_table = SYSTEM_TABLE.load(Ordering::Acquire);
    let guest_image = GUEST_IMAGE.load(Ordering::Acquire);
    let mut exit_data_size = 0_usize;
    let mut exit_data = ptr::null_mut();
    // SAFETY: both pointers were captured in root mode before VMLAUNCH, and
    // UEFI Boot Services are still active for this late-launch smoke test.
    let status = unsafe {
        ((*(*system_table).boot_services).start_image)(
            guest_image,
            &mut exit_data_size,
            &mut exit_data,
        )
    };
    GUEST_STATUS.store(status.as_usize(), Ordering::Release);
    // SAFETY: this guest runs specifically under the VMCALL smoke handler.
    unsafe { vmx::vmcall() };
    loop {
        core::hint::spin_loop();
    }
}

/// Hardware VM-exit target for the smoke VMCS.
#[unsafe(naked)]
extern "sysv64" fn vmexit_entry() -> ! {
    core::arch::naked_asm!(
        "push r15",
        "push r14",
        "push r13",
        "push r12",
        "push r11",
        "push r10",
        "push r9",
        "push r8",
        "push rdi",
        "push rsi",
        "push rbp",
        "push rdx",
        "push rcx",
        "push rbx",
        "push rax",
        "mov rdi, rsp",
        "call {dispatch}",
        "pop rax",
        "pop rbx",
        "pop rcx",
        "pop rdx",
        "pop rbp",
        "pop rsi",
        "pop rdi",
        "pop r8",
        "pop r9",
        "pop r10",
        "pop r11",
        "pop r12",
        "pop r13",
        "pop r14",
        "pop r15",
        "vmresume",
        "pushfq",
        "push r15",
        "push r14",
        "push r13",
        "push r12",
        "push r11",
        "push r10",
        "push r9",
        "push r8",
        "push rdi",
        "push rsi",
        "push rbp",
        "push rdx",
        "push rcx",
        "push rbx",
        "push rax",
        "sub rsp, 8",
        "lea rdi, [rsp + 8]",
        "mov rsi, [rsp + 128]",
        "call {resume_failed}",
        "ud2",
        dispatch = sym vmexit_dispatch,
        resume_failed = sym vmresume_failed,
    );
}

/// Handles one VM exit and returns only when the guest can be resumed.
unsafe extern "sysv64" fn vmexit_dispatch(registers: *mut GuestRegisters) {
    // SAFETY: `vmexit_entry` passes its live, uniquely owned stack frame.
    let registers = unsafe { &mut *registers };
    let reason = unsafe { vmx::vmread(vmcs::VM_EXIT_REASON) }.unwrap_or(u64::MAX);
    let qualification = unsafe { vmx::vmread(vmcs::EXIT_QUALIFICATION) }.unwrap_or(u64::MAX);
    let guest_rip = unsafe { vmx::vmread(vmcs::GUEST_RIP) }.unwrap_or(u64::MAX);
    let instruction_len = unsafe { vmx::vmread(vmcs::VM_EXIT_INSTRUCTION_LEN) }.unwrap_or(u64::MAX);

    // VM-entry event fields persist in the VMCS after delivery.
    let clear_event = unsafe { vmx::vmwrite(vmcs::VM_ENTRY_INTR_INFO_FIELD, 0) };
    if clear_event != VmxStatus::Success {
        stop_unexpected_exit(
            "clearing VM-entry event failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_CPUID {
        CPUID_EXIT_COUNT.fetch_add(1, Ordering::Relaxed);
        let leaf = registers.rax as u32;
        let subleaf = registers.rcx as u32;
        let mut result = if (0x4000_0000..=0x4fff_ffff).contains(&leaf) {
            cpu::CpuidResult {
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0,
            }
        } else {
            cpu::cpuid(leaf, subleaf)
        };
        if leaf == 1 {
            result.ecx |= 1 << 5;
            result.ecx &= !(1 << 31);
        }
        registers.rax = u64::from(result.eax);
        registers.rbx = u64::from(result.ebx);
        registers.rcx = u64::from(result.ecx);
        registers.rdx = u64::from(result.edx);

        advance_guest_rip(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_XSETBV {
        // SAFETY: L1 is trusted and supplied the architectural ECX/EDX:EAX
        // operands. Invalid values are not yet converted into a guest #GP.
        // ponytail: validate XCR0 dependencies and inject #GP before accepting
        // untrusted L1 input; the current Linux L1 is part of the TCB.
        unsafe {
            cpu::xsetbv(
                registers.rcx as u32,
                (registers.rdx << 32) | (registers.rax & u64::from(u32::MAX)),
            );
        }
        // ponytail: this one-vCPU smoke shares extended register state with
        // L0; add per-vCPU XSAVE switching before SMP or L2 workloads.
        advance_guest_rip(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_RDMSR {
        let msr = registers.rcx as u32;
        if VMX_CAPABILITY_MSR_RANGE.contains(&msr) {
            let Some(value) = l1_vmx_capability(msr) else {
                inject_general_protection(
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
                return;
            };
            registers.rax = value & u64::from(u32::MAX);
            registers.rdx = value >> 32;
            advance_guest_rip(reason, qualification, guest_rip, instruction_len, registers);
            return;
        }
        if AMD_MSR_RANGE.contains(&msr) {
            inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
            return;
        }
    }

    if reason & (1 << 31) == 0
        && reason & 0xffff == EXIT_REASON_CR_ACCESS
        && qualification & 0x3f == 4
    {
        let register = ((qualification >> 8) & 0xf) as u8;
        let Some(value) = guest_gpr(registers, register) else {
            stop_unexpected_exit(
                "invalid CR4 source register",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        };
        let fixed = (value | CR4_VMX_ENABLE | unsafe { cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED0) })
            & unsafe { cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED1) };
        for (field, field_value) in [(vmcs::GUEST_CR4, fixed), (vmcs::CR4_READ_SHADOW, value)] {
            let status = unsafe { vmx::vmwrite(field, field_value) };
            if status != VmxStatus::Success {
                stop_unexpected_exit(
                    "virtualizing CR4 write failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            }
        }
        advance_guest_rip(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_VMCALL {
        log_vmexit(reason, qualification, guest_rip, instruction_len);
        finish_vmcall(reason);
    }

    stop_unexpected_exit(
        "unhandled VM exit",
        reason,
        qualification,
        guest_rip,
        instruction_len,
        registers,
    );
}

/// Enables read exits for the complete VMX capability range.
///
/// # Safety
///
/// `bitmap` must name an exclusive, zeroed architectural MSR-bitmap page.
unsafe fn initialize_l1_msr_bitmap(bitmap: u64) {
    for (offset, value) in [(0x90, 0xff), (0x91, 0xff), (0x92, 0x07)] {
        // SAFETY: the three offsets are within the caller-owned 4 KiB page.
        unsafe { ptr::write_volatile((bitmap as *mut u8).add(offset), value) };
    }
}

/// Returns one masked VMX capability without touching absent optional MSRs.
fn l1_vmx_capability(msr: u32) -> Option<u64> {
    let hardware = match msr {
        vmx::IA32_VMX_VMFUNC | vmx::IA32_VMX_PROCBASED_CTLS3 => 0,
        _ => unsafe { cpu::rdmsr(msr) },
    };
    restrict_vmx_capability(msr, hardware)
}

/// Reads the GPR encoding used by control-register exit qualification.
fn guest_gpr(registers: &GuestRegisters, index: u8) -> Option<u64> {
    Some(match index {
        0 => registers.rax,
        1 => registers.rcx,
        2 => registers.rdx,
        3 => registers.rbx,
        4 => unsafe { vmx::vmread(vmcs::GUEST_RSP) }.ok()?,
        5 => registers.rbp,
        6 => registers.rsi,
        7 => registers.rdi,
        8 => registers.r8,
        9 => registers.r9,
        10 => registers.r10,
        11 => registers.r11,
        12 => registers.r12,
        13 => registers.r13,
        14 => registers.r14,
        15 => registers.r15,
        _ => return None,
    })
}

/// Injects the fault an unsupported bitmap-outside MSR would raise on Intel.
fn inject_general_protection(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    for (field, value) in [
        (vmcs::VM_ENTRY_EXCEPTION_ERROR_CODE, 0),
        (vmcs::VM_ENTRY_INTR_INFO_FIELD, INJECT_GENERAL_PROTECTION),
    ] {
        let status = unsafe { vmx::vmwrite(field, value) };
        if status != VmxStatus::Success {
            stop_unexpected_exit(
                "injecting guest #GP failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }
    }
}

/// Advances past one instruction handled entirely by L0.
fn advance_guest_rip(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    let Some(next_rip) = guest_rip.checked_add(instruction_len) else {
        stop_unexpected_exit(
            "guest RIP overflow",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    let status = unsafe { vmx::vmwrite(vmcs::GUEST_RIP, next_rip) };
    if status != VmxStatus::Success {
        stop_unexpected_exit(
            "VMWRITE(GUEST_RIP) failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
}

/// Logs the common architectural VM-exit state.
fn log_vmexit(reason: u64, qualification: u64, guest_rip: u64, instruction_len: u64) {
    let cpu_id = (cpu::cpuid(1, 0).ebx >> 24) & 0xff;
    let cpuid_exits = CPUID_EXIT_COUNT.load(Ordering::Relaxed);
    let mut serial = SerialPort;
    serial.init();
    let _ = writeln!(
        serial,
        "thin-hv: VMEXIT cpu={cpu_id} level=1 reason={reason:#x} qualification={qualification:#x} guest_rip={guest_rip:#x} instruction_len={instruction_len} cpuid_exits={cpuid_exits}"
    );
}

/// Completes the bounded smoke test after the guest's VMCALL.
fn finish_vmcall(reason: u64) -> ! {
    let marker = GUEST_RAN.load(Ordering::Acquire);
    let guest_status = GUEST_STATUS.load(Ordering::Acquire);
    let vmxoff = leave_vmx();
    let mut serial = SerialPort;
    serial.init();
    if reason & 0xffff == EXIT_REASON_VMCALL
        && marker == GUEST_MARKER
        && guest_status == efi::Status::SUCCESS.as_usize()
        && vmxoff == VmxStatus::Success
    {
        let _ = writeln!(
            serial,
            "thin-hv: vmx guest PASS start_image_status={guest_status:#x}"
        );
    } else {
        let _ = writeln!(
            serial,
            "thin-hv: vmx guest FAIL marker={marker:#x} start_image_status={guest_status:#x} vmxoff={vmxoff:?}"
        );
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Reports an exit that this smoke monitor cannot reflect or handle.
fn stop_unexpected_exit(
    message: &str,
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) -> ! {
    let vm_error = vm_instruction_error();
    let instruction_info = unsafe { vmx::vmread(vmcs::VMX_INSTRUCTION_INFO) }.unwrap_or(u64::MAX);
    let cpuid_exits = CPUID_EXIT_COUNT.load(Ordering::Relaxed);
    let mut serial = SerialPort;
    serial.init();
    let _ = writeln!(
        serial,
        "thin-hv: vmx guest FAIL: {message} reason={reason:#x} qualification={qualification:#x} instruction_info={instruction_info:#x} guest_rip={guest_rip:#x} instruction_len={instruction_len} vm_instruction_error={vm_error:#x} cpuid_exits={cpuid_exits} rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x}",
        registers.rax, registers.rbx, registers.rcx, registers.rdx,
    );
    let vmxoff = leave_vmx();
    let _ = writeln!(serial, "thin-hv: VMXOFF status={vmxoff:?}");
    loop {
        core::hint::spin_loop();
    }
}

/// Reports a VMRESUME architectural failure reached from the assembly stub.
unsafe extern "sysv64" fn vmresume_failed(registers: *const GuestRegisters, rflags: u64) -> ! {
    // SAFETY: `vmexit_entry` passes its still-live saved-register frame.
    let registers = unsafe { &*registers };
    let status = if rflags & 1 != 0 {
        VmxStatus::FailInvalid
    } else if rflags & (1 << 6) != 0 {
        VmxStatus::FailValid
    } else {
        VmxStatus::Success
    };
    let vm_error = if status == VmxStatus::FailValid {
        vm_instruction_error()
    } else {
        u64::MAX
    };
    let reason = unsafe { vmx::vmread(vmcs::VM_EXIT_REASON) }.unwrap_or(u64::MAX);
    let qualification = unsafe { vmx::vmread(vmcs::EXIT_QUALIFICATION) }.unwrap_or(u64::MAX);
    let guest_rip = unsafe { vmx::vmread(vmcs::GUEST_RIP) }.unwrap_or(u64::MAX);
    let instruction_len = unsafe { vmx::vmread(vmcs::VM_EXIT_INSTRUCTION_LEN) }.unwrap_or(u64::MAX);
    let cpuid_exits = CPUID_EXIT_COUNT.load(Ordering::Relaxed);
    let mut serial = SerialPort;
    serial.init();
    let _ = writeln!(
        serial,
        "thin-hv: VMRESUME FAIL status={status:?} rflags={rflags:#x} vm_instruction_error={vm_error:#x} reason={reason:#x} qualification={qualification:#x} guest_rip={guest_rip:#x} instruction_len={instruction_len} cpuid_exits={cpuid_exits} rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x}",
        registers.rax, registers.rbx, registers.rcx, registers.rdx,
    );
    let vmxoff = leave_vmx();
    let _ = writeln!(serial, "thin-hv: VMXOFF status={vmxoff:?}");
    loop {
        core::hint::spin_loop();
    }
}

/// Leaves VMX operation and restores the pre-smoke control registers on success.
fn leave_vmx() -> VmxStatus {
    let status = unsafe { vmx::vmxoff() };
    if status == VmxStatus::Success {
        restore_control_registers();
    }
    status
}
