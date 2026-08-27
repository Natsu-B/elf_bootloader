//! One-vCPU VMXON/VMLAUNCH/VMCALL validation.

use crate::SerialPort;
use core::fmt;
use core::fmt::Write;
use core::ptr;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
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
const MONITOR_PAGES: usize = 13;
/// First page used as the host stack.
const HOST_STACK_PAGE: u64 = 5;
/// First page used as the guest stack.
const GUEST_STACK_PAGE: u64 = 9;
/// One architectural page.
const PAGE_SIZE: u64 = 4096;
/// VMCALL basic exit reason.
const EXIT_REASON_VMCALL: u64 = 18;
/// Marker written in non-root mode before VMCALL.
const GUEST_MARKER: u64 = 0x7468_696e_6876_4d58;

static GUEST_RAN: AtomicU64 = AtomicU64::new(0);
static ORIGINAL_CR0: AtomicU64 = AtomicU64::new(0);
static ORIGINAL_CR4: AtomicU64 = AtomicU64::new(0);

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
    /// The smoke-only 1 GiB EPT cannot cover the allocated block or code.
    OutsideIdentityMap(u64),
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
        }
    }
}

/// Runs the VMX smoke test. Success transfers to `vmexit_entry` and does not return.
pub(crate) fn run(system_table: *mut efi::SystemTable) -> Result<(), Error> {
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
        if address >= 1 << 30 {
            return Err(Error::OutsideIdentityMap(address));
        }
    }

    // SAFETY: AllocatePages returned an exclusive, aligned block of this exact size.
    unsafe { ptr::write_bytes(block as *mut u8, 0, MONITOR_PAGES * PAGE_SIZE as usize) };
    // SAFETY: the first words belong to exclusive VMXON and VMCS pages.
    unsafe {
        ptr::write_volatile(block as *mut u32, basic.revision_id);
        ptr::write_volatile((block + PAGE_SIZE) as *mut u32, basic.revision_id);
    }

    let pml4_phys = EptPhys::new(block + 2 * PAGE_SIZE).unwrap();
    let pdpt_phys = EptPhys::new(block + 3 * PAGE_SIZE).unwrap();
    let pd_phys = EptPhys::new(block + 4 * PAGE_SIZE).unwrap();
    // SAFETY: these three exclusive pages are aligned, zeroed, and exactly EptPage-sized.
    let ept_pointer = unsafe {
        ept::build_identity_1g(
            &mut *((block + 2 * PAGE_SIZE) as *mut ept::EptPage),
            pml4_phys,
            &mut *((block + 3 * PAGE_SIZE) as *mut ept::EptPage),
            pdpt_phys,
            &mut *((block + 4 * PAGE_SIZE) as *mut ept::EptPage),
            pd_phys,
        )
    };

    let original_cr0 = cpu::read_cr0();
    let original_cr4 = cpu::read_cr4();
    let fixed_cr0 = (original_cr0 | unsafe { cpu::rdmsr(vmx::IA32_VMX_CR0_FIXED0) })
        & unsafe { cpu::rdmsr(vmx::IA32_VMX_CR0_FIXED1) };
    let fixed_cr4 = (original_cr4 | (1 << 13) | unsafe { cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED0) })
        & unsafe { cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED1) };
    ORIGINAL_CR0.store(original_cr0, Ordering::Relaxed);
    ORIGINAL_CR4.store(original_cr4, Ordering::Relaxed);
    // SAFETY: values were normalized with the CPU's VMX fixed-bit MSRs.
    unsafe {
        cpu::write_cr0(fixed_cr0);
        cpu::write_cr4(fixed_cr4);
    }

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
        fixed_cr0,
        fixed_cr4,
        fixed_cr4,
        block + (HOST_STACK_PAGE + 4) * PAGE_SIZE - 8,
        block + (GUEST_STACK_PAGE + 4) * PAGE_SIZE - 8,
        basic.true_controls,
    );

    // This is reached only when VM entry failed.
    let _ = unsafe { vmx::vmxoff() };
    restore_control_registers();
    result
}

/// Configures the current VMCS and launches the non-root marker.
#[allow(clippy::too_many_arguments)]
fn configure_and_launch(
    vmcs_page: VmcsPhys,
    ept_pointer: u64,
    host_cr0: u64,
    guest_cr4: u64,
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
    let primary = vmx::adjust_controls(vmcs::PRIMARY_EXEC_ACTIVATE_SECONDARY_CONTROLS, unsafe {
        cpu::rdmsr(primary_msr)
    });
    let secondary = vmx::adjust_controls(vmcs::SECONDARY_EXEC_ENABLE_EPT, unsafe {
        cpu::rdmsr(vmx::IA32_VMX_PROCBASED_CTLS2)
    });
    let exit = vmx::adjust_controls(vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE, unsafe {
        cpu::rdmsr(exit_msr)
    });
    let entry = vmx::adjust_controls(vmcs::VM_ENTRY_IA32E_MODE, unsafe { cpu::rdmsr(entry_msr) });
    if primary & vmcs::PRIMARY_EXEC_ACTIVATE_SECONDARY_CONTROLS == 0
        || secondary & vmcs::SECONDARY_EXEC_ENABLE_EPT == 0
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
        (vmcs::CR4_GUEST_HOST_MASK, 0),
        (vmcs::CR0_READ_SHADOW, host_cr0),
        (vmcs::CR4_READ_SHADOW, guest_cr4),
        (vmcs::EPT_POINTER, ept_pointer),
    ] {
        write_vmcs(field, value)?;
    }

    write_guest_state(host_cr0, guest_cr4, guest_rsp)?;
    write_host_state(host_cr0, host_cr4, host_rsp)?;
    log_guest_state();

    GUEST_RAN.store(0, Ordering::Release);
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
        cpu::write_cr4(ORIGINAL_CR4.load(Ordering::Relaxed));
        cpu::write_cr0(ORIGINAL_CR0.load(Ordering::Relaxed));
    }
}

/// First non-root instruction stream.
extern "C" fn guest_entry() -> ! {
    GUEST_RAN.store(GUEST_MARKER, Ordering::Release);
    // SAFETY: this guest runs specifically under the VMCALL smoke handler.
    unsafe { vmx::vmcall() };
    loop {
        core::hint::spin_loop();
    }
}

/// Hardware VM-exit target for the smoke VMCS.
extern "C" fn vmexit_entry() -> ! {
    let reason = unsafe { vmx::vmread(vmcs::VM_EXIT_REASON) }.unwrap_or(u64::MAX);
    let qualification = unsafe { vmx::vmread(vmcs::EXIT_QUALIFICATION) }.unwrap_or(u64::MAX);
    let guest_rip = unsafe { vmx::vmread(vmcs::GUEST_RIP) }.unwrap_or(u64::MAX);
    let instruction_len = unsafe { vmx::vmread(vmcs::VM_EXIT_INSTRUCTION_LEN) }.unwrap_or(u64::MAX);
    let cpu_id = (cpu::cpuid(1, 0).ebx >> 24) & 0xff;
    let marker = GUEST_RAN.load(Ordering::Acquire);
    let mut serial = SerialPort;
    serial.init();
    let _ = writeln!(
        serial,
        "thin-hv: VMEXIT cpu={cpu_id} level=1 reason={reason:#x} qualification={qualification:#x} guest_rip={guest_rip:#x} instruction_len={instruction_len}"
    );

    let vmxoff = unsafe { vmx::vmxoff() };
    restore_control_registers();
    if reason & 0xffff == EXIT_REASON_VMCALL
        && reason & (1 << 31) == 0
        && marker == GUEST_MARKER
        && vmxoff == VmxStatus::Success
    {
        let _ = writeln!(serial, "thin-hv: vmx guest PASS");
    } else {
        let _ = writeln!(
            serial,
            "thin-hv: vmx guest FAIL marker={marker:#x} vmxoff={vmxoff:?}"
        );
    }
    loop {
        core::hint::spin_loop();
    }
}
