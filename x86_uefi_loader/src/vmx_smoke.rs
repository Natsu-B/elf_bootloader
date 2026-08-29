//! One-vCPU VMXON/VMLAUNCH/VMCALL validation.

use crate::SerialPort;
use crate::runtime_variables;
use core::ffi::c_void;
use core::fmt;
use core::fmt::Write;
use core::ptr;
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use mutex::SpinLock;
use nested_vmx::DIRECT_VMCS_PATCH_MANIFEST;
use nested_vmx::EXIT_ACKNOWLEDGE_INTERRUPT;
use nested_vmx::PIN_EXTERNAL_INTERRUPT_EXITING;
use nested_vmx::PRIMARY_INTERRUPT_WINDOW_EXITING;
use nested_vmx::PatchKind;
use nested_vmx::VMXERR_INVALID_INVEPT_INVVPID_OPERAND;
use nested_vmx::VMXERR_VMCLEAR_INVALID_ADDRESS;
use nested_vmx::VMXERR_VMCLEAR_VMXON_POINTER;
use nested_vmx::VMXERR_VMPTRLD_INVALID_ADDRESS;
use nested_vmx::VMXERR_VMPTRLD_VMXON_POINTER;
use nested_vmx::VcpuState;
use nested_vmx::VmEntryInstruction;
use nested_vmx::VmInstructionResult;
use nested_vmx::VmcsField;
use nested_vmx::restrict_vmx_capability;
use r_efi::efi;
use uefi_variable_overlay::ProfileId;
use x86_64_hal::addr::EptPhys;
use x86_64_hal::addr::VmcsPhys;
use x86_64_hal::addr::VmxonPhys;
use x86_64_hal::cpu;
use x86_64_hal::ept;
use x86_64_hal::paging;
use x86_64_hal::vmcs;
use x86_64_hal::vmx;
use x86_64_hal::vmx::VmxStatus;

/// Pages allocated as one reserved monitor block.
const MONITOR_PAGES: usize = 91;
/// First of eight page directories mapping the low eight gibibytes.
const EPT_PD_FIRST_PAGE: u64 = 4;
/// L1 MSR bitmap, including conservative VMX capability interception.
const MSR_BITMAP_PAGE: u64 = 12;
/// First page used as the host stack.
const HOST_STACK_PAGE: u64 = 13;
/// First page used as the guest stack.
const GUEST_STACK_PAGE: u64 = 17;
/// Linux's EFI path uses more than the 128 KiB stack needed by small payloads.
const GUEST_STACK_PAGES: u64 = 64;
/// PML4 page for the L0-owned eight-gibibyte identity map.
const HOST_PML4_PAGE: u64 = 81;
/// PDPT page for the L0-owned eight-gibibyte identity map.
const HOST_PDPT_PAGE: u64 = 82;
/// First of eight L0-owned page directories.
const HOST_PD_FIRST_PAGE: u64 = 83;
/// One architectural page.
const PAGE_SIZE: u64 = 4096;
/// Upper bound of the smoke monitor's identity-mapped physical space.
const IDENTITY_MAP_LIMIT: u64 = 1 << 33;
/// VMCALL basic exit reason.
const EXIT_REASON_VMCALL: u64 = 18;
/// CPUID basic exit reason.
const EXIT_REASON_CPUID: u64 = 10;
/// External-interrupt basic exit reason.
const EXIT_REASON_EXTERNAL_INTERRUPT: u64 = 1;
/// Interrupt-window basic exit reason.
const EXIT_REASON_INTERRUPT_WINDOW: u64 = 7;
/// XSETBV basic exit reason.
const EXIT_REASON_XSETBV: u64 = 55;
/// RDMSR basic exit reason.
const EXIT_REASON_RDMSR: u64 = 31;
/// Control-register-access basic exit reason.
const EXIT_REASON_CR_ACCESS: u64 = 28;
/// VMXOFF basic exit reason.
const EXIT_REASON_VMXOFF: u64 = 26;
/// VMXON basic exit reason.
const EXIT_REASON_VMXON: u64 = 27;
/// VMCLEAR basic exit reason.
const EXIT_REASON_VMCLEAR: u64 = 19;
/// VMLAUNCH basic exit reason.
const EXIT_REASON_VMLAUNCH: u64 = 20;
/// VMRESUME basic exit reason.
const EXIT_REASON_VMRESUME: u64 = 24;
/// VMPTRLD basic exit reason.
const EXIT_REASON_VMPTRLD: u64 = 21;
/// VMPTRST basic exit reason.
const EXIT_REASON_VMPTRST: u64 = 22;
/// VMREAD basic exit reason.
const EXIT_REASON_VMREAD: u64 = 23;
/// VMWRITE basic exit reason.
const EXIT_REASON_VMWRITE: u64 = 25;
/// INVEPT basic exit reason.
const EXIT_REASON_INVEPT: u64 = 50;
/// INVVPID basic exit reason.
const EXIT_REASON_INVVPID: u64 = 53;
/// Continue the current carrier VMCS after dispatch.
const VMEXIT_ACTION_RESUME: u64 = 0;
/// Restore the saved GPR frame and enter L1's direct VMCS.
const VMEXIT_ACTION_VMLAUNCH: u64 = 1;
/// Restore the saved GPR frame and resume L1's direct VMCS.
const VMEXIT_ACTION_VMRESUME: u64 = 2;
/// VM-entry interruption information for #GP with an error code.
const INJECT_GENERAL_PROTECTION: u64 = (1 << 31) | (1 << 11) | (3 << 8) | 13;
/// VM-entry interruption information for #UD without an error code.
const INJECT_INVALID_OPCODE: u64 = (1 << 31) | (3 << 8) | 6;
/// Valid VM-entry interruption information for an external interrupt vector.
const INJECT_EXTERNAL_INTERRUPT: u64 = 1 << 31;
/// AMD-specific MSR range, which raises #GP when probed on this Intel target.
const AMD_MSR_RANGE: core::ops::RangeInclusive<u32> = 0xc001_0000..=0xc001_ffff;
/// VMX capability MSRs exposed through the conservative nested policy.
const VMX_CAPABILITY_MSR_RANGE: core::ops::RangeInclusive<u32> = 0x480..=0x492;
/// CR4.VMXE, required by the hardware VMCS but initially hidden from L1.
const CR4_VMX_ENABLE: u64 = 1 << 13;
/// CR4.LA57, unsupported by the current four-level L0 page table.
const CR4_LA57: u64 = 1 << 12;
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
/// Runtime-driver copy of this monitor staged by `run-uefi-smoke.sh`.
const MONITOR_IMAGE_PATH: [efi::Char16; 25] = [
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
    b'M' as u16,
    b'O' as u16,
    b'N' as u16,
    b'I' as u16,
    b'T' as u16,
    b'O' as u16,
    b'R' as u16,
    b'X' as u16,
    b'6' as u16,
    b'4' as u16,
    b'.' as u16,
    b'E' as u16,
    b'F' as u16,
    b'I' as u16,
    0,
];
/// Windows boot manager on a profile-owned EFI System Partition.
const WINDOWS_BOOT_IMAGE_PATH: [efi::Char16; 33] =
    ascii_uefi_path(b"\\EFI\\Microsoft\\Boot\\bootmgfw.efi\0");
/// Stable profile selected when the staged Linux/test payload is present.
const LINUX_PROFILE: ProfileId = ProfileId(2);
/// Stable profile selected when chainloading the installed Windows ESP.
const WINDOWS_PROFILE: ProfileId = ProfileId(1);

const fn ascii_uefi_path<const N: usize>(ascii: &[u8; N]) -> [efi::Char16; N] {
    let mut path = [0; N];
    let mut index = 0;
    while index < N {
        path[index] = ascii[index] as efi::Char16;
        index += 1;
    }
    path
}

static GUEST_RAN: AtomicU64 = AtomicU64::new(0);
static GUEST_STATUS: AtomicUsize = AtomicUsize::new(usize::MAX);
static GUEST_IMAGE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static SYSTEM_TABLE: AtomicPtr<efi::SystemTable> = AtomicPtr::new(ptr::null_mut());
static ORIGINAL_CR0: AtomicU64 = AtomicU64::new(0);
static ORIGINAL_CR4: AtomicU64 = AtomicU64::new(0);
/// XCR0 restored when the bounded smoke leaves VMX operation.
static ORIGINAL_XCR0: AtomicU64 = AtomicU64::new(0);
static CPUID_EXIT_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Nested VMX state for the current single-vCPU smoke run.
// ponytail: replace this global state with per-pCPU `VcpuState` before SMP.
static L1_VCPU_STATE: SpinLock<VcpuState> = SpinLock::new(VcpuState::new());
/// Host fields of the immutable single-vCPU carrier VMCS.
static CARRIER_PATCH_VALUES: SpinLock<Option<[u64; DIRECT_VMCS_PATCH_MANIFEST.len()]>> =
    SpinLock::new(None);
/// L1-visible fields of the one direct VMCS retained with L0 host patches.
// ponytail: materialize this single cached VMCS on pointer changes; add
// per-VMCS storage only when the trusted single-vCPU path needs concurrency.
static DIRECT_PATCH_VALUES: SpinLock<Option<(VmcsPhys, [u64; DIRECT_VMCS_PATCH_MANIFEST.len()])>> =
    SpinLock::new(None);
/// Validated direct-VMCS entry policy, invalidated by its three writable fields.
static DIRECT_ENTRY_POLICY: SpinLock<Option<(VmcsPhys, u64)>> = SpinLock::new(None);
/// State abandoned on the L0 stack while a direct L2 is running.
// ponytail: one global direct run is sufficient for the current one-pCPU
// probe; move this into per-pCPU storage before enabling SMP.
static NESTED_RUN: SpinLock<Option<NestedRun>> = SpinLock::new(None);

const _: () = assert!(HOST_PD_FIRST_PAGE + 8 == MONITOR_PAGES as u64);

/// Bootstrap-to-runtime handoff copied before entering VMX non-root mode.
#[repr(C)]
#[derive(Clone, Copy)]
struct RuntimeHandoff {
    guest: efi::Handle,
    profile: u32,
    reserved: u32,
}

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

/// Saved state needed to turn one direct hardware exit back into an L1 exit.
#[derive(Clone, Copy)]
struct NestedRun {
    carrier: VmcsPhys,
    direct: VmcsPhys,
    saved_direct: [u64; DIRECT_VMCS_PATCH_MANIFEST.len()],
    exit_loaded_l1_pat: Option<u64>,
    exit_loaded_l1_efer: Option<u64>,
    l1_interruptibility: u64,
    outer_reason: u64,
    outer_qualification: u64,
    outer_rip: u64,
    outer_instruction_len: u64,
}

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
    /// The smoke-only 8 GiB EPT cannot cover the allocated block or code.
    OutsideIdentityMap(u64),
    /// A UEFI service used to load the nested payload failed.
    Firmware(&'static str, usize),
}

impl Error {
    fn is_missing_image(self) -> bool {
        matches!(
            self,
            Self::Firmware("LoadImage", value) if value == efi::Status::NOT_FOUND.as_usize()
        )
    }
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
        }
    }
}

/// Runs the VMX smoke test. Success transfers to `vmexit_entry` and does not return.
pub(crate) fn run(
    parent_image: efi::Handle,
    system_table: *mut efi::SystemTable,
) -> Result<(), Error> {
    let loaded_image = loaded_image_protocol(parent_image, system_table)?;
    if unsafe { (*loaded_image).image_code_type } != efi::RUNTIME_SERVICES_CODE {
        let mut serial = SerialPort;
        serial.init();
        let _ = writeln!(serial, "thin-hv: loading runtime monitor");
        return start_runtime_monitor(parent_image, system_table);
    }
    let mut serial = SerialPort;
    serial.init();
    let _ = writeln!(serial, "thin-hv: runtime monitor active");
    // ponytail: a firmware-loaded runtime PE is enough for the current QEMU
    // path; use a self-relocated resident core before Windows or bare metal.

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
    if restrict_vmx_capability(vmx::IA32_VMX_EPT_VPID_CAP, ept_capability).is_none() {
        return Err(Error::Capability("nested EPT/VPID", ept_capability));
    }

    let (guest_image, profile) = runtime_handoff(loaded_image).ok_or(Error::Firmware(
        "runtime guest-image handoff",
        efi::Status::INVALID_PARAMETER.as_usize(),
    ))?;
    GUEST_IMAGE.store(guest_image, Ordering::Release);
    SYSTEM_TABLE.store(system_table, Ordering::Release);

    let image_base = unsafe { (*loaded_image).image_base } as usize as u64;
    let image_end = image_base
        .checked_add(unsafe { (*loaded_image).image_size })
        .ok_or(Error::OutsideIdentityMap(image_base))?;
    let _ = writeln!(
        serial,
        "thin-hv: runtime image base={image_base:#018x} end={image_end:#018x}"
    );
    if image_base >= IDENTITY_MAP_LIMIT || image_end > IDENTITY_MAP_LIMIT {
        return Err(Error::OutsideIdentityMap(image_end));
    }

    let feature_control = unsafe { cpu::rdmsr(cpu::IA32_FEATURE_CONTROL) };
    if feature_control & 1 == 0 {
        // SAFETY: unlocked IA32_FEATURE_CONTROL may be initialized exactly once at CPL0.
        unsafe { cpu::wrmsr(cpu::IA32_FEATURE_CONTROL, feature_control | 0b101) };
    } else if feature_control & (1 << 2) == 0 {
        return Err(Error::Capability("VMX outside SMX", feature_control));
    }

    let mut block = IDENTITY_MAP_LIMIT - 1;
    // SAFETY: the firmware owns `system_table`; its boot-services table remains live here.
    let status = unsafe {
        ((*(*system_table).boot_services).allocate_pages)(
            efi::ALLOCATE_MAX_ADDRESS,
            efi::RUNTIME_SERVICES_DATA,
            MONITOR_PAGES,
            &mut block,
        )
    };
    if status.is_error() {
        return Err(Error::Allocate(status.as_usize()));
    }
    let block_end = block
        .checked_add(MONITOR_PAGES as u64 * PAGE_SIZE)
        .ok_or(Error::OutsideIdentityMap(block))?;
    if block_end > IDENTITY_MAP_LIMIT {
        return Err(Error::OutsideIdentityMap(block_end));
    }
    for address in [
        guest_entry as usize as u64,
        vmexit_entry as usize as u64,
        cpu::read_cr3(),
    ] {
        if address >= IDENTITY_MAP_LIMIT {
            return Err(Error::OutsideIdentityMap(address));
        }
    }
    let _ = writeln!(
        serial,
        "thin-hv: monitor block={block:#018x} end={block_end:#018x}"
    );

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
    let pd_phys: [EptPhys; 8] = core::array::from_fn(|index| {
        EptPhys::new(block + (EPT_PD_FIRST_PAGE + index as u64) * PAGE_SIZE).unwrap()
    });
    // SAFETY: these ten exclusive pages are aligned and zeroed, and the final
    // eight are one contiguous `[EptPage; 8]` allocation.
    let ept_pointer = unsafe {
        ept::build_identity_8g(
            &mut *((block + 2 * PAGE_SIZE) as *mut ept::EptPage),
            pml4_phys,
            &mut *((block + 3 * PAGE_SIZE) as *mut ept::EptPage),
            pdpt_phys,
            &mut *((block + EPT_PD_FIRST_PAGE * PAGE_SIZE) as *mut [ept::EptPage; 8]),
            pd_phys,
        )
    };
    // SAFETY: the final ten pages are exclusive, aligned, and zeroed.
    let host_cr3 = unsafe { build_host_identity_8g(block) };

    let original_cr0 = cpu::read_cr0();
    let original_cr4 = cpu::read_cr4();
    if original_cr4 & CR4_LA57 != 0 {
        return Err(Error::Capability("CR4.LA57", original_cr4));
    }
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

    let variable_overlay =
        match runtime_variables::install(system_table, profile, image_base, unsafe {
            (*loaded_image).image_size
        }) {
            Ok(overlay) => overlay,
            Err(status) => {
                let _ = unsafe { vmx::vmxoff() };
                restore_control_registers();
                return Err(Error::Firmware(
                    "install variable overlay",
                    status.as_usize(),
                ));
            }
        };
    let _ = writeln!(
        serial,
        "thin-hv: variable overlay profile={} mat_patches={}",
        profile.0,
        variable_overlay.memory_attribute_patch_count()
    );

    let result = configure_and_launch(
        vmcs,
        ept_pointer,
        block + MSR_BITMAP_PAGE * PAGE_SIZE,
        fixed_cr0,
        fixed_cr4,
        original_cr4,
        host_cr4,
        host_cr3,
        block + (HOST_STACK_PAGE + 4) * PAGE_SIZE - 8,
        block + (GUEST_STACK_PAGE + GUEST_STACK_PAGES) * PAGE_SIZE - 8,
        basic.true_controls,
    );

    // This is reached only when VM entry failed.
    let overlay_rollback = variable_overlay.rollback();
    let _ = unsafe { vmx::vmxoff() };
    restore_control_registers();
    match overlay_rollback {
        Ok(()) => result,
        Err(status) => Err(Error::Firmware(
            "restore variable overlay",
            status.as_usize(),
        )),
    }
}

/// Starts a runtime-driver copy whose code survives guest ExitBootServices.
fn start_runtime_monitor(
    parent_image: efi::Handle,
    system_table: *mut efi::SystemTable,
) -> Result<(), Error> {
    let (guest, profile) = match load_image(parent_image, system_table, GUEST_IMAGE_PATH) {
        Ok(image) => (image, LINUX_PROFILE),
        Err(error) if error.is_missing_image() => (
            load_image_from_other_filesystem(parent_image, system_table, WINDOWS_BOOT_IMAGE_PATH)?,
            WINDOWS_PROFILE,
        ),
        Err(error) => return Err(error),
    };
    let monitor = match load_image(parent_image, system_table, MONITOR_IMAGE_PATH) {
        Ok(image) => image,
        Err(error) => {
            let _ = unsafe { ((*(*system_table).boot_services).unload_image)(guest) };
            return Err(error);
        }
    };
    let monitor_loaded = match loaded_image_protocol(monitor, system_table) {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = unsafe { ((*(*system_table).boot_services).unload_image)(monitor) };
            let _ = unsafe { ((*(*system_table).boot_services).unload_image)(guest) };
            return Err(error);
        }
    };
    let mut guest_handoff = RuntimeHandoff {
        guest,
        profile: profile.0,
        reserved: 0,
    };
    unsafe {
        (*monitor_loaded).load_options_size = core::mem::size_of::<RuntimeHandoff>() as u32;
        (*monitor_loaded).load_options = ptr::addr_of_mut!(guest_handoff).cast();
    }
    let mut exit_data_size = 0;
    let mut exit_data = ptr::null_mut();
    let status = unsafe {
        ((*(*system_table).boot_services).start_image)(monitor, &mut exit_data_size, &mut exit_data)
    };
    // A working monitor VM-launches and never returns to this loader copy.
    let _ = unsafe { ((*(*system_table).boot_services).unload_image)(monitor) };
    let _ = unsafe { ((*(*system_table).boot_services).unload_image)(guest) };
    Err(Error::Firmware(
        "StartImage(runtime monitor)",
        status.as_usize(),
    ))
}

/// Reads the target handle and profile supplied by the boot application copy.
fn runtime_handoff(
    loaded_image: *mut efi::protocols::loaded_image::Protocol,
) -> Option<(efi::Handle, ProfileId)> {
    if unsafe { (*loaded_image).load_options_size } as usize
        != core::mem::size_of::<RuntimeHandoff>()
        || unsafe { (*loaded_image).load_options }.is_null()
    {
        return None;
    }
    // SAFETY: the application copy keeps this fixed handoff live for the
    // complete nested StartImage call.
    let handoff =
        unsafe { ptr::read_unaligned((*loaded_image).load_options.cast::<RuntimeHandoff>()) };
    let profile = ProfileId(handoff.profile);
    (!handoff.guest.is_null()
        && handoff.reserved == 0
        && matches!(profile, WINDOWS_PROFILE | LINUX_PROFILE))
    .then_some((handoff.guest, profile))
}

/// Loads one staged image through its complete filesystem device path.
fn load_image<const PATH_SIZE: usize>(
    parent_image: efi::Handle,
    system_table: *mut efi::SystemTable,
    image_path: [efi::Char16; PATH_SIZE],
) -> Result<efi::Handle, Error> {
    let loaded_image = loaded_image_protocol(parent_image, system_table)?;
    let device_handle = unsafe { (*loaded_image).device_handle };
    load_image_on_device(parent_image, system_table, device_handle, image_path)
}

/// Loads one image from a filesystem other than the monitor's own ESP.
fn load_image_from_other_filesystem<const PATH_SIZE: usize>(
    parent_image: efi::Handle,
    system_table: *mut efi::SystemTable,
    image_path: [efi::Char16; PATH_SIZE],
) -> Result<efi::Handle, Error> {
    let boot_services = unsafe { (*system_table).boot_services };
    let parent_loaded = loaded_image_protocol(parent_image, system_table)?;
    let parent_device = unsafe { (*parent_loaded).device_handle };
    let mut filesystem_guid = efi::protocols::simple_file_system::PROTOCOL_GUID;
    let mut handle_count = 0;
    let mut handles = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).locate_handle_buffer)(
            efi::BY_PROTOCOL,
            &mut filesystem_guid,
            ptr::null_mut(),
            &mut handle_count,
            &mut handles,
        )
    };
    if status.is_error() {
        return Err(Error::Firmware(
            "LocateHandleBuffer(SimpleFileSystem)",
            status.as_usize(),
        ));
    }
    if handles.is_null() {
        return Err(Error::Firmware(
            "LocateHandleBuffer(SimpleFileSystem)",
            efi::Status::INVALID_PARAMETER.as_usize(),
        ));
    }

    let mut result = Err(Error::Firmware(
        "LoadImage(other filesystem)",
        efi::Status::NOT_FOUND.as_usize(),
    ));
    // ponytail: firmware order selects the first non-parent Windows ESP;
    // select by profile partition GUID when multiple Windows installs matter.
    for index in 0..handle_count {
        let device_handle = unsafe { *handles.add(index) };
        if device_handle != parent_device {
            match load_image_on_device(parent_image, system_table, device_handle, image_path) {
                Ok(image) => {
                    result = Ok(image);
                    break;
                }
                Err(error) if error.is_missing_image() => {}
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
    }
    free_pool(boot_services, handles.cast());
    result
}

/// Loads one image using a complete path rooted at `device_handle`.
fn load_image_on_device<const PATH_SIZE: usize>(
    parent_image: efi::Handle,
    system_table: *mut efi::SystemTable,
    device_handle: efi::Handle,
    image_path: [efi::Char16; PATH_SIZE],
) -> Result<efi::Handle, Error> {
    let boot_services = unsafe { (*system_table).boot_services };

    let mut device_path_guid = efi::protocols::device_path::PROTOCOL_GUID;
    let mut parent_device_path = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).handle_protocol)(
            device_handle,
            &mut device_path_guid,
            &mut parent_device_path,
        )
    };
    if status.is_error() {
        return Err(Error::Firmware(
            "HandleProtocol(DevicePath)",
            status.as_usize(),
        ));
    }

    let mut utilities_guid = efi::protocols::device_path_utilities::PROTOCOL_GUID;
    let mut utilities_interface = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).locate_protocol)(
            &mut utilities_guid,
            ptr::null_mut(),
            &mut utilities_interface,
        )
    };
    if status.is_error() {
        return Err(Error::Firmware(
            "LocateProtocol(DevicePathUtilities)",
            status.as_usize(),
        ));
    }
    let utilities = utilities_interface.cast::<efi::protocols::device_path_utilities::Protocol>();

    let node_size = PATH_SIZE
        .checked_mul(core::mem::size_of::<efi::Char16>())
        .and_then(|path_size| {
            core::mem::size_of::<efi::protocols::device_path::Protocol>().checked_add(path_size)
        })
        .and_then(|size| u16::try_from(size).ok())
        .ok_or(Error::Firmware(
            "CreateDeviceNode(FilePath)",
            efi::Status::INVALID_PARAMETER.as_usize(),
        ))?;
    let file_path_node = unsafe {
        ((*utilities).create_device_node)(
            efi::protocols::device_path::TYPE_MEDIA,
            efi::protocols::device_path::Media::SUBTYPE_FILE_PATH,
            node_size,
        )
    };
    if file_path_node.is_null() {
        return Err(Error::Firmware(
            "CreateDeviceNode(FilePath)",
            efi::Status::OUT_OF_RESOURCES.as_usize(),
        ));
    }
    unsafe {
        ptr::copy_nonoverlapping(
            image_path.as_ptr(),
            file_path_node
                .cast::<u8>()
                .add(core::mem::size_of::<efi::protocols::device_path::Protocol>())
                .cast::<efi::Char16>(),
            PATH_SIZE,
        );
    }

    let complete_path = unsafe {
        ((*utilities).append_device_node)(parent_device_path.cast(), file_path_node.cast())
    };
    free_pool(boot_services, file_path_node.cast());
    if complete_path.is_null() {
        return Err(Error::Firmware(
            "AppendDeviceNode(FilePath)",
            efi::Status::OUT_OF_RESOURCES.as_usize(),
        ));
    }

    let mut guest_image = ptr::null_mut();
    let status = unsafe {
        ((*boot_services).load_image)(
            efi::Boolean::FALSE,
            parent_image,
            complete_path,
            ptr::null_mut(),
            0,
            &mut guest_image,
        )
    };
    free_pool(boot_services, complete_path.cast());
    if status.is_error() {
        if status == efi::Status::SECURITY_VIOLATION && !guest_image.is_null() {
            let _ = unsafe { ((*boot_services).unload_image)(guest_image) };
        }
        return Err(Error::Firmware("LoadImage", status.as_usize()));
    }
    Ok(guest_image)
}

/// Returns the firmware's metadata for one loaded image handle.
fn loaded_image_protocol(
    image: efi::Handle,
    system_table: *mut efi::SystemTable,
) -> Result<*mut efi::protocols::loaded_image::Protocol, Error> {
    let mut guid = efi::protocols::loaded_image::PROTOCOL_GUID;
    let mut interface = ptr::null_mut();
    let status = unsafe {
        ((*(*system_table).boot_services).handle_protocol)(image, &mut guid, &mut interface)
    };
    if status.is_error() {
        Err(Error::Firmware(
            "HandleProtocol(LoadedImage)",
            status.as_usize(),
        ))
    } else {
        Ok(interface.cast())
    }
}

fn free_pool(boot_services: *mut efi::BootServices, buffer: *mut c_void) {
    // SAFETY: `buffer` was allocated by this firmware's device-path utilities.
    let _ = unsafe { ((*boot_services).free_pool)(buffer) };
}

/// Builds the L0-owned page tables used after firmware memory is reclaimed.
///
/// # Safety
///
/// `block` must point to the exclusive, zeroed `MONITOR_PAGES` allocation.
unsafe fn build_host_identity_8g(block: u64) -> u64 {
    const PRESENT_WRITE: u64 = 0b11;
    const LARGE_PAGE: u64 = 1 << 7;

    let pml4 = block + HOST_PML4_PAGE * PAGE_SIZE;
    let pdpt = block + HOST_PDPT_PAGE * PAGE_SIZE;
    // SAFETY: the caller provides the exclusive ten-page table area.
    unsafe { ptr::write_volatile(pml4 as *mut u64, pdpt | PRESENT_WRITE) };
    for directory in 0_u64..8 {
        let pd = block + (HOST_PD_FIRST_PAGE + directory) * PAGE_SIZE;
        // SAFETY: each index is within its exclusive 512-entry page.
        unsafe {
            ptr::write_volatile(
                (pdpt as *mut u64).add(directory as usize),
                pd | PRESENT_WRITE,
            );
        }
        for entry in 0_u64..512 {
            let physical = (directory * 512 + entry) << 21;
            // ponytail: 2 MiB RWX leaves cover the trusted 8 GiB smoke map;
            // split and protect only when enforcing an untrusted-L1 boundary.
            unsafe {
                ptr::write_volatile(
                    (pd as *mut u64).add(entry as usize),
                    physical | PRESENT_WRITE | LARGE_PAGE,
                );
            }
        }
    }
    pml4
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
    host_cr3: u64,
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
    let pin_capability = unsafe { cpu::rdmsr(pin_msr) };
    let pin = vmx::adjust_controls(PIN_EXTERNAL_INTERRUPT_EXITING, pin_capability);
    let primary_capability = unsafe { cpu::rdmsr(primary_msr) };
    let primary = vmx::adjust_controls(
        vmcs::PRIMARY_EXEC_ACTIVATE_SECONDARY_CONTROLS | vmcs::PRIMARY_EXEC_USE_MSR_BITMAPS,
        primary_capability,
    );
    let secondary = vmx::adjust_controls(
        vmcs::SECONDARY_EXEC_ENABLE_EPT
            | vmcs::SECONDARY_EXEC_ENABLE_RDTSCP
            | vmcs::SECONDARY_EXEC_ENABLE_INVPCID
            | vmcs::SECONDARY_EXEC_ENABLE_XSAVES
            | vmcs::SECONDARY_EXEC_ENABLE_USER_WAIT_PAUSE,
        unsafe { cpu::rdmsr(vmx::IA32_VMX_PROCBASED_CTLS2) },
    );
    // ponytail: keep the trusted L1 PAT live across carrier exits so KVM's
    // all-clear direct PAT controls retain native semantics.
    let exit_capability = unsafe { cpu::rdmsr(exit_msr) };
    let exit = vmx::adjust_controls(
        vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE
            | vmcs::VM_EXIT_SAVE_IA32_PAT
            | vmcs::VM_EXIT_SAVE_IA32_EFER
            | vmcs::VM_EXIT_LOAD_IA32_EFER,
        exit_capability,
    );
    let entry = vmx::adjust_controls(
        vmcs::VM_ENTRY_IA32E_MODE | vmcs::VM_ENTRY_LOAD_IA32_PAT | vmcs::VM_ENTRY_LOAD_IA32_EFER,
        unsafe { cpu::rdmsr(entry_msr) },
    );
    if primary & vmcs::PRIMARY_EXEC_ACTIVATE_SECONDARY_CONTROLS == 0
        || primary & vmcs::PRIMARY_EXEC_USE_MSR_BITMAPS == 0
        || secondary & vmcs::SECONDARY_EXEC_ENABLE_EPT == 0
        || secondary & vmcs::SECONDARY_EXEC_ENABLE_RDTSCP == 0
        || secondary & vmcs::SECONDARY_EXEC_ENABLE_INVPCID == 0
        || secondary & vmcs::SECONDARY_EXEC_ENABLE_XSAVES == 0
        || secondary & vmcs::SECONDARY_EXEC_ENABLE_USER_WAIT_PAUSE == 0
        || pin & PIN_EXTERNAL_INTERRUPT_EXITING == 0
        || pin_capability as u32 & PIN_EXTERNAL_INTERRUPT_EXITING != 0
        || primary & PRIMARY_INTERRUPT_WINDOW_EXITING != 0
        || (primary_capability >> 32) as u32 & PRIMARY_INTERRUPT_WINDOW_EXITING == 0
        || exit & vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE == 0
        || exit & EXIT_ACKNOWLEDGE_INTERRUPT != 0
        || (exit_capability >> 32) as u32 & EXIT_ACKNOWLEDGE_INTERRUPT == 0
        || exit & vmcs::VM_EXIT_SAVE_IA32_PAT == 0
        || exit & vmcs::VM_EXIT_LOAD_IA32_PAT != 0
        || (exit_capability >> 32) as u32 & vmcs::VM_EXIT_LOAD_IA32_PAT == 0
        || exit & vmcs::VM_EXIT_SAVE_IA32_EFER == 0
        || exit & vmcs::VM_EXIT_LOAD_IA32_EFER == 0
        || entry & vmcs::VM_ENTRY_IA32E_MODE == 0
        || entry & vmcs::VM_ENTRY_LOAD_IA32_PAT == 0
        || entry & vmcs::VM_ENTRY_LOAD_IA32_EFER == 0
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
    write_host_state(host_cr0, host_cr3, host_cr4, host_rsp)?;
    log_guest_state();

    GUEST_RAN.store(0, Ordering::Release);
    GUEST_STATUS.store(usize::MAX, Ordering::Release);
    CPUID_EXIT_COUNT.store(0, Ordering::Relaxed);
    *L1_VCPU_STATE.lock() = VcpuState::new();
    *CARRIER_PATCH_VALUES.lock() = None;
    *DIRECT_PATCH_VALUES.lock() = None;
    *DIRECT_ENTRY_POLICY.lock() = None;
    *NESTED_RUN.lock() = None;
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
fn write_host_state(cr0: u64, cr3: u64, cr4: u64, stack: u64) -> Result<(), Error> {
    // ponytail: reuse firmware descriptor tables on this fault-free one-vCPU
    // path; install private GDT/IDT/TSS before untrusted input or bare metal.
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
        (vmcs::HOST_CR3, cr3),
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
        "cmp rax, {vmlaunch_action}",
        "je 2f",
        "cmp rax, {vmresume_action}",
        "je 4f",
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
        "3:",
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
        "2:",
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
        "vmlaunch",
        "jmp 5f",
        "4:",
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
        "5:",
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
        "call {entry_failed}",
        "add rsp, 8",
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
        "add rsp, 8",
        "jmp 3b",
        dispatch = sym vmexit_dispatch,
        entry_failed = sym nested_vmentry_failed,
        resume_failed = sym vmresume_failed,
        vmlaunch_action = const VMEXIT_ACTION_VMLAUNCH,
        vmresume_action = const VMEXIT_ACTION_VMRESUME,
    );
}

/// Handles one VM exit and returns only when the guest can be resumed.
unsafe extern "sysv64" fn vmexit_dispatch(registers: *mut GuestRegisters) -> u64 {
    // SAFETY: `vmexit_entry` passes its live, uniquely owned stack frame.
    let registers = unsafe { &mut *registers };
    let nested_run = NESTED_RUN.lock().take();
    if let Some(run) = nested_run {
        reflect_l2_vmexit(&run, registers);
        return VMEXIT_ACTION_RESUME;
    }

    let reason = unsafe { vmx::vmread(vmcs::VM_EXIT_REASON) }.unwrap_or(u64::MAX);
    let qualification = unsafe { vmx::vmread(vmcs::EXIT_QUALIFICATION) }.unwrap_or(u64::MAX);
    let guest_rip = unsafe { vmx::vmread(vmcs::GUEST_RIP) }.unwrap_or(u64::MAX);
    let instruction_len = unsafe { vmx::vmread(vmcs::VM_EXIT_INSTRUCTION_LEN) }.unwrap_or(u64::MAX);

    // VM-entry event fields persist in the VMCS after delivery.
    let clear_event = unsafe { vmx::vmwrite(vmcs::VM_ENTRY_INTR_INFO_FIELD, 0) };
    if clear_event != VmxStatus::Success {
        stop_unexpected_exit(
            b"clearing VM-entry event failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_EXTERNAL_INTERRUPT {
        let interruption = unsafe { vmx::vmread(vmcs::VM_EXIT_INTR_INFO) }.unwrap_or(0);
        let Ok(acknowledged) = acknowledged_external_interrupt(interruption) else {
            stop_unexpected_exit(
                b"invalid acknowledged external interrupt",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        };
        let prepared = if let Some(interruption) = acknowledged {
            guest_accepts_external_interrupt() == Some(true)
                && set_carrier_interrupt_controls(true, false, false)
                && (unsafe { vmx::vmwrite(vmcs::VM_ENTRY_INTR_INFO_FIELD, interruption) })
                    == VmxStatus::Success
        } else {
            match guest_accepts_external_interrupt() {
                Some(true) => set_carrier_interrupt_controls(true, true, false),
                Some(false) => set_carrier_interrupt_controls(false, false, true),
                None => false,
            }
        };
        if !prepared {
            stop_unexpected_exit(
                b"preparing external interrupt failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }
        return VMEXIT_ACTION_RESUME;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_INTERRUPT_WINDOW {
        if !set_carrier_interrupt_controls(true, false, false) {
            stop_unexpected_exit(
                b"completing interrupt-window exit failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }
        return VMEXIT_ACTION_RESUME;
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
        return VMEXIT_ACTION_RESUME;
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
        return VMEXIT_ACTION_RESUME;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_RDMSR {
        let msr = registers.rcx as u32;
        if VMX_CAPABILITY_MSR_RANGE.contains(&msr) {
            log_l1_vmxon(b"capability_msr", u64::from(msr));
            let Some(value) = l1_vmx_capability(msr) else {
                inject_general_protection(
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
                return VMEXIT_ACTION_RESUME;
            };
            registers.rax = value & u64::from(u32::MAX);
            registers.rdx = value >> 32;
            advance_guest_rip(reason, qualification, guest_rip, instruction_len, registers);
            return VMEXIT_ACTION_RESUME;
        }
        if AMD_MSR_RANGE.contains(&msr) {
            inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
            return VMEXIT_ACTION_RESUME;
        }
    }

    if reason & (1 << 31) == 0
        && reason & 0xffff == EXIT_REASON_CR_ACCESS
        && qualification & 0x3f == 4
    {
        let register = ((qualification >> 8) & 0xf) as u8;
        let Some(value) = guest_gpr(registers, register) else {
            stop_unexpected_exit(
                b"invalid CR4 source register",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        };
        let fixed = (value | CR4_VMX_ENABLE | unsafe { cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED0) })
            & unsafe { cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED1) };
        log_l1_vmxon(b"cr4_write", value);
        for (field, field_value) in [(vmcs::GUEST_CR4, fixed), (vmcs::CR4_READ_SHADOW, value)] {
            let status = unsafe { vmx::vmwrite(field, field_value) };
            if status != VmxStatus::Success {
                stop_unexpected_exit(
                    b"virtualizing CR4 write failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            }
        }
        advance_guest_rip(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_VMXON {
        handle_l1_vmxon(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_VMXOFF {
        handle_l1_vmxoff(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_VMCLEAR {
        handle_l1_vmclear(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_VMPTRLD {
        handle_l1_vmptrld(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_VMPTRST {
        handle_l1_vmptrst(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }

    if reason & (1 << 31) == 0
        && matches!(reason & 0xffff, EXIT_REASON_VMLAUNCH | EXIT_REASON_VMRESUME)
    {
        let instruction = if reason & 0xffff == EXIT_REASON_VMLAUNCH {
            VmEntryInstruction::Vmlaunch
        } else {
            VmEntryInstruction::Vmresume
        };
        return handle_l1_vmentry(
            instruction,
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }

    if reason & (1 << 31) == 0
        && matches!(reason & 0xffff, EXIT_REASON_VMREAD | EXIT_REASON_VMWRITE)
    {
        handle_l1_vmcs_access(
            reason & 0xffff == EXIT_REASON_VMWRITE,
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return VMEXIT_ACTION_RESUME;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_INVEPT {
        handle_l1_invept(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_INVVPID {
        handle_l1_invvpid(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_VMCALL {
        log_vmexit(reason, qualification, guest_rip, instruction_len);
        finish_vmcall(reason);
    }

    stop_unexpected_exit(
        b"unhandled VM exit",
        reason,
        qualification,
        guest_rip,
        instruction_len,
        registers,
    );
}

/// Returns one vector acknowledged by VM exit, ignoring an invalid field.
fn acknowledged_external_interrupt(interruption: u64) -> Result<Option<u64>, ()> {
    if interruption & INJECT_EXTERNAL_INTERRUPT == 0 {
        return Ok(None);
    }
    if interruption & !0xff != INJECT_EXTERNAL_INTERRUPT {
        return Err(());
    }
    Ok(Some(INJECT_EXTERNAL_INTERRUPT | (interruption & 0xff)))
}

/// Reports whether L1 can accept an external interrupt on the next VM entry.
fn guest_accepts_external_interrupt() -> Option<bool> {
    let rflags = unsafe { vmx::vmread(vmcs::GUEST_RFLAGS) }.ok()?;
    let interruptibility = unsafe { vmx::vmread(vmcs::GUEST_INTERRUPTIBILITY_INFO) }.ok()?;
    let activity = unsafe { vmx::vmread(vmcs::GUEST_ACTIVITY_STATE) }.ok()?;
    if activity > 1 {
        return None;
    }
    Some(rflags & (1 << 9) != 0 && interruptibility & 0b11 == 0)
}

/// Updates carrier interrupt controls while preserving every unrelated bit.
fn set_carrier_interrupt_controls(external: bool, acknowledge: bool, window: bool) -> bool {
    for (field, mask, enabled) in [
        (
            vmcs::VM_EXIT_CONTROLS,
            u64::from(EXIT_ACKNOWLEDGE_INTERRUPT),
            acknowledge,
        ),
        (
            vmcs::CPU_BASED_VM_EXEC_CONTROL,
            u64::from(PRIMARY_INTERRUPT_WINDOW_EXITING),
            window,
        ),
        (
            vmcs::PIN_BASED_VM_EXEC_CONTROL,
            u64::from(PIN_EXTERNAL_INTERRUPT_EXITING),
            external,
        ),
    ] {
        let Ok(value) = (unsafe { vmx::vmread(field) }) else {
            return false;
        };
        let value = if enabled { value | mask } else { value & !mask };
        if unsafe { vmx::vmwrite(field, value) } != VmxStatus::Success {
            return false;
        }
    }
    true
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

/// Emulates L1's transition into VMX operation while L0 remains VMX root.
fn handle_l1_vmxon(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    log_l1_vmxon(b"entry", reason);
    let cr4_shadow = unsafe { vmx::vmread(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmx::vmread(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    if L1_VCPU_STATE.lock().in_vmx_operation() {
        complete_vmx_instruction(
            VmInstructionResult::VmfailInvalid,
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return;
    }
    let cr0 = unsafe { vmx::vmread(vmcs::GUEST_CR0) }.unwrap_or(0);
    if !vmx_control_registers_valid(cr0, cr4_shadow) {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    let region_address =
        read_l1_vmx_pointer(reason, qualification, guest_rip, instruction_len, registers);
    log_l1_vmxon(b"region", region_address);

    let result = validate_l1_vmxon_region(region_address).map_or(
        VmInstructionResult::VmfailInvalid,
        |region| {
            if !materialize_direct_patch(None) {
                stop_unexpected_exit(
                    b"restoring direct VMCS before VMXON failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            }
            L1_VCPU_STATE.lock().record_vmxon_success(region);
            VmInstructionResult::Vmsucceed
        },
    );
    complete_vmx_instruction(
        result,
        reason,
        qualification,
        guest_rip,
        instruction_len,
        registers,
    );
}

/// Emulates L1's exit from VMX operation while L0 remains in VMX root mode.
fn handle_l1_vmxoff(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    let state = *L1_VCPU_STATE.lock();
    let cr4_shadow = unsafe { vmx::vmread(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmx::vmread(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    if !materialize_direct_patch(None) {
        stop_unexpected_exit(
            b"restoring direct VMCS before VMXOFF failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    L1_VCPU_STATE.lock().record_vmxoff_success();
    complete_vmx_instruction(
        VmInstructionResult::Vmsucceed,
        reason,
        qualification,
        guest_rip,
        instruction_len,
        registers,
    );
}

/// Executes L1's VMCLEAR directly while retaining VMCS01 as current.
fn handle_l1_vmclear(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    let state = *L1_VCPU_STATE.lock();
    let cr4_shadow = unsafe { vmx::vmread(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmx::vmread(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    let address = read_l1_vmx_pointer(reason, qualification, guest_rip, instruction_len, registers);
    log_l1_vmx(b"VMCLEAR", b"region", address);
    let Some(region) = validate_l1_vmcs_address(address) else {
        complete_vmx_instruction(
            l1_vmx_failure(&state, VMXERR_VMCLEAR_INVALID_ADDRESS),
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return;
    };
    if state
        .vmxon_region()
        .is_some_and(|vmxon| vmxon.get() == region.get())
    {
        complete_vmx_instruction(
            l1_vmx_failure(&state, VMXERR_VMCLEAR_VMXON_POINTER),
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return;
    }

    let mut carrier = u64::MAX;
    if unsafe { vmx::vmptrst(&mut carrier) } != VmxStatus::Success || carrier == region.get() {
        // ponytail: the trusted L1 allocator and reserved runtime block are
        // disjoint; stop on a carrier collision instead of virtualizing it.
        stop_unexpected_exit(
            b"VMCLEAR targets VMCS01",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    if !materialize_direct_patch(Some(region)) {
        stop_unexpected_exit(
            b"restoring direct VMCS before VMCLEAR failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    let status = unsafe { vmx::vmclear(region) };
    let result = match status {
        VmxStatus::Success => {
            L1_VCPU_STATE.lock().record_vmclear_success(region);
            VmInstructionResult::Vmsucceed
        }
        VmxStatus::FailInvalid => VmInstructionResult::VmfailInvalid,
        VmxStatus::FailValid => {
            let Some(error) = (unsafe { vmx::vmread(vmcs::VM_INSTRUCTION_ERROR) })
                .ok()
                .and_then(|value| u32::try_from(value).ok())
            else {
                stop_unexpected_exit(
                    b"reading VMCLEAR error failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            };
            l1_vmx_failure(&state, error)
        }
    };
    complete_vmx_instruction(
        result,
        reason,
        qualification,
        guest_rip,
        instruction_len,
        registers,
    );
}

/// Selects L1's direct VMCS while retaining VMCS01 as the hardware carrier.
fn handle_l1_vmptrld(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    let mut carrier_address = u64::MAX;
    if unsafe { vmx::vmptrst(&mut carrier_address) } != VmxStatus::Success {
        stop_unexpected_exit(
            b"saving VMPTRLD carrier failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    let Some(carrier) = validate_l1_vmcs_address(carrier_address) else {
        stop_unexpected_exit(
            b"invalid VMPTRLD carrier",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };

    let state = *L1_VCPU_STATE.lock();
    let cr4_shadow = unsafe { vmx::vmread(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmx::vmread(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    let address = read_l1_vmx_pointer(reason, qualification, guest_rip, instruction_len, registers);
    let Some(region) = validate_l1_vmcs_address(address) else {
        complete_vmx_instruction(
            l1_vmx_failure(&state, VMXERR_VMPTRLD_INVALID_ADDRESS),
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return;
    };
    if state
        .vmxon_region()
        .is_some_and(|vmxon| vmxon.get() == region.get())
    {
        complete_vmx_instruction(
            l1_vmx_failure(&state, VMXERR_VMPTRLD_VMXON_POINTER),
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return;
    }
    if carrier == region {
        // ponytail: L1 memory and the reserved runtime block are disjoint;
        // virtualize a colliding carrier only if that trusted layout changes.
        stop_unexpected_exit(
            b"VMPTRLD targets VMCS01",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    let old_current = state.current_vmcs().map(|current| current.address());
    if old_current != Some(region) && !materialize_direct_patch(None) {
        stop_unexpected_exit(
            b"restoring old direct VMCS before VMPTRLD failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }

    let status = unsafe { vmx::vmptrld(region) };
    let hardware_error = if status == VmxStatus::FailValid {
        (unsafe { vmx::vmread(vmcs::VM_INSTRUCTION_ERROR) })
            .ok()
            .and_then(|value| u32::try_from(value).ok())
    } else {
        None
    };
    if unsafe { vmx::vmptrld(carrier) } != VmxStatus::Success {
        stop_unexpected_exit(
            b"restoring VMPTRLD carrier failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }

    let result = match status {
        VmxStatus::Success => {
            L1_VCPU_STATE.lock().record_vmptrld_success(region);
            VmInstructionResult::Vmsucceed
        }
        VmxStatus::FailInvalid => VmInstructionResult::VmfailInvalid,
        VmxStatus::FailValid => {
            let Some(error) = hardware_error else {
                stop_unexpected_exit(
                    b"reading VMPTRLD error failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            };
            l1_vmx_failure(&state, error)
        }
    };
    complete_vmx_instruction(
        result,
        reason,
        qualification,
        guest_rip,
        instruction_len,
        registers,
    );
}

/// Stores L1's tracked current-VMCS pointer without switching hardware VMCSes.
fn handle_l1_vmptrst(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    let state = *L1_VCPU_STATE.lock();
    let cr4_shadow = unsafe { vmx::vmread(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmx::vmread(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    let instruction_info = unsafe { vmx::vmread(vmcs::VMX_INSTRUCTION_INFO) }
        .ok()
        .and_then(|value| u32::try_from(value).ok());
    let fs_base = unsafe { vmx::vmread(vmcs::GUEST_FS_BASE) }.unwrap_or(u64::MAX);
    let gs_base = unsafe { vmx::vmread(vmcs::GUEST_GS_BASE) }.unwrap_or(u64::MAX);
    let linear = instruction_info.and_then(|information| {
        vmx::memory_operand_address_64(
            information,
            qualification,
            |register| guest_gpr(registers, register),
            fs_base,
            gs_base,
        )
    });
    let Some(linear) = linear else {
        stop_unexpected_exit(
            b"decoding VMPTRST destination failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    let current = state
        .current_vmcs()
        .map_or(u64::MAX, |vmcs| vmcs.address().get());
    if write_l1_linear_u64(linear, current).is_none() {
        // ponytail: trusted long-mode L1 supplies a valid mapped m64
        // destination; synthesize precise memory faults before relaxing it.
        stop_unexpected_exit(
            b"writing VMPTRST destination failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    complete_vmx_instruction(
        VmInstructionResult::Vmsucceed,
        reason,
        qualification,
        guest_rip,
        instruction_len,
        registers,
    );
}

/// Prepares L1's current VMCS for one direct hardware VM entry.
fn handle_l1_vmentry(
    instruction: VmEntryInstruction,
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) -> u64 {
    let state = *L1_VCPU_STATE.lock();
    let cr4_shadow = unsafe { vmx::vmread(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }
    let cs = unsafe { vmx::vmread(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }
    let Some(current) = state.current_vmcs() else {
        complete_vmx_instruction(
            VmInstructionResult::VmfailInvalid,
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return VMEXIT_ACTION_RESUME;
    };

    let mut carrier_address = u64::MAX;
    if unsafe { vmx::vmptrst(&mut carrier_address) } != VmxStatus::Success {
        stop_unexpected_exit(
            b"saving VMLAUNCH carrier failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    let Some(carrier) = validate_l1_vmcs_address(carrier_address) else {
        stop_unexpected_exit(
            b"invalid VMLAUNCH carrier",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    if carrier == current.address() {
        stop_unexpected_exit(
            b"VMLAUNCH targets VMCS01",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    let cached_direct = match *DIRECT_PATCH_VALUES.lock() {
        Some((address, values)) if address == current.address() => Some(values),
        Some(_) => stop_unexpected_exit(
            b"cached direct VMCS does not match current pointer",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        ),
        None => None,
    };
    let carrier_values = if cached_direct.is_some() {
        None
    } else {
        let mut cached = CARRIER_PATCH_VALUES.lock();
        let values = if let Some(values) = *cached {
            values
        } else {
            let Some(values) = read_direct_patch_fields() else {
                stop_unexpected_exit(
                    b"saving carrier host state failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            };
            *cached = Some(values);
            values
        };
        Some(values)
    };
    let l1_interruptibility =
        unsafe { vmx::vmread(vmcs::GUEST_INTERRUPTIBILITY_INFO) }.unwrap_or(u64::MAX) & 8;

    if unsafe { vmx::vmptrld(current.address()) } != VmxStatus::Success {
        stop_unexpected_exit(
            b"selecting L1 VMCS for VMLAUNCH failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    let saved_direct = if let Some(values) = cached_direct {
        values
    } else {
        let Some(values) = read_direct_patch_fields() else {
            stop_unexpected_exit(
                b"saving direct VMCS patch fields failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        };
        values
    };
    let exit_store_count = direct_patch_value(&saved_direct, VmcsField::VmExitMsrStoreCount);
    let exit_load_count = direct_patch_value(&saved_direct, VmcsField::VmExitMsrLoadCount);
    let cached_exit_controls = match *DIRECT_ENTRY_POLICY.lock() {
        Some((address, exit_controls)) if address == current.address() => Some(exit_controls),
        Some(_) => stop_unexpected_exit(
            b"cached entry policy does not match current pointer",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        ),
        None => None,
    };
    let cache_entry_policy = cached_exit_controls.is_none();
    let (exit_controls, supported_entry_policy) = if let Some(exit_controls) = cached_exit_controls
    {
        (exit_controls, true)
    } else {
        let entry_load_count = unsafe { vmx::vmread(vmcs::VM_ENTRY_MSR_LOAD_COUNT) }.ok();
        let exit_controls = unsafe { vmx::vmread(vmcs::VM_EXIT_CONTROLS) }.unwrap_or(u64::MAX);
        let entry_controls = unsafe { vmx::vmread(vmcs::VM_ENTRY_CONTROLS) }.unwrap_or(u64::MAX);
        // ponytail: keep PERF_GLOBAL_CTRL direct because L0 does not use the PMU;
        // outer KVM advertises the VMCS pair even when direct MSR access would #GP.
        let exit_perf_mask = u64::from(vmcs::VM_EXIT_LOAD_IA32_PERF_GLOBAL_CTRL);
        let entry_perf_mask = u64::from(vmcs::VM_ENTRY_LOAD_IA32_PERF_GLOBAL_CTRL);
        let exit_pat_mask = u64::from(vmcs::VM_EXIT_SAVE_IA32_PAT | vmcs::VM_EXIT_LOAD_IA32_PAT);
        let entry_pat_mask = u64::from(vmcs::VM_ENTRY_LOAD_IA32_PAT);
        let exit_pat_controls = exit_controls & exit_pat_mask;
        let entry_pat_controls = entry_controls & entry_pat_mask;
        let supported_pat_controls = (exit_pat_controls == 0 && entry_pat_controls == 0)
            || (exit_pat_controls & u64::from(vmcs::VM_EXIT_LOAD_IA32_PAT) != 0
                && entry_pat_controls == entry_pat_mask);
        let exit_efer_mask = u64::from(vmcs::VM_EXIT_SAVE_IA32_EFER | vmcs::VM_EXIT_LOAD_IA32_EFER);
        let entry_efer_mask = u64::from(vmcs::VM_ENTRY_LOAD_IA32_EFER);
        let exit_efer_controls = exit_controls & exit_efer_mask;
        let entry_efer_controls = entry_controls & entry_efer_mask;
        let supported_efer_controls = (exit_efer_controls == 0 && entry_efer_controls == 0)
            || (exit_efer_controls & u64::from(vmcs::VM_EXIT_LOAD_IA32_EFER) != 0
                && entry_efer_controls == entry_efer_mask);
        let supported = entry_load_count == Some(0)
            && (exit_controls & !(exit_perf_mask | exit_pat_mask | exit_efer_mask)) >> 18 == 0
            && (entry_controls & !(entry_perf_mask | entry_pat_mask | entry_efer_mask)) >> 13 == 0
            && supported_pat_controls
            && supported_efer_controls;
        (exit_controls, supported)
    };
    if exit_store_count != Some(0) || exit_load_count != Some(0) || !supported_entry_policy {
        // ponytail: the measured KVM probe has empty MSR lists. Add bounded L0
        // MSR mirrors when a real workload first supplies a non-empty list.
        // ponytail: PAT controls follow the same all-clear or entry+exit-load
        // ceiling as EFER; add forced-control shadowing before relaxing it.
        // ponytail: accept measured trusted KVM's all-clear controls (its EFER
        // writes are intercepted), or sets that load L2 and restore L1 EFER.
        // Add forced-control shadowing before allowing other partial sets.
        stop_unexpected_exit(
            b"unsupported nested VM-entry state",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    let exit_loaded_l1_pat = (exit_controls & u64::from(vmcs::VM_EXIT_LOAD_IA32_PAT) != 0)
        .then(|| direct_patch_value(&saved_direct, VmcsField::HostIa32Pat))
        .flatten();
    let exit_loaded_l1_efer = (exit_controls & u64::from(vmcs::VM_EXIT_LOAD_IA32_EFER) != 0)
        .then(|| direct_patch_value(&saved_direct, VmcsField::HostIa32Efer))
        .flatten();
    if let Some(carrier_values) = carrier_values {
        if !patch_direct_vmcs(&saved_direct, &carrier_values) {
            stop_unexpected_exit(
                b"patching direct VMCS failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }
        *DIRECT_PATCH_VALUES.lock() = Some((current.address(), saved_direct));
    }
    if cache_entry_policy {
        *DIRECT_ENTRY_POLICY.lock() = Some((current.address(), exit_controls));
    }

    let mut active = NESTED_RUN.lock();
    if active.is_some() {
        stop_unexpected_exit(
            b"nested VMLAUNCH already active",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    *active = Some(NestedRun {
        carrier,
        direct: current.address(),
        saved_direct,
        exit_loaded_l1_pat,
        exit_loaded_l1_efer,
        l1_interruptibility,
        outer_reason: reason,
        outer_qualification: qualification,
        outer_rip: guest_rip,
        outer_instruction_len: instruction_len,
    });
    drop(active);
    match instruction {
        VmEntryInstruction::Vmlaunch => VMEXIT_ACTION_VMLAUNCH,
        VmEntryInstruction::Vmresume => VMEXIT_ACTION_VMRESUME,
    }
}

/// Reads every field whose direct-VMCS value must survive L0 patching.
fn read_direct_patch_fields() -> Option<[u64; DIRECT_VMCS_PATCH_MANIFEST.len()]> {
    let mut values = [0; DIRECT_VMCS_PATCH_MANIFEST.len()];
    for (value, patch) in values.iter_mut().zip(DIRECT_VMCS_PATCH_MANIFEST) {
        *value = unsafe { vmx::vmread(patch.field as u32) }.ok()?;
    }
    Some(values)
}

/// Returns one saved field by its architectural encoding.
fn direct_patch_value(
    values: &[u64; DIRECT_VMCS_PATCH_MANIFEST.len()],
    field: VmcsField,
) -> Option<u64> {
    DIRECT_VMCS_PATCH_MANIFEST
        .iter()
        .position(|patch| patch.field == field)
        .map(|index| values[index])
}

/// Finds a full or architecturally valid high-half manifest field.
fn direct_patch_field(field: u32) -> Option<(usize, bool)> {
    DIRECT_VMCS_PATCH_MANIFEST
        .iter()
        .enumerate()
        .find_map(|(index, patch)| {
            let encoding = patch.field as u32;
            if field == encoding {
                Some((index, false))
            } else if (encoding >> 13) & 3 == 1 && field == (encoding | 1) {
                Some((index, true))
            } else {
                None
            }
        })
}

/// Reads one L1-visible manifest field from retained direct-VMCS state.
fn read_direct_patch_field(
    values: &[u64; DIRECT_VMCS_PATCH_MANIFEST.len()],
    field: u32,
) -> Option<u64> {
    direct_patch_field(field).map(|(index, high)| {
        if high {
            values[index] >> 32
        } else {
            values[index]
        }
    })
}

/// Writes one L1-visible manifest field with architectural width semantics.
fn write_direct_patch_field(
    values: &mut [u64; DIRECT_VMCS_PATCH_MANIFEST.len()],
    field: u32,
    value: u64,
) -> bool {
    let Some((index, high)) = direct_patch_field(field) else {
        return false;
    };
    let encoding = DIRECT_VMCS_PATCH_MANIFEST[index].field as u32;
    values[index] = if high {
        (values[index] & u64::from(u32::MAX)) | ((value & u64::from(u32::MAX)) << 32)
    } else {
        match (encoding >> 13) & 3 {
            0 => value & u64::from(u16::MAX),
            2 => value & u64::from(u32::MAX),
            _ => value,
        }
    };
    true
}

/// Replaces differing L1 host state with VMCS01 state and disables exit MSR lists.
fn patch_direct_vmcs(
    saved_direct: &[u64; DIRECT_VMCS_PATCH_MANIFEST.len()],
    carrier_values: &[u64; DIRECT_VMCS_PATCH_MANIFEST.len()],
) -> bool {
    for (index, patch) in DIRECT_VMCS_PATCH_MANIFEST.iter().enumerate() {
        let value = match patch.kind {
            PatchKind::HostState => carrier_values[index],
            PatchKind::ExitMsrStore | PatchKind::ExitMsrLoad => 0,
        };
        if saved_direct[index] == value {
            continue;
        }
        if unsafe { vmx::vmwrite(patch.field as u32, value) } != VmxStatus::Success {
            return false;
        }
    }
    true
}

/// Restores every L1-visible field before exposing a retained direct VMCS.
fn restore_direct_vmcs(values: &[u64; DIRECT_VMCS_PATCH_MANIFEST.len()]) -> bool {
    for (value, patch) in values.iter().zip(DIRECT_VMCS_PATCH_MANIFEST) {
        if unsafe { vmx::vmwrite(patch.field as u32, *value) } != VmxStatus::Success {
            return false;
        }
    }
    true
}

/// Materializes one retained direct VMCS while VMCS01 is current.
fn materialize_direct_patch(only: Option<VmcsPhys>) -> bool {
    let mut cached = DIRECT_PATCH_VALUES.lock();
    let Some((direct, values)) = *cached else {
        return true;
    };
    if only.is_some_and(|address| address != direct) {
        return true;
    }

    let mut carrier_address = u64::MAX;
    if unsafe { vmx::vmptrst(&mut carrier_address) } != VmxStatus::Success {
        return false;
    }
    let Some(carrier) = validate_l1_vmcs_address(carrier_address) else {
        return false;
    };
    if carrier == direct
        || unsafe { vmx::vmptrld(direct) } != VmxStatus::Success
        || !restore_direct_vmcs(&values)
        || unsafe { vmx::vmptrld(carrier) } != VmxStatus::Success
    {
        return false;
    }
    *cached = None;
    *DIRECT_ENTRY_POLICY.lock() = None;
    true
}

/// Reflects a hardware L2 exit through VMCS01 into Linux KVM's host RIP.
fn reflect_l2_vmexit(run: &NestedRun, registers: &GuestRegisters) {
    let mut current = u64::MAX;
    if unsafe { vmx::vmptrst(&mut current) } != VmxStatus::Success || current != run.direct.get() {
        stop_nested_exit(b"unexpected direct VMCS on L2 exit", run.direct, registers);
    }
    let l1_pat = run
        .exit_loaded_l1_pat
        .unwrap_or_else(|| unsafe { cpu::rdmsr(cpu::IA32_PAT) });
    let l1_efer = run
        .exit_loaded_l1_efer
        .unwrap_or_else(|| unsafe { cpu::rdmsr(cpu::IA32_EFER) });
    if unsafe { vmx::vmptrld(run.carrier) } != VmxStatus::Success {
        stop_nested_exit(
            b"restoring carrier after L2 exit failed",
            run.direct,
            registers,
        );
    }

    if write_reflected_l1_state(run, l1_pat, l1_efer).is_none() {
        stop_nested_exit(b"reflecting L1 host state failed", run.direct, registers);
    }

    // ponytail: the fault-free real-mode probe leaves CR2 untouched. Save it
    // in the entry stub before allowing an L2 that can fault.
}

/// Stops after recovering L2's exit diagnostics only on an error path.
fn stop_nested_exit(message: &'static [u8], direct: VmcsPhys, registers: &GuestRegisters) -> ! {
    if unsafe { vmx::vmptrld(direct) } != VmxStatus::Success {
        stop_unexpected_exit(message, u64::MAX, u64::MAX, u64::MAX, u64::MAX, registers);
    }
    let reason = unsafe { vmx::vmread(vmcs::VM_EXIT_REASON) }.unwrap_or(u64::MAX);
    let qualification = unsafe { vmx::vmread(vmcs::EXIT_QUALIFICATION) }.unwrap_or(u64::MAX);
    let guest_rip = unsafe { vmx::vmread(vmcs::GUEST_RIP) }.unwrap_or(u64::MAX);
    let instruction_len = unsafe { vmx::vmread(vmcs::VM_EXIT_INSTRUCTION_LEN) }.unwrap_or(u64::MAX);
    stop_unexpected_exit(
        message,
        reason,
        qualification,
        guest_rip,
        instruction_len,
        registers,
    );
}

/// Writes the architectural 64-bit VM-exit host state as VMCS01 guest state.
fn write_reflected_l1_state(run: &NestedRun, l1_pat: u64, l1_efer: u64) -> Option<()> {
    let host = |field| direct_patch_value(&run.saved_direct, field);
    let es = host(VmcsField::HostEsSelector)?;
    let cs = host(VmcsField::HostCsSelector)?;
    let ss = host(VmcsField::HostSsSelector)?;
    let ds = host(VmcsField::HostDsSelector)?;
    let fs = host(VmcsField::HostFsSelector)?;
    let gs = host(VmcsField::HostGsSelector)?;

    for (field, value) in [
        (vmcs::GUEST_ES_SELECTOR, es),
        (vmcs::GUEST_CS_SELECTOR, cs),
        (vmcs::GUEST_SS_SELECTOR, ss),
        (vmcs::GUEST_DS_SELECTOR, ds),
        (vmcs::GUEST_FS_SELECTOR, fs),
        (vmcs::GUEST_GS_SELECTOR, gs),
        (vmcs::GUEST_TR_SELECTOR, host(VmcsField::HostTrSelector)?),
        (vmcs::GUEST_LDTR_SELECTOR, 0),
        (vmcs::GUEST_ES_LIMIT, u64::from(u32::MAX)),
        (vmcs::GUEST_CS_LIMIT, u64::from(u32::MAX)),
        (vmcs::GUEST_SS_LIMIT, u64::from(u32::MAX)),
        (vmcs::GUEST_DS_LIMIT, u64::from(u32::MAX)),
        (vmcs::GUEST_FS_LIMIT, u64::from(u32::MAX)),
        (vmcs::GUEST_GS_LIMIT, u64::from(u32::MAX)),
        (vmcs::GUEST_TR_LIMIT, 0x67),
        (vmcs::GUEST_LDTR_LIMIT, 0),
        (vmcs::GUEST_GDTR_LIMIT, 0xffff),
        (vmcs::GUEST_IDTR_LIMIT, 0xffff),
        (vmcs::GUEST_ES_AR_BYTES, 0xc093),
        (vmcs::GUEST_CS_AR_BYTES, 0xa09b),
        (vmcs::GUEST_SS_AR_BYTES, 0xc093),
        (vmcs::GUEST_DS_AR_BYTES, 0xc093),
        (vmcs::GUEST_FS_AR_BYTES, 0xc093),
        (vmcs::GUEST_GS_AR_BYTES, 0xc093),
        (vmcs::GUEST_TR_AR_BYTES, 0x008b),
        (
            vmcs::GUEST_LDTR_AR_BYTES,
            u64::from(vmcs::GUEST_SEGMENT_UNUSABLE),
        ),
        (vmcs::GUEST_CR0, host(VmcsField::HostCr0)?),
        (vmcs::GUEST_CR3, host(VmcsField::HostCr3)?),
        (vmcs::GUEST_CR4, host(VmcsField::HostCr4)?),
        (vmcs::CR4_READ_SHADOW, host(VmcsField::HostCr4)?),
        (vmcs::GUEST_ES_BASE, 0),
        (vmcs::GUEST_CS_BASE, 0),
        (vmcs::GUEST_SS_BASE, 0),
        (vmcs::GUEST_DS_BASE, 0),
        (vmcs::GUEST_FS_BASE, host(VmcsField::HostFsBase)?),
        (vmcs::GUEST_GS_BASE, host(VmcsField::HostGsBase)?),
        (vmcs::GUEST_LDTR_BASE, 0),
        (vmcs::GUEST_TR_BASE, host(VmcsField::HostTrBase)?),
        (vmcs::GUEST_GDTR_BASE, host(VmcsField::HostGdtrBase)?),
        (vmcs::GUEST_IDTR_BASE, host(VmcsField::HostIdtrBase)?),
        (vmcs::GUEST_DR7, 0x400),
        (vmcs::GUEST_RSP, host(VmcsField::HostRsp)?),
        (vmcs::GUEST_RIP, host(VmcsField::HostRip)?),
        (vmcs::GUEST_RFLAGS, 2),
        (vmcs::GUEST_PENDING_DBG_EXCEPTIONS, 0),
        // VM exit clears STI/MOV-SS blocking and retains L1's
        // blocking-by-NMI state, matching KVM's host-state load.
        (vmcs::GUEST_INTERRUPTIBILITY_INFO, run.l1_interruptibility),
        (vmcs::GUEST_ACTIVITY_STATE, 0),
        (vmcs::GUEST_IA32_DEBUGCTL, 0),
        (vmcs::GUEST_IA32_PAT, l1_pat),
        (vmcs::GUEST_IA32_EFER, l1_efer),
        (
            vmcs::GUEST_SYSENTER_CS,
            host(VmcsField::HostIa32SysenterCs)?,
        ),
        (
            vmcs::GUEST_SYSENTER_ESP,
            host(VmcsField::HostIa32SysenterEsp)?,
        ),
        (
            vmcs::GUEST_SYSENTER_EIP,
            host(VmcsField::HostIa32SysenterEip)?,
        ),
        (vmcs::VM_ENTRY_INTR_INFO_FIELD, 0),
    ] {
        if unsafe { vmx::vmwrite(field, value) } != VmxStatus::Success {
            return None;
        }
    }
    Some(())
}

/// Completes an immediate direct VM-entry failure and resumes VMCS01.
unsafe extern "sysv64" fn nested_vmentry_failed(registers: *const GuestRegisters, rflags: u64) {
    // SAFETY: `vmexit_entry` passes its live, uniquely owned saved-GPR frame.
    let registers = unsafe { &*registers };
    let Some(run) = NESTED_RUN.lock().take() else {
        stop_unexpected_exit(b"missing failed VMLAUNCH state", 20, 0, 0, 0, registers);
    };
    let result = if rflags & 1 != 0 {
        VmInstructionResult::VmfailInvalid
    } else if rflags & (1 << 6) != 0 {
        let Some(error) = (unsafe { vmx::vmread(vmcs::VM_INSTRUCTION_ERROR) })
            .ok()
            .and_then(|value| u32::try_from(value).ok())
        else {
            stop_unexpected_exit(
                b"reading failed VMLAUNCH error failed",
                run.outer_reason,
                run.outer_qualification,
                run.outer_rip,
                run.outer_instruction_len,
                registers,
            );
        };
        VmInstructionResult::VmfailValid(error)
    } else {
        stop_unexpected_exit(
            b"VMLAUNCH returned without failure flags",
            run.outer_reason,
            run.outer_qualification,
            run.outer_rip,
            run.outer_instruction_len,
            registers,
        );
    };
    if unsafe { vmx::vmptrld(run.carrier) } != VmxStatus::Success {
        stop_unexpected_exit(
            b"restoring carrier after VMLAUNCH failure failed",
            run.outer_reason,
            run.outer_qualification,
            run.outer_rip,
            run.outer_instruction_len,
            registers,
        );
    }
    complete_vmx_instruction(
        result,
        run.outer_reason,
        run.outer_qualification,
        run.outer_rip,
        run.outer_instruction_len,
        registers,
    );
}

/// Executes one L1 VMREAD or VMWRITE on its direct VMCS.
fn handle_l1_vmcs_access(
    write: bool,
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &mut GuestRegisters,
) {
    let state = *L1_VCPU_STATE.lock();
    let cr4_shadow = unsafe { vmx::vmread(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmx::vmread(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let Some(current) = state.current_vmcs() else {
        complete_vmx_instruction(
            VmInstructionResult::VmfailInvalid,
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return;
    };

    let Some(instruction_info) = (unsafe { vmx::vmread(vmcs::VMX_INSTRUCTION_INFO) })
        .ok()
        .and_then(|value| u32::try_from(value).ok())
    else {
        stop_unexpected_exit(
            b"reading VMCS-access instruction information failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    let (register1, memory_linear, field_register) = if let Some((register1, field_register)) =
        vmx::register_operand_indices(instruction_info)
    {
        (Some(register1), None, field_register)
    } else {
        let fs_base = unsafe { vmx::vmread(vmcs::GUEST_FS_BASE) }.unwrap_or(u64::MAX);
        let gs_base = unsafe { vmx::vmread(vmcs::GUEST_GS_BASE) }.unwrap_or(u64::MAX);
        let linear = vmx::memory_operand_address_64(
            instruction_info,
            qualification,
            |register| guest_gpr(registers, register),
            fs_base,
            gs_base,
        );
        let Some(linear) = linear else {
            // ponytail: trusted long-mode L1 supplies a valid mapped m64
            // operand; synthesize precise memory faults before relaxing it.
            stop_unexpected_exit(
                b"decoding memory-form VMCS access failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        };
        (None, Some(linear), ((instruction_info >> 28) & 0xf) as u8)
    };
    let field_value = guest_gpr(registers, field_register);
    let write_value = if write {
        if let Some(register1) = register1 {
            let Some(value) = guest_gpr(registers, register1) else {
                stop_unexpected_exit(
                    b"invalid VMWRITE value register",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            };
            Some(value)
        } else {
            let Some(value) = memory_linear.and_then(read_l1_linear_u64) else {
                // ponytail: trusted L1 memory operands fail-stop until this
                // path synthesizes the architectural #PF/#GP/#SS exception.
                stop_unexpected_exit(
                    b"reading memory-form VMWRITE source failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            };
            Some(value)
        }
    } else {
        None
    };
    let Some(field) = field_value.and_then(|value| u32::try_from(value).ok()) else {
        stop_unexpected_exit(
            b"invalid VMCS field operand",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    let shadowed = {
        let mut cached = DIRECT_PATCH_VALUES.lock();
        match cached.as_mut() {
            Some((address, _)) if *address != current.address() => stop_unexpected_exit(
                b"cached direct VMCS does not match VMCS access",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            ),
            Some((_, values)) => {
                if let Some(value) = write_value {
                    write_direct_patch_field(values, field, value).then_some(None)
                } else {
                    read_direct_patch_field(values, field).map(Some)
                }
            }
            None => None,
        }
    };
    let (status, read_value, hardware_error) = if let Some(read_value) = shadowed {
        (VmxStatus::Success, read_value, None)
    } else {
        let mut carrier_address = u64::MAX;
        if unsafe { vmx::vmptrst(&mut carrier_address) } != VmxStatus::Success {
            stop_unexpected_exit(
                b"saving VMCS-access carrier failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }
        let Some(carrier) = validate_l1_vmcs_address(carrier_address) else {
            stop_unexpected_exit(
                b"invalid VMCS-access carrier",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        };
        if carrier == current.address()
            || unsafe { vmx::vmptrld(current.address()) } != VmxStatus::Success
        {
            stop_unexpected_exit(
                b"selecting L1 VMCS for access failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }

        let (status, read_value) = if let Some(value) = write_value {
            (unsafe { vmx::vmwrite(field, value) }, None)
        } else {
            match unsafe { vmx::vmread(field) } {
                Ok(value) => (VmxStatus::Success, Some(value)),
                Err(status) => (status, None),
            }
        };
        let hardware_error = if status == VmxStatus::FailValid {
            (unsafe { vmx::vmread(vmcs::VM_INSTRUCTION_ERROR) })
                .ok()
                .and_then(|value| u32::try_from(value).ok())
        } else {
            None
        };
        if unsafe { vmx::vmptrld(carrier) } != VmxStatus::Success {
            stop_unexpected_exit(
                b"restoring VMCS-access carrier failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }
        (status, read_value, hardware_error)
    };
    if status == VmxStatus::Success
        && write_value.is_some()
        && matches!(
            field,
            vmcs::VM_ENTRY_MSR_LOAD_COUNT | vmcs::VM_EXIT_CONTROLS | vmcs::VM_ENTRY_CONTROLS
        )
    {
        *DIRECT_ENTRY_POLICY.lock() = None;
    }

    let result = match status {
        VmxStatus::Success => VmInstructionResult::Vmsucceed,
        VmxStatus::FailInvalid => VmInstructionResult::VmfailInvalid,
        VmxStatus::FailValid => {
            let Some(error) = hardware_error else {
                stop_unexpected_exit(
                    b"reading VMCS-access error failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            };
            VmInstructionResult::VmfailValid(error)
        }
    };
    if let Some(value) = read_value {
        if let Some(register1) = register1 {
            if !set_guest_gpr(registers, register1, value) {
                stop_unexpected_exit(
                    b"writing VMREAD destination failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            }
        } else if memory_linear
            .and_then(|linear| write_l1_linear_u64(linear, value))
            .is_none()
        {
            // ponytail: trusted L1 memory operands fail-stop until this path
            // synthesizes the architectural #PF/#GP/#SS exception.
            stop_unexpected_exit(
                b"writing memory-form VMREAD destination failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }
    }
    complete_vmx_instruction(
        result,
        reason,
        qualification,
        guest_rip,
        instruction_len,
        registers,
    );
}

/// Applies one trusted L1 EPT invalidation directly to hardware.
fn handle_l1_invept(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    let state = *L1_VCPU_STATE.lock();
    let cr4_shadow = unsafe { vmx::vmread(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmx::vmread(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    let instruction_info = unsafe { vmx::vmread(vmcs::VMX_INSTRUCTION_INFO) }
        .ok()
        .and_then(|value| u32::try_from(value).ok());
    let fs_base = unsafe { vmx::vmread(vmcs::GUEST_FS_BASE) }.unwrap_or(u64::MAX);
    let gs_base = unsafe { vmx::vmread(vmcs::GUEST_GS_BASE) }.unwrap_or(u64::MAX);
    let operands = instruction_info.and_then(|information| {
        let kind = guest_gpr(registers, ((information >> 28) & 0xf) as u8)?;
        let linear = vmx::memory_operand_address_64(
            information,
            qualification,
            |register| guest_gpr(registers, register),
            fs_base,
            gs_base,
        )?;
        Some((kind, linear))
    });
    let Some((kind, linear)) = operands else {
        stop_unexpected_exit(
            b"decoding INVEPT operands failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    let descriptor = read_l1_linear_u64(linear).and_then(|ept_pointer| {
        read_l1_linear_u64(linear.checked_add(8)?).map(|reserved| vmx::InveptDescriptor {
            ept_pointer,
            reserved,
        })
    });
    let Some(descriptor) = descriptor else {
        // ponytail: trusted long-mode L1 supplies a valid m128 operand. Add
        // precise #PF/#GP/#SS synthesis before accepting untrusted L1 input.
        stop_unexpected_exit(
            b"reading INVEPT descriptor failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    let status = unsafe { vmx::invept(kind, &descriptor) };
    let result = match status {
        VmxStatus::Success => VmInstructionResult::Vmsucceed,
        VmxStatus::FailInvalid => VmInstructionResult::VmfailInvalid,
        VmxStatus::FailValid => {
            let Some(error) = (unsafe { vmx::vmread(vmcs::VM_INSTRUCTION_ERROR) })
                .ok()
                .and_then(|value| u32::try_from(value).ok())
            else {
                stop_unexpected_exit(
                    b"reading INVEPT error failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            };
            // ponytail: type-2 with a zero descriptor is the measured path;
            // switch to L1's VMCS before supporting observable failure errors.
            l1_vmx_failure(&state, error)
        }
    };
    complete_vmx_instruction(
        result,
        reason,
        qualification,
        guest_rip,
        instruction_len,
        registers,
    );
}

/// Applies one trusted L1 VPID invalidation directly to hardware.
fn handle_l1_invvpid(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    let state = *L1_VCPU_STATE.lock();
    let cr4_shadow = unsafe { vmx::vmread(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmx::vmread(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    let instruction_info = unsafe { vmx::vmread(vmcs::VMX_INSTRUCTION_INFO) }
        .ok()
        .and_then(|value| u32::try_from(value).ok());
    let fs_base = unsafe { vmx::vmread(vmcs::GUEST_FS_BASE) }.unwrap_or(u64::MAX);
    let gs_base = unsafe { vmx::vmread(vmcs::GUEST_GS_BASE) }.unwrap_or(u64::MAX);
    let operands = instruction_info.and_then(|information| {
        let kind = guest_gpr(registers, ((information >> 28) & 0xf) as u8)?;
        let linear = vmx::memory_operand_address_64(
            information,
            qualification,
            |register| guest_gpr(registers, register),
            fs_base,
            gs_base,
        )?;
        Some((kind, linear))
    });
    let Some((kind, linear)) = operands else {
        stop_unexpected_exit(
            b"decoding INVVPID operands failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    if kind > 3 {
        complete_vmx_instruction(
            l1_vmx_failure(&state, VMXERR_INVALID_INVEPT_INVVPID_OPERAND),
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return;
    }
    let descriptor = read_l1_linear_u64(linear).and_then(|first| {
        read_l1_linear_u64(linear.checked_add(8)?)
            .map(|address| vmx::InvvpidDescriptor::from_words(first, address))
    });
    let Some(descriptor) = descriptor else {
        // ponytail: trusted long-mode L1 supplies a valid m128 operand. Add
        // precise #PF/#GP/#SS synthesis before accepting untrusted L1 input.
        stop_unexpected_exit(
            b"reading INVVPID descriptor failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    // ponytail: VPID tags are direct and globally shared with this trusted
    // one-vCPU L1; add per-pCPU VPID ownership before monitor SMP.
    let status = unsafe { vmx::invvpid(kind, &descriptor) };
    let result = match status {
        VmxStatus::Success => VmInstructionResult::Vmsucceed,
        VmxStatus::FailInvalid => VmInstructionResult::VmfailInvalid,
        VmxStatus::FailValid => {
            let Some(error) = (unsafe { vmx::vmread(vmcs::VM_INSTRUCTION_ERROR) })
                .ok()
                .and_then(|value| u32::try_from(value).ok())
            else {
                stop_unexpected_exit(
                    b"reading INVVPID error failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            };
            l1_vmx_failure(&state, error)
        }
    };
    complete_vmx_instruction(
        result,
        reason,
        qualification,
        guest_rip,
        instruction_len,
        registers,
    );
}

/// Converts a VMfail error according to whether L1 has a current VMCS.
fn l1_vmx_failure(state: &VcpuState, error: u32) -> VmInstructionResult {
    if state.current_vmcs().is_some() {
        // ponytail: write this error into L1's direct VMCS when VMPTRLD makes
        // one current; today's observed VMCLEAR sequence has no current VMCS.
        VmInstructionResult::VmfailValid(error)
    } else {
        VmInstructionResult::VmfailInvalid
    }
}

/// Logs one cold-path VMXON decode value.
fn log_l1_vmxon(name: &[u8], value: u64) {
    log_l1_vmx(b"VMXON", name, value);
}

/// Logs one cold-path nested-VMX decode value without runtime formatting.
fn log_l1_vmx(instruction: &[u8], name: &[u8], value: u64) {
    let mut serial = SerialPort;
    serial.init();
    serial.write_bytes(b"thin-hv: L1 ");
    serial.write_bytes(instruction);
    serial.write_byte(b' ');
    serial.write_bytes(name);
    serial.write_byte(b'=');
    serial.write_hex(value);
    serial.write_byte(b'\r');
    serial.write_byte(b'\n');
}

/// Decodes and reads one nested-VMX m64 pointer operand.
fn read_l1_vmx_pointer(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) -> u64 {
    let instruction_info = unsafe { vmx::vmread(vmcs::VMX_INSTRUCTION_INFO) }
        .ok()
        .and_then(|value| u32::try_from(value).ok());
    let fs_base = unsafe { vmx::vmread(vmcs::GUEST_FS_BASE) }.unwrap_or(u64::MAX);
    let gs_base = unsafe { vmx::vmread(vmcs::GUEST_GS_BASE) }.unwrap_or(u64::MAX);
    let linear = instruction_info.and_then(|information| {
        vmx::memory_operand_address_64(
            information,
            qualification,
            |register| guest_gpr(registers, register),
            fs_base,
            gs_base,
        )
    });
    let Some(linear) = linear else {
        stop_unexpected_exit(
            b"decoding VMX pointer failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    let Some(pointer) = read_l1_linear_u64(linear) else {
        // ponytail: trusted long-mode L1 uses a valid operand. Add precise
        // #PF/#GP/#SS synthesis before accepting untrusted or 32-bit L1s.
        stop_unexpected_exit(
            b"reading VMX pointer failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    pointer
}

/// Checks the fixed-bit contract used by VMXON.
fn vmx_control_registers_valid(cr0: u64, cr4: u64) -> bool {
    let cr0_fixed0 = unsafe { cpu::rdmsr(vmx::IA32_VMX_CR0_FIXED0) };
    let cr0_fixed1 = unsafe { cpu::rdmsr(vmx::IA32_VMX_CR0_FIXED1) };
    let cr4_fixed0 = unsafe { cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED0) };
    let cr4_fixed1 = unsafe { cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED1) };
    cr0 & cr0_fixed0 == cr0_fixed0
        && cr0 & !cr0_fixed1 == 0
        && cr4 & cr4_fixed0 == cr4_fixed0
        && cr4 & !cr4_fixed1 == 0
}

/// Validates one direct VMCS physical operand without inspecting its header.
fn validate_l1_vmcs_address(address: u64) -> Option<VmcsPhys> {
    let bits = max_physical_address_bits()?;
    let end = address.checked_add(PAGE_SIZE)?;
    if end > 1_u64 << bits || end > IDENTITY_MAP_LIMIT {
        return None;
    }
    VmcsPhys::new(address)
}

/// Reads an m64 operand through L1's current long-mode page tables.
fn read_l1_linear_u64(linear: u64) -> Option<u64> {
    let cr3 = unsafe { vmx::vmread(vmcs::GUEST_CR3) }.ok()?;
    let cr4 = unsafe { vmx::vmread(vmcs::GUEST_CR4) }.ok()?;
    let max_physical_bits = max_physical_address_bits()?;
    let mut bytes = [0_u8; 8];
    // ponytail: eight walks make a cold VMX operand cross-page safe; cache
    // translated pages only if instruction emulation becomes measurable.
    for (index, byte) in bytes.iter_mut().enumerate() {
        let address = linear.checked_add(index as u64)?;
        let physical =
            paging::translate_long_mode(address, cr3, cr4, max_physical_bits, |entry| {
                read_l1_physical_u64(entry)
            })?;
        if physical >= IDENTITY_MAP_LIMIT {
            return None;
        }
        // SAFETY: the trusted L1 page walk resolved this byte inside the
        // identity-mapped eight-gibibyte smoke address space.
        *byte = unsafe { ptr::read_volatile(physical as *const u8) };
    }
    Some(u64::from_le_bytes(bytes))
}

/// Writes an m64 operand through L1's current long-mode page tables.
fn write_l1_linear_u64(linear: u64, value: u64) -> Option<()> {
    let cr3 = unsafe { vmx::vmread(vmcs::GUEST_CR3) }.ok()?;
    let cr4 = unsafe { vmx::vmread(vmcs::GUEST_CR4) }.ok()?;
    let max_physical_bits = max_physical_address_bits()?;
    // ponytail: mirror the read path's eight walks so an m64 crossing a page
    // boundary remains valid without adding a translation cache to the TCB.
    for (index, byte) in value.to_le_bytes().iter().enumerate() {
        let address = linear.checked_add(index as u64)?;
        let physical =
            paging::translate_long_mode(address, cr3, cr4, max_physical_bits, |entry| {
                read_l1_physical_u64(entry)
            })?;
        if physical >= IDENTITY_MAP_LIMIT {
            return None;
        }
        // SAFETY: the trusted L1 page walk resolved this byte inside the
        // identity-mapped eight-gibibyte smoke address space.
        unsafe { ptr::write_volatile(physical as *mut u8, *byte) };
    }
    Some(())
}

/// Reads one aligned paging entry from identity-mapped L1 physical memory.
fn read_l1_physical_u64(physical: u64) -> Option<u64> {
    if physical.checked_add(7)? >= IDENTITY_MAP_LIMIT || !physical.is_multiple_of(8) {
        return None;
    }
    // SAFETY: trusted L1 supplies paging structures in identity-mapped RAM.
    Some(unsafe { ptr::read_volatile(physical as *const u64) })
}

/// Returns the physical-address width exposed unchanged to L1 by CPUID.
fn max_physical_address_bits() -> Option<u8> {
    if cpu::cpuid(0x8000_0000, 0).eax < 0x8000_0008 {
        return Some(36);
    }
    let bits = u8::try_from(cpu::cpuid(0x8000_0008, 0).eax & 0xff).ok()?;
    (12..=52).contains(&bits).then_some(bits)
}

/// Validates the VMXON GPA and its direct-hardware revision identifier.
fn validate_l1_vmxon_region(address: u64) -> Option<VmxonPhys> {
    let bits = max_physical_address_bits()?;
    let physical_limit = 1_u64 << bits;
    if address.checked_add(PAGE_SIZE)? > physical_limit
        || address.checked_add(PAGE_SIZE)? > IDENTITY_MAP_LIMIT
    {
        return None;
    }
    let region = VmxonPhys::new(address)?;
    // SAFETY: the checks above cover the four-byte header in the trusted
    // identity-mapped VMXON page.
    let revision = unsafe { ptr::read_volatile(address as *const u32) };
    let expected = vmx::VmxBasic::from_msr(unsafe { cpu::rdmsr(vmx::IA32_VMX_BASIC) }).revision_id;
    (revision == expected).then_some(region)
}

/// Applies VMX status flags and advances past an emulated VMX instruction.
fn complete_vmx_instruction(
    result: VmInstructionResult,
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    let Some(rflags) = (unsafe { vmx::vmread(vmcs::GUEST_RFLAGS) }).ok() else {
        stop_unexpected_exit(
            b"VMREAD(GUEST_RFLAGS) failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    if unsafe { vmx::vmwrite(vmcs::GUEST_RFLAGS, result.apply_to_rflags(rflags)) }
        != VmxStatus::Success
    {
        stop_unexpected_exit(
            b"VMWRITE(GUEST_RFLAGS) failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    advance_guest_rip(reason, qualification, guest_rip, instruction_len, registers);
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

/// Writes a GPR saved by the VM-exit entry, including VMCS-backed RSP.
fn set_guest_gpr(registers: &mut GuestRegisters, index: u8, value: u64) -> bool {
    match index {
        0 => registers.rax = value,
        1 => registers.rcx = value,
        2 => registers.rdx = value,
        3 => registers.rbx = value,
        4 => return unsafe { vmx::vmwrite(vmcs::GUEST_RSP, value) } == VmxStatus::Success,
        5 => registers.rbp = value,
        6 => registers.rsi = value,
        7 => registers.rdi = value,
        8 => registers.r8 = value,
        9 => registers.r9 = value,
        10 => registers.r10 = value,
        11 => registers.r11 = value,
        12 => registers.r12 = value,
        13 => registers.r13 = value,
        14 => registers.r14 = value,
        15 => registers.r15 = value,
        _ => return false,
    }
    true
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
                b"injecting guest #GP failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }
    }
}

/// Injects the #UD that VMXON raises when virtual CR4.VMXE is clear.
fn inject_invalid_opcode(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    let status = unsafe { vmx::vmwrite(vmcs::VM_ENTRY_INTR_INFO_FIELD, INJECT_INVALID_OPCODE) };
    if status != VmxStatus::Success {
        stop_unexpected_exit(
            b"injecting guest #UD failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
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
            b"guest RIP overflow",
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
            b"VMWRITE(GUEST_RIP) failed",
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
    serial.write_bytes(b"thin-hv: VMEXIT");
    write_raw_field(&mut serial, b"cpu", u64::from(cpu_id));
    write_raw_field(&mut serial, b"level", 1);
    write_raw_field(&mut serial, b"reason", reason);
    write_raw_field(&mut serial, b"qualification", qualification);
    write_raw_field(&mut serial, b"guest_rip", guest_rip);
    write_raw_field(&mut serial, b"instruction_len", instruction_len);
    write_raw_field(&mut serial, b"cpuid_exits", cpuid_exits as u64);
    write_raw_newline(&mut serial);
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
        serial.write_bytes(b"thin-hv: vmx guest PASS");
        write_raw_field(&mut serial, b"start_image_status", guest_status as u64);
    } else {
        serial.write_bytes(b"thin-hv: vmx guest FAIL");
        write_raw_field(&mut serial, b"marker", marker);
        write_raw_field(&mut serial, b"start_image_status", guest_status as u64);
        serial.write_bytes(b" vmxoff=");
        write_raw_vmx_status(&mut serial, vmxoff);
    }
    write_raw_newline(&mut serial);
    loop {
        core::hint::spin_loop();
    }
}

/// Reports an exit that this smoke monitor cannot reflect or handle.
fn stop_unexpected_exit(
    message: &[u8],
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
    serial.write_bytes(b"thin-hv: vmx guest FAIL: ");
    serial.write_bytes(message);
    write_raw_field(&mut serial, b"reason", reason);
    write_raw_field(&mut serial, b"qualification", qualification);
    write_raw_field(&mut serial, b"instruction_info", instruction_info);
    write_raw_field(&mut serial, b"guest_rip", guest_rip);
    write_raw_field(&mut serial, b"instruction_len", instruction_len);
    write_raw_field(&mut serial, b"vm_instruction_error", vm_error);
    write_raw_field(&mut serial, b"cpuid_exits", cpuid_exits as u64);
    write_raw_field(&mut serial, b"rax", registers.rax);
    write_raw_field(&mut serial, b"rbx", registers.rbx);
    write_raw_field(&mut serial, b"rcx", registers.rcx);
    write_raw_field(&mut serial, b"rdx", registers.rdx);
    write_raw_newline(&mut serial);
    let vmxoff = leave_vmx();
    serial.write_bytes(b"thin-hv: VMXOFF status=");
    write_raw_vmx_status(&mut serial, vmxoff);
    write_raw_newline(&mut serial);
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
    serial.write_bytes(b"thin-hv: VMRESUME FAIL status=");
    write_raw_vmx_status(&mut serial, status);
    write_raw_field(&mut serial, b"rflags", rflags);
    write_raw_field(&mut serial, b"vm_instruction_error", vm_error);
    write_raw_field(&mut serial, b"reason", reason);
    write_raw_field(&mut serial, b"qualification", qualification);
    write_raw_field(&mut serial, b"guest_rip", guest_rip);
    write_raw_field(&mut serial, b"instruction_len", instruction_len);
    write_raw_field(&mut serial, b"cpuid_exits", cpuid_exits as u64);
    write_raw_field(&mut serial, b"rax", registers.rax);
    write_raw_field(&mut serial, b"rbx", registers.rbx);
    write_raw_field(&mut serial, b"rcx", registers.rcx);
    write_raw_field(&mut serial, b"rdx", registers.rdx);
    write_raw_newline(&mut serial);
    let vmxoff = leave_vmx();
    serial.write_bytes(b"thin-hv: VMXOFF status=");
    write_raw_vmx_status(&mut serial, vmxoff);
    write_raw_newline(&mut serial);
    loop {
        core::hint::spin_loop();
    }
}

/// Writes one name/value pair without EFI-relocated formatting metadata.
fn write_raw_field(serial: &mut SerialPort, name: &[u8], value: u64) {
    serial.write_byte(b' ');
    serial.write_bytes(name);
    serial.write_byte(b'=');
    serial.write_hex(value);
}

/// Writes one VMX instruction status without `core::fmt`.
fn write_raw_vmx_status(serial: &mut SerialPort, status: VmxStatus) {
    serial.write_bytes(match status {
        VmxStatus::Success => b"Success",
        VmxStatus::FailInvalid => b"FailInvalid",
        VmxStatus::FailValid => b"FailValid",
    });
}

/// Ends one raw serial record.
fn write_raw_newline(serial: &mut SerialPort) {
    serial.write_byte(b'\r');
    serial.write_byte(b'\n');
}

/// Leaves VMX operation and restores the pre-smoke control registers on success.
fn leave_vmx() -> VmxStatus {
    let status = unsafe { vmx::vmxoff() };
    if status == VmxStatus::Success {
        restore_control_registers();
    }
    status
}

#[cfg(test)]
mod tests {
    use super::DIRECT_VMCS_PATCH_MANIFEST;
    use super::INJECT_EXTERNAL_INTERRUPT;
    use super::VmcsField;
    use super::acknowledged_external_interrupt;
    use super::read_direct_patch_field;
    use super::write_direct_patch_field;

    #[test]
    fn acknowledged_external_interrupt_requires_a_plain_external_vector() {
        assert_eq!(acknowledged_external_interrupt(0), Ok(None));
        assert_eq!(acknowledged_external_interrupt(0x7fff_ffff), Ok(None));
        assert_eq!(
            acknowledged_external_interrupt(INJECT_EXTERNAL_INTERRUPT | 0xff),
            Ok(Some(INJECT_EXTERNAL_INTERRUPT | 0xff))
        );
        assert_eq!(
            acknowledged_external_interrupt(INJECT_EXTERNAL_INTERRUPT | (1 << 8) | 0x20),
            Err(())
        );
    }

    #[test]
    fn retained_direct_fields_preserve_vmcs_width_semantics() {
        let mut values = [0; DIRECT_VMCS_PATCH_MANIFEST.len()];
        let selector = VmcsField::HostEsSelector as u32;
        let count = VmcsField::VmExitMsrStoreCount as u32;
        let pat = VmcsField::HostIa32Pat as u32;
        let cr0 = VmcsField::HostCr0 as u32;

        assert!(write_direct_patch_field(&mut values, selector, u64::MAX));
        assert_eq!(read_direct_patch_field(&values, selector), Some(0xffff));
        assert_eq!(read_direct_patch_field(&values, selector | 1), None);

        assert!(write_direct_patch_field(&mut values, count, u64::MAX));
        assert_eq!(
            read_direct_patch_field(&values, count),
            Some(u64::from(u32::MAX))
        );

        assert!(write_direct_patch_field(
            &mut values,
            pat,
            0x1122_3344_5566_7788
        ));
        assert_eq!(read_direct_patch_field(&values, pat | 1), Some(0x1122_3344));
        assert!(write_direct_patch_field(&mut values, pat | 1, u64::MAX));
        assert_eq!(
            read_direct_patch_field(&values, pat),
            Some(0xffff_ffff_5566_7788)
        );

        assert!(write_direct_patch_field(&mut values, cr0, u64::MAX));
        assert_eq!(read_direct_patch_field(&values, cr0), Some(u64::MAX));
    }
}
