//! One-vCPU VMXON/VMLAUNCH/VMCALL validation.

// The exit stub protects x87/SSE, not AVX/AVX-512/AMX. A linked-image ISA check
// in xtask also covers dependencies and explicit per-function target features.
#[cfg(target_feature = "avx")]
compile_error!("Direct L0 requires a baseline, non-AVX compilation target");

use crate::SerialPort;
use crate::chainload;
use crate::chainload::device_path_utilities_protocol;
#[cfg(not(feature = "physical-direct-vmx"))]
use crate::chainload::load_image_from_other_filesystem;
use crate::chainload::load_image_on_device;
use crate::chainload::loaded_image_protocol;
use crate::platform_acpi;
use crate::platform_resources;
use crate::platform_resources::MmioMap;
use crate::platform_snapshot;
use crate::resident_image;
#[cfg(not(feature = "physical-direct-vmx"))]
use crate::runtime_variables;
use core::ffi::c_void;
use core::fmt;
use core::fmt::Write;
use core::ptr;
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use core::sync::atomic::compiler_fence;
use mutex::SpinLock;
use nested_vmx::ControlProvenance;
use nested_vmx::DIRECT_VMCS_PATCH_MANIFEST;
use nested_vmx::EXIT_ACKNOWLEDGE_INTERRUPT;
use nested_vmx::PIN_EXTERNAL_INTERRUPT_EXITING;
use nested_vmx::PRIMARY_INTERRUPT_WINDOW_EXITING;
use nested_vmx::PatchKind;
use nested_vmx::VMXERR_INVALID_INVEPT_INVVPID_OPERAND;
use nested_vmx::VMXERR_UNSUPPORTED_VMCS_COMPONENT;
use nested_vmx::VMXERR_VMCLEAR_INVALID_ADDRESS;
use nested_vmx::VMXERR_VMCLEAR_VMXON_POINTER;
use nested_vmx::VMXERR_VMPTRLD_INCORRECT_REVISION;
use nested_vmx::VMXERR_VMPTRLD_INVALID_ADDRESS;
use nested_vmx::VMXERR_VMPTRLD_VMXON_POINTER;
use nested_vmx::VMXERR_VMWRITE_READ_ONLY_COMPONENT;
use nested_vmx::VMXERR_VMXON_IN_ROOT;
use nested_vmx::VcpuState;
use nested_vmx::VmEntryInstruction;
use nested_vmx::VmInstructionResult;
use nested_vmx::VmcsField;
use nested_vmx::exit_snapshot::ExitSnapshot;
use nested_vmx::host_validation;
use nested_vmx::msr_list::Entry as MsrEntry;
use nested_vmx::msr_list::ExitStoreSource;
use nested_vmx::msr_list::List as MsrList;
use nested_vmx::msr_list::Operation as MsrOperation;
use nested_vmx::msr_list::PatEfer;
use nested_vmx::msr_list::exit_store_source;
use nested_vmx::restrict_vmx_capability;
use nested_vmx::vmcs_revision_is_supported;
use nested_vmx::vpid::Namespace as VpidNamespace;
use r_efi::efi;
use uefi_variable_overlay::ProfileId;
use x86_64_hal::addr::EptPhys;
use x86_64_hal::addr::HostPhys;
use x86_64_hal::addr::VmcsPhys;
use x86_64_hal::addr::VmxonPhys;
use x86_64_hal::cpu;
use x86_64_hal::ept;
use x86_64_hal::host_state;
use x86_64_hal::host_state::HostEnvironment;
use x86_64_hal::host_state::HostStack;
use x86_64_hal::paging;
use x86_64_hal::paging::DataAccess;
use x86_64_hal::paging::DataFault;
use x86_64_hal::platform_memory;
use x86_64_hal::platform_memory::FirmwareDescriptor;
use x86_64_hal::platform_memory::FirmwareMap;
use x86_64_hal::platform_memory::PageCapabilities;
use x86_64_hal::platform_memory::PhysicalRange;
use x86_64_hal::platform_memory::PhysicalWidth;
use x86_64_hal::platform_memory::PlatformMap;
use x86_64_hal::vmcs;
use x86_64_hal::vmx;
use x86_64_hal::vmx::VmxStatus;
use x86_64_hal::xstate;
use x86_64_hal::xstate::XsetbvFault;

/// Pages allocated as one reserved monitor block.
const MONITOR_PAGES: usize = CPU_STATE_FIRST_PAGE as usize + CPU_STATE_PAGES;
/// Shared boot guard pages are outside the private payload, not VMXON storage.
const MONITOR_ALLOCATION_PAGES: usize = MONITOR_PAGES + 2;
/// Exclusive, bounded platform EPT arena; no incomplete EPTP is published.
const EPT_FIRST_PAGE: u64 = 2;
const EPT_PAGES: usize = 256;
/// L1 MSR bitmap, including conservative VMX capability interception.
const MSR_BITMAP_PAGE: u64 = EPT_FIRST_PAGE + EPT_PAGES as u64;
/// First page used as the host stack.
const HOST_STACK_PAGE: u64 = MSR_BITMAP_PAGE + 1;
/// First page used as the guest stack.
const GUEST_STACK_PAGE: u64 = HOST_STACK_PAGE + 4;
/// Linux's EFI path uses more than the 128 KiB stack needed by small payloads.
const GUEST_STACK_PAGES: u64 = 64;
/// Private platform RAM tables and one CPU-owned temporary MMIO window.
const HOST_TABLE_FIRST_PAGE: u64 = GUEST_STACK_PAGE + GUEST_STACK_PAGES;
const HOST_TABLE_PAGES: usize = 256;
/// Private GDT/TSS, IDT and four independent IST stacks, after the host tables.
const HOST_ENVIRONMENT_FIRST_PAGE: u64 = HOST_TABLE_FIRST_PAGE + HOST_TABLE_PAGES as u64;
/// Inactive, deliberately invalid-revision page used only to record VMX error 11.
const ERROR_REVISION_PAGE: u64 =
    HOST_ENVIRONMENT_FIRST_PAGE + host_state::HOST_ENVIRONMENT_PAGES as u64;
/// CPU-owned MSR mirrors and VPID lease, separate from descriptors and stack.
const CPU_STATE_FIRST_PAGE: u64 = ERROR_REVISION_PAGE + 1;
const CPU_STATE_PAGES: usize = core::mem::size_of::<CpuMonitor>().div_ceil(4096);
/// One architectural page.
const PAGE_SIZE: u64 = 4096;
/// Low-canonical identity limit of the private four-level HOST_CR3. Actual
/// coverage still requires validated RAM; this is never a blanket mapping.
const IDENTITY_MAP_LIMIT: u64 = 1 << 47;
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
/// CET requires additional host shadow-stack state which is not yet provisioned.
const CR4_CET: u64 = 1 << 23;
/// Marker written in non-root mode before VMCALL.
const GUEST_MARKER: u64 = 0x7468_696e_6876_4d58;
/// Payload staged by `run-uefi-smoke.sh`.
#[cfg(not(feature = "physical-direct-vmx"))]
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
#[cfg(not(feature = "physical-direct-vmx"))]
const WINDOWS_BOOT_IMAGE_PATH: [efi::Char16; 33] =
    ascii_uefi_path(b"\\EFI\\Microsoft\\Boot\\bootmgfw.efi\0");
/// Stable profile selected when the staged Linux/test payload is present.
#[cfg(not(feature = "physical-direct-vmx"))]
const LINUX_PROFILE: ProfileId = ProfileId(2);
/// Stable profile selected when chainloading the installed Windows ESP.
#[cfg(not(feature = "physical-direct-vmx"))]
const WINDOWS_PROFILE: ProfileId = ProfileId(1);

#[cfg(not(feature = "physical-direct-vmx"))]
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

/// Firmware controls retained only on the initial-entry call stack. Hardware
/// VM exit never restores this old snapshot over the live guest's state.
struct FirmwareControls {
    cr0: u64,
    cr4: u64,
    xcr0: u64,
}

impl FirmwareControls {
    /// Initial VMXON/entry failure only: no private host GS has been installed.
    fn restore(&self) {
        // SAFETY: this same CPU captured these exact controls before VMXON.
        // CR4.OSXSAVE is still set here; restore XCR0 before CR4. VMXON failed
        // or VMXOFF succeeded, and no guest has run on this returning path.
        unsafe {
            cpu::xsetbv(0, self.xcr0);
            cpu::write_cr4(self.cr4);
            cpu::write_cr0(self.cr0);
        }
    }
}

/// Little-endian snapshot signature; only the published owned record is captured.
const DIAGNOSTIC_MAGIC: u64 = u64::from_le_bytes(*b"THVSTAT1");
/// Fixed snapshot ABI, independent of the Rust lock's private layout.
const DIAGNOSTIC_VERSION: u64 = 4;
/// Reasons 0..63 have separate bins; newer/unknown basic reasons share bin 64.
const DIAGNOSTIC_REASON_BINS: usize = 65;
/// Selected VMREAD miss encodings, in ABI order; other encodings share the last
/// bin. Only encodings are classified, never the contents of a VMCS field.
const DIAGNOSTIC_READ_FIELDS: [u32; 15] = [
    vmcs::GUEST_RIP,
    vmcs::GUEST_RSP,
    vmcs::GUEST_RFLAGS,
    vmcs::GUEST_INTERRUPTIBILITY_INFO,
    vmcs::GUEST_CR0,
    vmcs::GUEST_CR3,
    vmcs::GUEST_CR4,
    vmcs::GUEST_CS_AR_BYTES,
    vmcs::GUEST_SS_AR_BYTES,
    vmcs::GUEST_ACTIVITY_STATE,
    vmcs::GUEST_IA32_EFER,
    vmcs::GUEST_IA32_PAT,
    vmcs::VM_ENTRY_INTR_INFO_FIELD,
    vmcs::VM_INSTRUCTION_ERROR,
    vmcs::VM_ENTRY_INSTRUCTION_LEN,
];
/// Scope value 1 means the current BSP-only QEMU Direct-VMX prototype.
const DIAGNOSTIC_BSP_SCOPE: u64 = 1;

/// Memory-only progress events; values contain no guest registers or MSR data.
#[derive(Clone, Copy)]
enum DiagnosticEvent {
    L1Exit(u64),
    L0Handled { reason: u64, action: u64 },
    NestedExit(u64),
    Reflected(u64),
    EntryFailure(u64),
    GuestReturned,
    VmcsRead,
    VmcsWrite,
    VmcsLoad,
    ReflectedStateWrite,
    VmcsAccessBatch(vmx::VmcsAccessCounts),
    L1VmreadHardware(u32),
}

/// Movable value state for eventual per-pCPU ownership; all counters saturate.
#[repr(C)]
#[derive(Clone, Copy)]
struct ExitCounterValues {
    l1_exits: u64,
    direct_entry_attempts: u64,
    observed_l2_entries: u64,
    reflected_l2_exits: u64,
    l0_only_handled_exits: u64,
    external_interrupt_exits: u64,
    interrupt_window_exits: u64,
    invept: u64,
    invvpid: u64,
    nested_entry_failures: u64,
    cpuid_exits: u64,
    /// Raw hardware attempts, including failures; no software-cache hit counts.
    vmread_attempts: u64,
    vmwrite_attempts: u64,
    vmptrld_attempts: u64,
    /// Subset of vmwrite_attempts reconstructing L1 state for reflection.
    reflected_state_writes: u64,
    /// 0 initial, 1 L1 exit, 2 L0 handled, 3 direct entry, 4 L2 exit,
    /// 5 reflection complete, 6 immediate entry failure, 7 bounded guest return.
    last_phase: u64,
    /// Raw VM-exit reason, or u64::MAX when unavailable; never a guest payload.
    last_reason: u64,
    /// Actual hardware exits only, not reflected copies or entry-failure exits.
    l1_reasons: [u64; DIAGNOSTIC_REASON_BINS],
    l2_reasons: [u64; DIAGNOSTIC_REASON_BINS],
    /// L1 VMREADs requiring direct-VMCS selection, excluding software hits.
    l1_vmread_hardware: [u64; DIAGNOSTIC_READ_FIELDS.len() + 1],
}

impl ExitCounterValues {
    const fn new() -> Self {
        Self {
            l1_exits: 0,
            direct_entry_attempts: 0,
            observed_l2_entries: 0,
            reflected_l2_exits: 0,
            l0_only_handled_exits: 0,
            external_interrupt_exits: 0,
            interrupt_window_exits: 0,
            invept: 0,
            invvpid: 0,
            nested_entry_failures: 0,
            cpuid_exits: 0,
            vmread_attempts: 0,
            vmwrite_attempts: 0,
            vmptrld_attempts: 0,
            reflected_state_writes: 0,
            last_phase: 0,
            last_reason: u64::MAX,
            l1_reasons: [0; DIAGNOSTIC_REASON_BINS],
            l2_reasons: [0; DIAGNOSTIC_REASON_BINS],
            l1_vmread_hardware: [0; DIAGNOSTIC_READ_FIELDS.len() + 1],
        }
    }

    /// Counts reasons only for actual non-entry-failure hardware exits.
    fn count_reason(&mut self, reason: u64, l1: bool) {
        if reason == u64::MAX || reason & (1 << 31) != 0 {
            return;
        }
        let bin = ((reason & 0xffff) as usize).min(DIAGNOSTIC_REASON_BINS - 1);
        let reasons = if l1 {
            &mut self.l1_reasons
        } else {
            &mut self.l2_reasons
        };
        diagnostic_increment(&mut reasons[bin]);
        match reason & 0xffff {
            EXIT_REASON_EXTERNAL_INTERRUPT => {
                diagnostic_increment(&mut self.external_interrupt_exits)
            }
            EXIT_REASON_INTERRUPT_WINDOW => diagnostic_increment(&mut self.interrupt_window_exits),
            EXIT_REASON_INVEPT if l1 => diagnostic_increment(&mut self.invept),
            EXIT_REASON_INVVPID if l1 => diagnostic_increment(&mut self.invvpid),
            EXIT_REASON_CPUID if l1 => diagnostic_increment(&mut self.cpuid_exits),
            _ => {}
        }
    }

    /// Updates scalar cells only. VMCS operations do not change the last exit
    /// phase/reason; operation telemetry must not obscure progress diagnostics.
    fn record(&mut self, event: DiagnosticEvent) {
        let (phase, reason) = match event {
            DiagnosticEvent::L1VmreadHardware(field) => {
                let bin = DIAGNOSTIC_READ_FIELDS
                    .iter()
                    .position(|&candidate| candidate == field)
                    .unwrap_or(DIAGNOSTIC_READ_FIELDS.len());
                diagnostic_increment(&mut self.l1_vmread_hardware[bin]);
                return;
            }
            DiagnosticEvent::L1Exit(reason) => {
                diagnostic_increment(&mut self.l1_exits);
                self.count_reason(reason, true);
                (1, reason)
            }
            DiagnosticEvent::L0Handled { reason, action } => {
                diagnostic_increment(&mut self.l0_only_handled_exits);
                if matches!(action, VMEXIT_ACTION_VMLAUNCH | VMEXIT_ACTION_VMRESUME) {
                    diagnostic_increment(&mut self.direct_entry_attempts);
                    (3, reason)
                } else {
                    (2, reason)
                }
            }
            DiagnosticEvent::NestedExit(reason) => {
                if reason != u64::MAX {
                    if reason & (1 << 31) == 0 {
                        diagnostic_increment(&mut self.observed_l2_entries);
                    } else {
                        diagnostic_increment(&mut self.nested_entry_failures);
                    }
                }
                self.count_reason(reason, false);
                (4, reason)
            }
            DiagnosticEvent::Reflected(reason) => {
                // Includes a VM-entry-failure exit successfully reflected to L1;
                // observed_l2_entries excludes such entries that never ran L2.
                diagnostic_increment(&mut self.reflected_l2_exits);
                (5, reason)
            }
            DiagnosticEvent::EntryFailure(reason) => {
                diagnostic_increment(&mut self.nested_entry_failures);
                (6, reason)
            }
            DiagnosticEvent::GuestReturned => (7, EXIT_REASON_VMCALL),
            DiagnosticEvent::VmcsRead => {
                diagnostic_increment(&mut self.vmread_attempts);
                return;
            }
            DiagnosticEvent::VmcsWrite => {
                diagnostic_increment(&mut self.vmwrite_attempts);
                return;
            }
            DiagnosticEvent::VmcsLoad => {
                diagnostic_increment(&mut self.vmptrld_attempts);
                return;
            }
            DiagnosticEvent::ReflectedStateWrite => {
                diagnostic_increment(&mut self.vmwrite_attempts);
                diagnostic_increment(&mut self.reflected_state_writes);
                return;
            }
            DiagnosticEvent::VmcsAccessBatch(accesses) => {
                diagnostic_add(&mut self.vmread_attempts, accesses.reads);
                diagnostic_add(&mut self.vmwrite_attempts, accesses.writes);
                diagnostic_add(&mut self.vmptrld_attempts, accesses.loads);
                return;
            }
        };
        diagnostic_store(&mut self.last_phase, phase);
        diagnostic_store(&mut self.last_reason, reason);
    }
}

/// Stores scalar telemetry observably for an external stopped-VM reader.
fn diagnostic_store(destination: &mut u64, value: u64) {
    // SAFETY: destination is an aligned, initialized, exclusively borrowed scalar
    // in the guarded record (or a host test). Volatile preserves external capture
    // visibility without exposing any guest-memory pointer or adding synchronization.
    unsafe { ptr::write_volatile(destination, value) };
}

fn diagnostic_increment(destination: &mut u64) {
    diagnostic_add(destination, 1);
}

fn diagnostic_add(destination: &mut u64, amount: u64) {
    let next = destination.saturating_add(amount);
    diagnostic_store(destination, next);
}

/// Odd means in progress; exhaustion remains permanently odd, never wraps/ABAs.
fn diagnostic_next_sequence(previous: u64) -> Option<(u64, u64)> {
    if previous & 1 != 0 {
        return None;
    }
    Some((previous.checked_add(1)?, previous.checked_add(2)?))
}

/// ABI v4: v3's 152 words followed by 16 L1 VMREAD hardware-miss counters.
/// Sequence is word 3, counters start at 5. No guest address/data is recorded.
/// Readers require magic/version/size/scope and a stable even sequence. Stop all
/// QEMU vCPUs before copying this record; an odd/exhausted snapshot is not valid.
#[repr(C)]
struct ExitDiagnostics {
    magic: u64,
    version: u64,
    size: u64,
    sequence: AtomicU64,
    scope: u64,
    values: ExitCounterValues,
}

impl ExitDiagnostics {
    const fn new() -> Self {
        Self {
            magic: DIAGNOSTIC_MAGIC,
            version: DIAGNOSTIC_VERSION,
            size: core::mem::size_of::<Self>() as u64,
            sequence: AtomicU64::new(0),
            scope: DIAGNOSTIC_BSP_SCOPE,
            values: ExitCounterValues::new(),
        }
    }

    fn record(&mut self, event: DiagnosticEvent) {
        let sequence = diagnostic_next_sequence(self.sequence.load(Ordering::Relaxed));
        self.sequence
            .store(sequence.map_or(u64::MAX, |(odd, _)| odd), Ordering::Relaxed);
        // x86 preserves store order; this compiler fence also prevents moving any
        // scalar store before the observable odd marker or after the even commit.
        compiler_fence(Ordering::SeqCst);
        self.values.record(event);
        compiler_fence(Ordering::Release);
        if let Some((_, even)) = sequence {
            self.sequence.store(even, Ordering::Release);
        }
    }
}

const _: () = assert!(core::mem::size_of::<ExitDiagnostics>() == 1344);

/// No serial or allocation is permitted here: this is the bounded hot-path hook.
fn record_diagnostic(event: DiagnosticEvent) {
    current_cpu().diagnostics.lock().record(event);
}

/// Counts a real VMREAD, never a read from a software mirror.
///
/// # Safety
/// Caller remains at CPL0 in VMX root operation on the owning CPU. The current
/// VMCS and its backing remain live. Unsupported encodings return VMfail through
/// the unchanged HAL result; counting neither loads nor changes a VMCS.
unsafe fn vmcs_read(field: u32) -> Result<u64, VmxStatus> {
    record_diagnostic(DiagnosticEvent::VmcsRead);
    // SAFETY: the caller supplies the HAL's CPU-mode/current-VMCS invariants;
    // the short telemetry lock is released before executing the instruction.
    unsafe { vmx::vmread(field) }
}

/// Counts a real VMWRITE. The caller owns the current resident VMCS at CPL0
/// in VMX root operation; values retain the HAL's exact hardware failure path.
unsafe fn vmcs_write(field: u32, value: u64) -> VmxStatus {
    record_diagnostic(DiagnosticEvent::VmcsWrite);
    // SAFETY: current-VMCS ownership/mode/backing are caller invariants; the
    // telemetry update has completed without changing any hardware VMCS state.
    unsafe { vmx::vmwrite(field, value) }
}

/// Counts a VMPTRLD attempt without modifying VMCS ownership or failure flags.
/// Caller is pinned at CPL0 in VMX root operation and supplies a checked aligned
/// physical VMCS region, distinct from VMXON and owned by this CPU alone.
unsafe fn vmcs_load(region: VmcsPhys) -> VmxStatus {
    record_diagnostic(DiagnosticEvent::VmcsLoad);
    // SAFETY: the caller validated region alignment, residency and CPU ownership
    // according to the HAL contract; hardware still validates its VMCS header.
    unsafe { vmx::vmptrld(region) }
}

/// Writes only a dirty reflected field, comparing the live carrier value.
/// No prior L1 snapshot is trusted: L1 may change selectors, CRs, MSRs or tables
/// without exiting. The current VM-exit state is the sole equality witness.
/// Only the owned carrier may be current; fields/values are validated by the
/// reflection manifest, and a read failure is returned without hiding it.
unsafe fn vmcs_write_reflected(field: u32, value: u64) -> VmxStatus {
    // SAFETY: this CPU owns the stopped carrier; each field is readable as well
    // as writable. No guest execution or VMCS switch occurs between comparison
    // and write, so equality cannot race an architectural update.
    match unsafe { vmcs_read(field) } {
        Ok(current) if current == value => return VmxStatus::Success,
        Ok(_) => {}
        Err(status) => return status,
    }
    record_diagnostic(DiagnosticEvent::ReflectedStateWrite);
    // SAFETY: reflection selected this CPU's resident carrier in VMX root;
    // field/value are the validated L1 host state or architectural exit resets.
    unsafe { vmx::vmwrite(field, value) }
}

/// Preserves existing cold diagnostic fields with a saturating counter.
fn cpuid_exit_count() -> u64 {
    current_cpu().diagnostics.lock().values.cpuid_exits
}

/// Publishes only this CPU's reserved metadata, before VMX entry. This explicit
/// reference is used while firmware GS is still active, never current_cpu().
fn publish_diagnostics(
    monitor: &CpuMonitor,
    block: u64,
    block_end: u64,
    serial: &mut SerialPort,
) -> Result<(), Error> {
    let mut diagnostics = monitor.diagnostics.lock();
    *diagnostics = ExitDiagnostics::new();
    let address = ptr::from_ref(&*diagnostics) as usize as u64;
    let size = core::mem::size_of::<ExitDiagnostics>() as u64;
    let end = address
        .checked_add(size)
        .ok_or(Error::OutsideIdentityMap(address))?;
    if address < block || end > block_end || end > IDENTITY_MAP_LIMIT {
        return Err(Error::OutsideIdentityMap(end));
    }
    drop(diagnostics);
    let _ = writeln!(
        serial,
        "thin-hv: vmx diagnostics address={address:#018x} size={size} version={DIAGNOSTIC_VERSION} scope=bsp-only environment=qemu-prototype storage=cpu-runtime"
    );
    Ok(())
}

/// Emits a bounded summary only after the existing returning guest leaves VMX.
fn log_diagnostic_summary(serial: &mut SerialPort) {
    let diagnostics = current_cpu().diagnostics.lock();
    let values = diagnostics.values;
    let sequence = diagnostics.sequence.load(Ordering::Acquire);
    drop(diagnostics);
    serial.write_bytes(b"thin-hv: vmx diagnostics summary scope=bsp-only");
    for (name, value) in [
        (&b"sequence"[..], sequence),
        (&b"l1_exits"[..], values.l1_exits),
        (&b"direct_entry_attempts"[..], values.direct_entry_attempts),
        (&b"observed_l2_entries"[..], values.observed_l2_entries),
        (&b"reflected_l2_exits"[..], values.reflected_l2_exits),
        (&b"l0_only_handled_exits"[..], values.l0_only_handled_exits),
        (
            &b"external_interrupt_exits"[..],
            values.external_interrupt_exits,
        ),
        (
            &b"interrupt_window_exits"[..],
            values.interrupt_window_exits,
        ),
        (&b"invept"[..], values.invept),
        (&b"invvpid"[..], values.invvpid),
        (&b"nested_entry_failures"[..], values.nested_entry_failures),
        (&b"cpuid_exits"[..], values.cpuid_exits),
        (&b"vmread_attempts"[..], values.vmread_attempts),
        (&b"vmwrite_attempts"[..], values.vmwrite_attempts),
        (&b"vmptrld_attempts"[..], values.vmptrld_attempts),
        (
            &b"reflected_state_writes"[..],
            values.reflected_state_writes,
        ),
        (&b"last_phase"[..], values.last_phase),
        (&b"last_reason"[..], values.last_reason),
    ] {
        write_raw_field(serial, name, value);
    }
    write_raw_newline(serial);
}

const _: () =
    assert!(HOST_TABLE_FIRST_PAGE + HOST_TABLE_PAGES as u64 == HOST_ENVIRONMENT_FIRST_PAGE);
const _: () = assert!(ERROR_REVISION_PAGE + 1 == CPU_STATE_FIRST_PAGE);

/// Bootstrap-to-runtime handoff retained for the direct nested `StartImage` call.
const RUNTIME_MODE: u32 = if cfg!(feature = "physical-direct-vmx") {
    1
} else {
    0
};

#[repr(C)]
#[derive(Clone, Copy)]
struct RuntimeHandoff {
    guest: efi::Handle,
    profile: u32,
    mode: u32,
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
    l1_interruptibility: u64,
    outer_reason: u64,
    outer_qualification: u64,
    outer_rip: u64,
    outer_instruction_len: u64,
}

/// One immovable CPU-owned object, bound through that CPU's private host GS.
/// Locks protect separate short metadata borrows, not a shared SMP instance.
/// Diagnostics must remain separate: MSR-list processing can count VMCS accesses
/// while holding runtime metadata. No guard may span hardware guest entry.
///
/// The backing allocation also owns this CPU's VMXON/carrier, host stack,
/// GDT/IDT/TSS/IST, XSTATE scratch and private host page tables. BSP launch uses
/// this object now; AP/INIT/SIPI support is still required before enabling SMP.
#[repr(C, align(16))]
struct CpuMonitor {
    physical_bits: u8,
    /// Captured on this physical CPU before entry, immutable for its VMX
    /// lifetime. l1_ia32e is deliberately false here, never cached guest mode.
    host_limits: host_validation::Limits,
    diagnostics: SpinLock<ExitDiagnostics>,
    vcpu: SpinLock<VcpuState>,
    carrier_patch: SpinLock<Option<[u64; DIRECT_VMCS_PATCH_MANIFEST.len()]>>,
    direct_patch: SpinLock<Option<(VmcsPhys, [u64; DIRECT_VMCS_PATCH_MANIFEST.len()])>>,
    entry_policy: SpinLock<Option<(VmcsPhys, u64)>>,
    nested_run: SpinLock<Option<NestedRun>>,
    runtime: SpinLock<CpuRuntimeState>,
}

impl CpuMonitor {
    fn new(runtime: CpuRuntimeState, host_limits: host_validation::Limits) -> Self {
        Self {
            physical_bits: runtime.ram.physical_width().bits(),
            host_limits,
            diagnostics: SpinLock::new(ExitDiagnostics::new()),
            vcpu: SpinLock::new(VcpuState::new()),
            carrier_patch: SpinLock::new(None),
            direct_patch: SpinLock::new(None),
            entry_policy: SpinLock::new(None),
            nested_run: SpinLock::new(None),
            runtime: SpinLock::new(runtime),
        }
    }

    /// Combine immutable CPU limits with this entry's actual stopped L1 mode.
    /// VMXOFF/VMXON in L1 does not change physical CPU capabilities. A physical
    /// CPU reset/migration/resume must rebuild the owning monitor, not copy it.
    fn host_limits_for_efer(&self, efer: u64) -> host_validation::Limits {
        host_validation::Limits {
            l1_ia32e: efer & (1 << 10) != 0,
            ..self.host_limits
        }
    }
}

/// Per-CPU MSR mirrors, VPID lifetime and physical access metadata. No mutable
/// reference spans guest entry or a call into another CPU's context.
#[repr(C, align(16))]
struct CpuRuntimeState {
    vpids: VpidNamespace,
    exit_snapshot: Option<ExitSnapshot>,
    ram: FirmwareMap<205>,
    basic: vmx::VmxBasic,
    private: [(u64, u64); 2],
    mmio: MmioMap,
    /// Absent only for host policy tests, which never execute physical access.
    window: Option<ept::HostWindow>,
    /// Explicit L1-visible bootstrap storage, not the private PE's copied statics.
    bootstrap: Option<GuestBootstrap>,
    entry: [MsrEntry; nested_vmx::MAX_MSR_MIRROR_ENTRIES as usize],
    entry_count: u32,
    exit_capture: [MsrEntry; 2],
    capture_count: u32,
    host_load: [MsrEntry; nested_vmx::MAX_MSR_MIRROR_ENTRIES as usize],
    host_count: u32,
    host_owner: Option<VmcsPhys>,
    aborted: bool,
    inherited: PatEfer,
    entry_loaded: PatEfer,
}

impl CpuRuntimeState {
    /// Initial state is overwritten by a carrier snapshot before nested entry.
    fn new(
        ram: FirmwareMap<205>,
        basic: vmx::VmxBasic,
        private: [(u64, u64); 2],
        mmio: MmioMap,
        window: Option<ept::HostWindow>,
        bootstrap: Option<GuestBootstrap>,
    ) -> Self {
        Self {
            vpids: VpidNamespace::new(),
            exit_snapshot: None,
            ram,
            basic,
            private,
            mmio,
            window,
            bootstrap,
            entry: [MsrEntry::new(0, 0); nested_vmx::MAX_MSR_MIRROR_ENTRIES as usize],
            entry_count: 0,
            exit_capture: [MsrEntry::new(0x1d9, 0), MsrEntry::new(0x38f, 0)],
            capture_count: 0,
            host_load: [MsrEntry::new(0, 0); nested_vmx::MAX_MSR_MIRROR_ENTRIES as usize],
            host_count: 0,
            host_owner: None,
            aborted: false,
            inherited: PatEfer { pat: 0, efer: 0 },
            entry_loaded: PatEfer { pat: 0, efer: 0 },
        }
    }

    /// Requires actual firmware RAM, current host-map coverage and exclusion of
    /// all Rust-owned monitor storage before creating a physical pointer.
    fn allows_list_access(&self, address: u64, bytes: u64, write: bool) -> bool {
        let Some(end) = address.checked_add(bytes) else {
            return false;
        };
        end <= IDENTITY_MAP_LIMIT
            && self
                .private
                .iter()
                .all(|&(start, limit)| end <= start || limit <= address)
            && self.ram.allows_ram_access(address, bytes, write)
    }

    /// Validates only this CPU's two allocated VMCS roles, never an arbitrary
    /// private address and never a guest VMCS merely because it lies in RAM.
    fn private_vmcs(&self, address: u64) -> Option<VmcsPhys> {
        let (base, limit) = self.private[0];
        let end = address.checked_add(PAGE_SIZE)?;
        if ![1, ERROR_REVISION_PAGE]
            .into_iter()
            .any(|page| base.checked_add(page * PAGE_SIZE) == Some(address))
            || end > limit
            || end > IDENTITY_MAP_LIMIT
            || !self.ram.allows_ram_access(address, PAGE_SIZE, true)
        {
            return None;
        }
        VmcsPhys::new(address)
    }

    /// BASIC's 32-bit VMX-region restriction is independent of CPUID MAXPHYADDR.
    /// Lists use the architectural physical width instead, so do not apply this
    /// narrower rule to ordinary operands or MSR lists.
    fn allows_vmx_region(&self, address: u64) -> bool {
        (!self.basic.physical_address_width_32 || address < 1 << 32)
            && address.is_multiple_of(PAGE_SIZE)
            && self.allows_list_access(address, PAGE_SIZE, true)
    }

    /// Guest operands may use the explicit L1 stack within the retained block,
    /// but never any other monitor data, paging structure or runtime PE page.
    fn allows_operand_ram(&self, address: u64, bytes: u64, write: bool) -> bool {
        let Some(end) = address.checked_add(bytes) else {
            return false;
        };
        let stack = self.private[0].0.checked_add(GUEST_STACK_PAGE * PAGE_SIZE);
        let stack_end = stack.and_then(|start| start.checked_add(GUEST_STACK_PAGES * PAGE_SIZE));
        let guest_stack = stack.zip(stack_end).is_some_and(|(start, limit)| {
            limit <= self.private[0].1 && start <= address && end <= limit
        });
        (self.allows_list_access(address, bytes, write) || guest_stack)
            && end <= IDENTITY_MAP_LIMIT
            && self.ram.allows_ram_access(address, bytes, write)
    }

    /// No speculative physical dereference: MMIO must be in the validated UC
    /// inventory and outside every monitor allocation. Holes remain backing
    /// failures, distinct from the guest's architectural paging exceptions.
    fn operand_backing(&self, address: u64, write: bool) -> Option<bool> {
        if self.allows_operand_ram(address, 1, write) {
            return Some(false);
        }
        let end = address.checked_add(1)?;
        (self
            .private
            .iter()
            .all(|&(start, limit)| end <= start || limit <= address)
            && self
                .mmio
                .ranges()
                .iter()
                .any(|range| range.start() <= address && end <= range.end()))
        .then_some(true)
    }

    /// One checked byte access. The short per-CPU lock serializes the scratch
    /// PTE; no callback, allocation, nested entry or guest execution intervenes.
    fn operand_byte(&mut self, physical: u64, value: Option<u8>) -> Option<u8> {
        let mmio = self.operand_backing(physical, value.is_some())?;
        let window = if mmio { Some(self.window?) } else { None };
        let address = if let Some(window) = window {
            let entry = window.entry(HostPhys::new(physical & !4095)?).ok()?;
            // SAFETY: this CPU owns the retained WB host table arena and its
            // only scratch PTE. No Rust table borrow survives construction; IF
            // is clear and the CPU-state lock excludes concurrent window use.
            // The selected page is validated UC MMIO, never a RAM cache alias.
            unsafe {
                ptr::write_volatile(window.pte_address() as *mut u64, entry);
                core::arch::asm!("invlpg [{address}]", address = in(reg) ept::HostWindow::VIRTUAL_BASE, options(nostack, preserves_flags));
            }
            ept::HostWindow::VIRTUAL_BASE + (physical & 4095)
        } else {
            physical
        };
        let result;
        // SAFETY: coverage, width, RAM permissions/ownership or the active UC
        // window were checked above. The sole L1 CPU is stopped. Integer-address
        // assembly also permits legitimate RAM at PA zero without a Rust null
        // dereference. Exactly one byte is accessed, with no implicit SIMD use.
        unsafe {
            if let Some(value) = value {
                core::arch::asm!("mov byte ptr [{address}], {value}", address = in(reg) address, value = in(reg_byte) value, options(nostack, preserves_flags));
                result = value;
            } else {
                core::arch::asm!("mov {value}, byte ptr [{address}]", address = in(reg) address, value = out(reg_byte) result, options(nostack, preserves_flags));
            }
        }
        if let Some(window) = window {
            // SAFETY: the same CPU still exclusively owns this installed PTE;
            // clear it and evict the translation before releasing the lock.
            unsafe {
                ptr::write_volatile(window.pte_address() as *mut u64, 0);
                core::arch::asm!("invlpg [{address}]", address = in(reg) ept::HostWindow::VIRTUAL_BASE, options(nostack, preserves_flags));
            }
        }
        Some(result)
    }

    /// Page tables must be RAM, never MMIO. A/D changes are atomic with foreign
    /// CPU writes; never replace a newly installed PFN/permission with an old
    /// read-modify-write snapshot. PA zero remains valid without Rust null UB.
    fn paging_word(&self, physical: u64, update: u64) -> Option<u64> {
        if update & !((1 << 5) | (1 << 6)) != 0
            || !physical.is_multiple_of(8)
            || !self.allows_operand_ram(physical, 8, update != 0)
        {
            return None;
        }
        // SAFETY: the complete aligned 8-byte word is accessible foreign RAM,
        // never MMIO or Rust-owned monitor storage. Only architectural A/D bits
        // can be ORed. LOCK OR atomically preserves other CPUs' concurrent bit,
        // PFN and permission changes; the aligned MOV is an atomic x86-64 load.
        // No non-atomic Rust data aliases the word. Both operations declare memory
        // effects, and LOCK OR declares its flags clobber. PA zero is allowed.
        unsafe {
            if update != 0 {
                core::arch::asm!("lock or qword ptr [{address}], {bits}", address = in(reg) physical, bits = in(reg) update, options(nostack));
            }
            let value: u64;
            core::arch::asm!("mov {value}, [{address}]", value = out(reg) value, address = in(reg) physical, options(nostack, preserves_flags));
            Some(value)
        }
    }

    /// Captures ordinary RAM without performing a guest-visible MSR write.
    /// Hardware still checks contents/value faults at the actual entry stage.
    fn prepare_entry(&mut self, list: MsrList) -> Option<(u64, u32)> {
        self.entry_count = 0;
        if list.count() == 0 {
            return Some((0, 0));
        }
        if !self.allows_list_access(list.address(), u64::from(list.count()) * 16, false) {
            return None;
        }
        for index in 0..list.count() {
            let source = list.entry_address(index)?;
            // SAFETY: List checked alignment/count/physical width. Complete
            // readable RAM and L0 ownership exclusions were checked above.
            // This BSP is the only running L1 CPU and is stopped; the source is
            // not MMIO or any Rust-owned monitor object. The private destination
            // is not published to hardware until the loop and lock complete.
            self.entry[index as usize] = unsafe { read_physical_msr_entry(source) };
        }
        self.entry_count = list.count();
        Some((self.entry.as_ptr() as usize as u64, self.entry_count))
    }

    /// Only architecturally known readable capture MSRs reach hardware's exit
    /// store list. Arbitrary L1 store items are handled after L0 is safely entered.
    fn prepare_capture(&mut self, store_count: u32) -> Option<(u64, u32)> {
        self.capture_count = 0;
        if store_count == 0 {
            return Some((0, 0));
        }
        // SAFETY: this CPU runs in its private GS/IDT/IST environment, with IF
        // clear and no active guest. Absent MSRs return None, never a root fault.
        // DEBUGCTL is the mandatory VMX debug MSR; readable PERF_GLOBAL_CTRL
        // supports VMX automatic storage on the Intel VMX CPU used here.
        unsafe {
            host_state::try_rdmsr(0x1d9)?;
            self.capture_count = if host_state::try_rdmsr(0x38f).is_some() {
                2
            } else {
                1
            };
        }
        Some((
            self.exit_capture.as_ptr() as usize as u64,
            self.capture_count,
        ))
    }

    /// Reads one original list item, only while the owning L1/L2 is stopped.
    fn list_item(&self, list: MsrList, index: u32) -> Option<MsrEntry> {
        let source = list.entry_address(index)?;
        if !self.allows_list_access(source, 16, false) {
            return None;
        }
        // SAFETY: List and the complete RAM/ownership check establish alignment,
        // initialized foreign RAM and current host-map coverage. No other guest
        // CPU runs; the source cannot alias any Rust-owned monitor object.
        Some(unsafe { read_physical_msr_entry(source) })
    }

    /// Stores only the value half, preserving L1's index and reserved word.
    fn store_value(&self, list: MsrList, index: u32, value: u64) -> Option<()> {
        let destination = list.entry_address(index)?.checked_add(8)?;
        if !self.allows_list_access(destination, 8, true) {
            return None;
        }
        // SAFETY: this aligned value slot is writable foreign RAM inside the
        // host map, outside all monitor-owned pages. L1/L2 is stopped and no
        // Rust borrow aliases the destination. Earlier stores remain visible.
        unsafe { ptr::write_volatile(destination as *mut u64, value) };
        Some(())
    }

    /// Copies L1 host loads only after all original exit stores have completed.
    fn prepare_host_load(&mut self, list: MsrList, owner: VmcsPhys) -> Option<(u64, u32)> {
        if self.host_owner.is_some() || self.host_count != 0 {
            return None;
        }
        if list.count() == 0 {
            return Some((0, 0));
        }
        for index in 0..list.count() {
            self.host_load[index as usize] = self.list_item(list, index)?;
        }
        self.host_count = list.count();
        self.host_owner = Some(owner);
        Some((self.host_load.as_ptr() as usize as u64, self.host_count))
    }

    /// Processes the original store list in order, never exposing private host
    /// replacements. None means unsupported L0 backing; Err means VMX abort 1.
    fn store_guest_msrs(&self, list: MsrList) -> Option<Result<(), ()>> {
        for index in 0..list.count() {
            let item = self.list_item(list, index)?;
            if !item.format_valid(MsrOperation::ExitStore) {
                return Some(Err(()));
            }
            let value = match exit_store_source(item.index) {
                // SAFETY: the stopped L2's Direct VMCS is current on its owning
                // CPU. These mandatory fields hold hardware-saved guest values.
                ExitStoreSource::GuestField(field) => Some(unsafe { vmcs_read(field) }.ok()?),
                ExitStoreSource::PrivateDebugCapture => {
                    (self.capture_count >= 1).then_some(self.exit_capture[0].value)
                }
                ExitStoreSource::PrivatePerfCapture => {
                    (self.capture_count == 2).then_some(self.exit_capture[1].value)
                }
                ExitStoreSource::VmxCapability => l1_vmx_capability(item.index),
                // SAFETY: private GS/IDT/IST and IF=0 satisfy the guarded read
                // contract. This MSR is neither switched by VMX nor used by L0;
                // unsupported model-specific reads return None rather than #GP.
                ExitStoreSource::Live => unsafe { host_state::try_rdmsr(item.index) },
            };
            let Some(value) = value else {
                return Some(Err(()));
            };
            self.store_value(list, index, value)?;
        }
        Some(Ok(()))
    }
}

/// Reads two aligned words without forming a Rust pointer at physical zero.
///
/// # Safety
/// The caller must prove complete readable, identity-mapped foreign RAM for the
/// aligned 16-byte record, with no running guest CPU or Rust-owned object alias.
unsafe fn read_physical_msr_entry(address: u64) -> MsrEntry {
    let index: u64;
    let value: u64;
    // SAFETY: the caller validated both complete words and stopped their only
    // guest owner. These scalar loads preserve reserved bits and use no SIMD.
    unsafe {
        core::arch::asm!("mov {index}, [{address}]", "mov {value}, [{address} + 8]",
            address = in(reg) address, index = out(reg) index, value = out(reg) value,
            options(nostack, preserves_flags));
    }
    MsrEntry {
        index: index as u32,
        reserved: (index >> 32) as u32,
        value,
    }
}

const _: () = assert!(core::mem::align_of::<CpuMonitor>() <= 4096);
const _: () = assert!(CPU_STATE_FIRST_PAGE as usize + CPU_STATE_PAGES == MONITOR_PAGES);

/// Borrows only the current CPU's runtime metadata, never across hardware entry.
/// The callback cannot return a reference derived from the short lock guard.
fn with_cpu_runtime<T>(inspect: impl FnOnce(&mut CpuRuntimeState) -> T) -> Option<T> {
    let mut state = current_cpu().runtime.try_lock()?;
    Some(inspect(&mut state))
}

/// Post-VM-exit only. Never query private GS during firmware-side preparation.
fn current_cpu() -> &'static CpuMonitor {
    // SAFETY: VMCS HOST_GS_BASE installed this CPU's initialized HostXstate.
    // Its bound monitor object is immovable, correctly aligned and retained on
    // every terminal path. Only shared lock references escape this function;
    // no CPU migration or guest-controlled GS base is used in VMX root.
    let pointer = unsafe { host_state::monitor_data() };
    if let Some(pointer) = pointer {
        // SAFETY: bind_monitor_data receives exactly one live CpuMonitor for
        // this host environment. Guest EPT excludes the complete allocation;
        // construction finished before publication and backing is never freed
        // after the first VM exit. Interior mutability is only through locks.
        return unsafe { pointer.cast::<CpuMonitor>().as_ref() };
    }
    // A missing binding is an L0 lifetime invariant failure, never a guest bad
    // operand or invalid VMCS. Avoid telemetry here: it needs the broken binding.
    SerialPort.write_bytes(b"thin-hv: private CPU binding FAIL\n");
    halt_with_guest_xstate()
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
    /// An address cannot be owned by the private low-canonical host map.
    OutsideIdentityMap(u64),
    /// A UEFI service used to load the nested payload failed.
    Firmware(&'static str, usize),
    /// A private descriptor, exception stack, or host ABI invariant was rejected.
    HostState(host_state::Error),
    /// The platform inventory or complete EPT construction was rejected.
    Platform(chainload::Error),
    /// The already loaded project image cannot be copied safely.
    Resident(resident_image::Error),
}

impl From<chainload::Error> for Error {
    fn from(error: chainload::Error) -> Self {
        let chainload::Error::Firmware(service, status) = error;
        Self::Firmware(service, status)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Platform(error) => write!(formatter, "platform map: {error}"),
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
                write!(formatter, "private host map cannot own {address:#x}")
            }
            Self::Firmware(service, status) => {
                write!(formatter, "{service} status={status:#x}")
            }
            Self::HostState(error) => write!(formatter, "private host state {error:?}"),
            Self::Resident(error) => write!(formatter, "resident image {error:?}"),
        }
    }
}

/// Starts the selected guest backend.
pub(crate) fn run(
    parent_image: efi::Handle,
    system_table: *mut efi::SystemTable,
    serial: &mut SerialPort,
) -> Result<(), Error> {
    #[cfg(feature = "physical-direct-vmx")]
    crate::physical_chainload::require_single_cpu(system_table, serial).map_err(Error::Platform)?;
    let loaded_image = loaded_image_protocol(parent_image, system_table)?;
    // SAFETY: HandleProtocol returned the non-null, live protocol for this
    // executing image. Copy only metadata while Boot Services are still live.
    let (parent_device, code_type) = unsafe {
        (
            (*loaded_image).device_handle,
            (*loaded_image).image_code_type,
        )
    };
    if code_type != efi::RUNTIME_SERVICES_CODE {
        let _ = writeln!(serial, "thin-hv: loading runtime monitor");
        return start_runtime_monitor(parent_image, parent_device, system_table);
    }
    let _ = writeln!(serial, "thin-hv: runtime monitor active");
    start_resident_core(loaded_image, system_table, serial)
}

/// Only these three addresses intentionally cross from L0 into the retained
/// firmware runtime image. That image owns L1's bootstrap and legacy test hooks.
#[repr(C)]
#[derive(Clone, Copy)]
struct GuestBootstrap {
    entry: u64,
    marker: u64,
    status: u64,
}

/// Scalar ABI boundary: no private-copy strings, references or vtables escape.
#[repr(C)]
struct ResidentHandoff {
    system_table: *mut efi::SystemTable,
    image_base: u64,
    image_size: u64,
    monitor_allocation: u64,
    bootstrap: GuestBootstrap,
}

/// Owns one pre-entry allocation through all validation and cleanup errors.
/// Both callers below return only before guest entry and after any VMX teardown;
/// successful entry never returns, so no live private backing can be released.
fn with_runtime_pages<T>(
    system_table: *mut efi::SystemTable,
    kind: u32,
    pages: usize,
    limit: u64,
    prepare: impl FnOnce(u64) -> Result<T, Error>,
) -> Result<T, Error> {
    let mut allocation = limit - 1;
    // SAFETY: both call sites supply bounded nonzero page counts and a checked
    // physical/canonical limit. Boot Services are live; this output describes
    // new exclusive runtime pages, not a registered firmware PE image.
    let status = unsafe {
        ((*(*system_table).boot_services).allocate_pages)(
            efi::ALLOCATE_MAX_ADDRESS,
            kind,
            pages,
            &mut allocation,
        )
    };
    if status.is_error() {
        return Err(Error::Allocate(status.as_usize()));
    }
    let result = prepare(allocation);
    // SAFETY: preparation returns only before guest entry and after VMXOFF plus
    // CPU restoration. No VMCS, host root, or firmware PE relocation metadata
    // retains this exact allocation. Errors contain no pointers into its pages.
    let status = unsafe { ((*(*system_table).boot_services).free_pages)(allocation, pages) };
    if status.is_error() {
        return Err(Error::Firmware(
            "FreePages resident allocation",
            status.as_usize(),
        ));
    }
    result
}

/// Firmware authenticates/loads the source normally; only L0's copy is kept out
/// of firmware PE relocation registration. No firmware identity is rewritten.
fn start_resident_core(
    loaded_image: *mut efi::protocols::loaded_image::Protocol,
    system_table: *mut efi::SystemTable,
    serial: &mut SerialPort,
) -> Result<(), Error> {
    let handoff = runtime_handoff(loaded_image).ok_or(Error::Firmware(
        "runtime guest-image handoff",
        efi::Status::INVALID_PARAMETER.as_usize(),
    ))?;
    let guest = handoff.0;
    #[cfg(not(feature = "physical-direct-vmx"))]
    let profile = handoff.1;
    // SAFETY: run validated this live protocol for the executing runtime image.
    let (source, size, data_type) = unsafe {
        (
            (*loaded_image).image_base as u64,
            (*loaded_image).image_size,
            (*loaded_image).image_data_type,
        )
    };
    let pages = resident_image::image_pages(source, size).map_err(Error::Resident)?;
    if data_type != efi::RUNTIME_SERVICES_DATA {
        return Err(Error::Firmware(
            "bootstrap requires runtime image data",
            efi::Status::UNSUPPORTED.as_usize(),
        ));
    }
    GUEST_IMAGE.store(guest, Ordering::Release);
    SYSTEM_TABLE.store(system_table, Ordering::Release);
    GUEST_RAN.store(0, Ordering::Release);
    GUEST_STATUS.store(usize::MAX, Ordering::Release);
    let width =
        max_physical_address_bits().ok_or(Error::Capability("resident physical width", 0))?;
    let limit = (1_u64 << width).min(IDENTITY_MAP_LIMIT);
    // SAFETY: efi_main established CPUID.VMX and this BSP runs at CPL0 before
    // entry. BASIC's narrower limit applies only to the VMX storage allocation.
    let basic = vmx::VmxBasic::from_msr(unsafe { cpu::rdmsr(vmx::IA32_VMX_BASIC) });
    let vmx_limit = limit.min(if basic.physical_address_width_32 {
        1 << 32
    } else {
        u64::MAX
    });
    with_runtime_pages(
        system_table,
        efi::RUNTIME_SERVICES_CODE,
        pages + 2,
        limit,
        |allocation| {
            with_runtime_pages(
                system_table,
                efi::RUNTIME_SERVICES_DATA,
                MONITOR_ALLOCATION_PAGES,
                vmx_limit,
                |monitor_allocation| {
                    // All runtime allocations precede legacy test-hook installation. OVMF
                    // republishes its MAT when runtime memory changes; installing earlier
                    // would lose the original image's narrowly scoped executable fixup.
                    // Hooks remain in that registered image, never in the private L0 copy.
                    #[cfg(not(feature = "physical-direct-vmx"))]
                    let overlay = runtime_variables::install(system_table, profile, source, size)
                        .map_err(|status| {
                        Error::Firmware("install variable overlay", status.as_usize())
                    })?;
                    #[cfg(not(feature = "physical-direct-vmx"))]
                    let _ = writeln!(
                        serial,
                        "thin-hv: variable overlay profile={} mat_patches={}",
                        profile.0,
                        overlay.memory_attribute_patch_count()
                    );
                    let result = (|| {
                        let guarded =
                            resident_image::GuardedAllocation::new(allocation, pages, limit)
                                .map_err(Error::Resident)?;
                        let destination = guarded.payload;
                        resident_image::image_pages(destination, size).map_err(Error::Resident)?;
                        let (entry, bootstrap) = platform_snapshot::with_snapshot(
                            system_table,
                            serial,
                            |cpu, map, _serial| {
                                let reject = || {
                                    platform_snapshot::malformed(
                                        "resident code allocation/PE validation",
                                    )
                                };
                                if !cpu.identity_range_supported(
                                    guarded.base,
                                    (guarded.end - guarded.base) as usize,
                                ) || !map.readable(source, size as usize)
                                    || (source < guarded.end && guarded.base < source + size)
                                {
                                    return Err(reject());
                                }
                                let mtrrs =
                                    cpu.mtrrs().map_err(|_| reject())?.ok_or_else(reject)?;
                                for address in
                                    (guarded.base..guarded.end).step_by(PAGE_SIZE as usize)
                                {
                                    let region = (0..map.count())
                                        .filter_map(|index| map.region(index))
                                        .find(|region| {
                                            region.start <= address
                                                && region.end().is_some_and(|limit| {
                                                    address + PAGE_SIZE <= limit
                                                })
                                        });
                                    if !region.is_some_and(|region| {
                                        region.kind == efi::RUNTIME_SERVICES_CODE
                                            && region.attributes
                                                & (efi::MEMORY_RUNTIME | efi::MEMORY_WB)
                                                == (efi::MEMORY_RUNTIME | efi::MEMORY_WB)
                                            && region.attributes
                                                & (efi::MEMORY_RP
                                                    | efi::MEMORY_RO
                                                    | efi::MEMORY_WP
                                                    | efi::MEMORY_XP)
                                                == 0
                                    }) || mtrrs.memory_type(address).map_err(|_| reject())?
                                        != platform_memory::MemoryType::WriteBack
                                    {
                                        return Err(reject());
                                    }
                                }
                                // No firmware call, callback dispatch, lock or static write
                                // occurs while the source PE is borrowed. Overlay/bootstrap
                                // initialization is complete; VMX and guest execution have not
                                // begun. Only stack temporaries and disjoint output are written.
                                let source_bytes = map
                                    .firmware_bytes(source, size as usize)
                                    .ok_or_else(reject)?;
                                let pe = resident_image::LoadedPe::parse(source_bytes, source)
                                    .map_err(|_| reject())?;
                                let entry = pe
                                    .entry(destination, resident_entry as usize as u64)
                                    .map_err(|_| reject())?;
                                let bootstrap = GuestBootstrap {
                                    entry: pe
                                        .entry(source, guest_entry as usize as u64)
                                        .map_err(|_| reject())?,
                                    marker: ptr::addr_of!(GUEST_RAN) as u64,
                                    status: ptr::addr_of!(GUEST_STATUS) as u64,
                                };
                                if !pe.bootstrap_word(bootstrap.marker)
                                    || !pe.bootstrap_word(bootstrap.status)
                                    || bootstrap.marker == bootstrap.status
                                {
                                    return Err(reject());
                                }
                                // SAFETY: the complete nonzero, aligned allocation is disjoint
                                // from the source, uniquely owned writable/executable WB runtime
                                // RAM under firmware paging. No CPU or firmware image metadata
                                // references the destination. The mutable borrow ends before
                                // calling copied code; runtime execution retains all its pages.
                                let output = unsafe {
                                    core::slice::from_raw_parts_mut(
                                        guarded.base as *mut u8,
                                        (guarded.end - guarded.base) as usize,
                                    )
                                };
                                output.fill(0);
                                pe.copy_to(
                                    &mut output
                                        [PAGE_SIZE as usize..PAGE_SIZE as usize + size as usize],
                                    destination,
                                )
                                .map_err(|_| reject())?;
                                Ok((entry, bootstrap))
                            },
                        )
                        .map_err(Error::Platform)?;
                        let _ = writeln!(
                            serial,
                            "thin-hv: resident image PASS source={source:#018x} private={destination:#018x} bytes={size:#x} firmware_relocation=excluded bootstrap=firmware-runtime boot_guards=2"
                        );
                        let handoff = ResidentHandoff {
                            system_table,
                            image_base: destination,
                            image_size: size,
                            monitor_allocation,
                            bootstrap,
                        };
                        // CPUID serializes this CPU's instruction stream after copying code.
                        let _ = cpu::cpuid(0, 0);
                        // SAFETY: entry is the validated executable RVA of resident_entry,
                        // rebased into our complete loaded AMD64 PE copy. Its exact C ABI
                        // consumes this live handoff only before VM entry. This allocation
                        // remains owned until it returns with VMX disabled, or forever after
                        // successful entry. Only a scalar status returns; no borrowed
                        // copied-image error, string or vtable escapes through it.
                        let status = unsafe {
                            let entry: extern "C" fn(&ResidentHandoff) -> usize =
                                core::mem::transmute(entry as usize);
                            entry(&handoff)
                        };
                        Err(Error::Firmware("resident VMX entry returned", status))
                    })();
                    // Rollback runs in the original image with its original statics. It
                    // precedes FreePages, which may replace the MAT whose edits it restores.
                    #[cfg(not(feature = "physical-direct-vmx"))]
                    overlay.rollback().map_err(|status| {
                        Error::Firmware("restore variable overlay", status.as_usize())
                    })?;
                    result
                },
            )
        },
    )
}

/// Executed exclusively from the independent PE; no borrowed error escapes it.
extern "C" fn resident_entry(handoff: &ResidentHandoff) -> usize {
    let mut serial = SerialPort;
    match run_direct_monitor(handoff, &mut serial) {
        Ok(()) => efi::Status::SUCCESS.as_usize(),
        Err(error) => {
            let _ = writeln!(serial, "thin-hv: resident VMX launch FAIL: {error}");
            efi::Status::ABORTED.as_usize()
        }
    }
}

fn run_direct_monitor(handoff: &ResidentHandoff, serial: &mut SerialPort) -> Result<(), Error> {
    let system_table = handoff.system_table;
    let image_base = handoff.image_base;
    let image_size = handoff.image_size;
    let image_end = image_base + image_size;
    // SAFETY: efi_main checked CPUID.VMX before dispatching this backend, and
    // the x86-64 UEFI application still executes at CPL0 before ExitBootServices.
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

    let primary_msr = if basic.true_controls {
        vmx::IA32_VMX_TRUE_PROCBASED_CTLS
    } else {
        vmx::IA32_VMX_PROCBASED_CTLS
    };
    // SAFETY: CPUID.VMX establishes the legacy control MSR; VMX_BASIC's
    // true-controls flag additionally establishes the selected true-control MSR.
    let primary_capability = unsafe { cpu::rdmsr(primary_msr) };
    if (primary_capability >> 32) as u32 & vmcs::PRIMARY_EXEC_ACTIVATE_SECONDARY_CONTROLS == 0 {
        return Err(Error::Capability(
            "secondary VMX controls",
            primary_capability,
        ));
    }
    // SAFETY: the primary control MSR advertises secondary controls, which
    // establishes the secondary capability MSR's presence on this CPU.
    let secondary_capability = unsafe { cpu::rdmsr(vmx::IA32_VMX_PROCBASED_CTLS2) };
    if (secondary_capability >> 32) as u32 & vmcs::SECONDARY_EXEC_ENABLE_EPT == 0 {
        return Err(Error::Capability(
            "EPT execution control",
            secondary_capability,
        ));
    }
    // SAFETY: secondary controls advertise EPT, which establishes the EPT/VPID
    // capability MSR's presence; this pre-launch check still executes at CPL0.
    let ept_capability = unsafe { cpu::rdmsr(vmx::IA32_VMX_EPT_VPID_CAP) };
    if ept_capability & ept::REQUIRED_EPT_CAPS != ept::REQUIRED_EPT_CAPS {
        return Err(Error::Capability("EPT", ept_capability));
    }
    if restrict_vmx_capability(vmx::IA32_VMX_EPT_VPID_CAP, ept_capability).is_none() {
        return Err(Error::Capability("nested EPT/VPID", ept_capability));
    }

    let _ = writeln!(
        serial,
        "thin-hv: runtime image base={image_base:#018x} end={image_end:#018x}"
    );

    // SAFETY: the caller established CPUID.VMX and this application is at CPL0,
    // where the architectural VMX feature-control MSR can be read.
    let feature_control = unsafe { cpu::rdmsr(cpu::IA32_FEATURE_CONTROL) };
    if feature_control & 1 != 0 && feature_control & (1 << 2) == 0 {
        return Err(Error::Capability("VMX outside SMX", feature_control));
    }

    let physical_bits = max_physical_address_bits()
        .ok_or(Error::Capability("monitor physical-address width", 0))?;
    let allocation_limit =
        (1_u64 << physical_bits)
            .min(IDENTITY_MAP_LIMIT)
            .min(if basic.physical_address_width_32 {
                1 << 32
            } else {
                u64::MAX
            });
    let allocation = handoff.monitor_allocation;
    let guarded =
        resident_image::GuardedAllocation::new(allocation, MONITOR_PAGES, allocation_limit)
            .map_err(Error::Resident)?;
    let block = guarded.payload;
    let block_end = guarded.payload_end;
    let ram = validate_monitor_allocation(system_table, allocation)?;
    if !ram.allows_ram_access(handoff.bootstrap.entry, 1, false)
        || !ram.allows_ram_access(handoff.bootstrap.marker, 8, true)
        || !ram.allows_ram_access(handoff.bootstrap.status, 8, true)
    {
        return Err(Error::Firmware(
            "L1 bootstrap RAM ownership",
            efi::Status::UNSUPPORTED.as_usize(),
        ));
    }
    // Resolve fallible typed addresses before changing CR0/CR4. Any rejection
    // therefore returns through allocation cleanup without a CPU-state restore.
    let vmxon = VmxonPhys::new(block).ok_or(Error::Firmware(
        "VMXON page address",
        efi::Status::COMPROMISED_DATA.as_usize(),
    ))?;
    let vmcs = VmcsPhys::new(block + PAGE_SIZE).ok_or(Error::Firmware(
        "VMCS page address",
        efi::Status::COMPROMISED_DATA.as_usize(),
    ))?;
    for address in [
        handoff.bootstrap.entry,
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

    // SAFETY: AllocatePages returned this exclusive runtime allocation; its
    // nonzero base, page alignment and complete byte range were checked above.
    unsafe {
        ptr::write_bytes(
            allocation as *mut u8,
            0,
            MONITOR_ALLOCATION_PAGES * PAGE_SIZE as usize,
        )
    };
    // SAFETY: the checked allocation contains disjoint VMXON, VMCS, invalid
    // revision and MSR-bitmap pages. No CPU uses them yet; all writes remain
    // inside their pages. The extra page's low revision bit is inverted, so
    // it can never become a current hardware VMCS, even with shadowing.
    unsafe {
        ptr::write_volatile(block as *mut u32, basic.revision_id);
        ptr::write_volatile((block + PAGE_SIZE) as *mut u32, basic.revision_id);
        ptr::write_volatile(
            (block + ERROR_REVISION_PAGE * PAGE_SIZE) as *mut u32,
            basic.revision_id ^ 1,
        );
        initialize_l1_msr_bitmap(block + MSR_BITMAP_PAGE * PAGE_SIZE);
    }

    let maps = build_carrier_maps(system_table, block, (image_base, image_end), &ram, serial)?;

    let original_cr0 = cpu::read_cr0();
    let original_cr4 = cpu::read_cr4();
    if original_cr4 & CR4_LA57 != 0 {
        return Err(Error::Capability("CR4.LA57", original_cr4));
    }
    if original_cr4 & CR4_CET != 0 {
        return Err(Error::Capability(
            "CR4.CET host shadow stacks",
            original_cr4,
        ));
    }
    let host_stack = HostStack::new(block + HOST_STACK_PAGE * PAGE_SIZE, 4 * PAGE_SIZE)
        .map_err(Error::HostState)?;
    // SAFETY: these exclusive host-environment pages lie inside the checked
    // runtime block, before the disjoint invalid-revision page.
    // The ordinary host stack is disjoint and also runtime-owned. HOST_CR3 maps
    // the entire block supervisor-writable and the retained PE executable. No
    // CPU/VMCS uses this storage yet; CR4.CET/LA57 were rejected above. After a
    // successful entry all terminal paths retain the pages and never return to
    // firmware. Immediate VMfail does not install these host descriptor fields.
    let mut host_environment = unsafe {
        HostEnvironment::initialize(
            core::slice::from_raw_parts_mut(
                (block + HOST_ENVIRONMENT_FIRST_PAGE * PAGE_SIZE) as *mut u8,
                host_state::HOST_ENVIRONMENT_BYTES,
            ),
            host_stack,
            48,
        )
    }
    .map_err(Error::HostState)?;
    let monitor = ptr::NonNull::new((block + CPU_STATE_FIRST_PAGE * PAGE_SIZE) as *mut CpuMonitor)
        .ok_or(Error::Firmware(
            "CPU runtime state address",
            efi::Status::COMPROMISED_DATA.as_usize(),
        ))?;
    // SAFETY: the same physical CPU remains at CPL0 in firmware preparation.
    // CPUID.VMX and VMX capability MSRs were checked before reaching this path.
    // The validated firmware map supplies its physical width. No guest has run;
    // this immutable snapshot belongs only to this CPU's newly allocated state.
    let host_limits = unsafe { capture_host_validation_limits(ram.physical_width().bits()) };
    // SAFETY: the checked runtime allocation includes this disjoint,
    // page-aligned final arena with enough space for the complete lock and
    // state. No CPU or VMCS references it yet. The object never moves; all
    // post-entry terminal paths retain its pages and private HOST_CR3 map.
    unsafe {
        monitor.as_ptr().write(CpuMonitor::new(
            CpuRuntimeState::new(
                ram,
                basic,
                [(block, block_end), (image_base, image_end)],
                maps.mmio,
                Some(maps.window),
                Some(handoff.bootstrap),
            ),
            host_limits,
        ));
        host_environment.bind_monitor_data(monitor.cast());
    }
    // SAFETY: the complete CpuMonitor was initialized in its final allocation
    // above. Only a shared reference to its short diagnostic lock is borrowed;
    // it cannot move or outlive the reserved block on this preparation path.
    publish_diagnostics(unsafe { monitor.as_ref() }, block, block_end, serial)?;
    for address in host_environment.required_image_addresses() {
        if address < image_base || address >= image_end {
            return Err(Error::OutsideIdentityMap(address));
        }
    }
    let _ = writeln!(serial, "thin-hv: private host state PASS");
    // SAFETY: CPUID.VMX establishes these four architectural fixed-bit MSRs;
    // all reads occur at CPL0 before control-register changes or VMXON.
    let (cr0_fixed0, cr0_fixed1, cr4_fixed0, cr4_fixed1) = unsafe {
        (
            cpu::rdmsr(vmx::IA32_VMX_CR0_FIXED0),
            cpu::rdmsr(vmx::IA32_VMX_CR0_FIXED1),
            cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED0),
            cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED1),
        )
    };
    let fixed_cr0 = (original_cr0 | cr0_fixed0) & cr0_fixed1;
    let fixed_cr4 = (original_cr4 | (1 << 13) | cr4_fixed0) & cr4_fixed1;
    if cpu::cpuid(1, 0).ecx & (1 << 26) == 0 {
        return Err(Error::Capability("XSAVE", 0));
    }
    let legacy_features = (1 << 24) | (1 << 25) | (1 << 26);
    if cpu::cpuid(1, 0).edx & legacy_features != legacy_features
        || fixed_cr0 & ((1 << 2) | (1 << 3)) != 0
    {
        return Err(Error::Capability("UEFI x87/SSE environment", fixed_cr0));
    }
    let entry_cr4 = fixed_cr4 | CR4_OSXSAVE;
    // No guest protection-key register is changed. Private supervisor maps
    // must not inherit L1's PKRS restrictions; OSFXSR makes FXSAVE64 usable
    // even when L1 has cleared its own OSFXSR or XCR0.SSE.
    let host_cr4 = (entry_cr4 | (1 << 9)) & !((1 << 22) | (1 << 24));
    if host_cr4 & cr4_fixed0 != cr4_fixed0 || host_cr4 & !cr4_fixed1 != 0 {
        return Err(Error::Capability("private host CR4", host_cr4));
    }
    if feature_control & 1 == 0 {
        // SAFETY: this BSP read unlocked FEATURE_CONTROL at CPL0. All
        // platform-map/allocation/host-state validation has now succeeded;
        // initialize this one-way VMX permission only immediately before use.
        unsafe { cpu::wrmsr(cpu::IA32_FEATURE_CONTROL, feature_control | 0b101) };
    }
    // SAFETY: the original controls were saved and normalized with the VMX
    // fixed-bit MSRs; CPUID advertised XSAVE before OSXSAVE is enabled. No
    // recoverable fallible operation intervenes before VMXON's restore path.
    unsafe {
        cpu::write_cr0(fixed_cr0);
        cpu::write_cr4(entry_cr4);
    }
    // SAFETY: CPUID advertised XSAVE and host CR4.OSXSAVE is now set. XCR0 is
    // restored before the original CR4 is restored.
    let original_xcr0 = unsafe { cpu::xgetbv(0) };
    let original = FirmwareControls {
        cr0: original_cr0,
        cr4: original_cr4,
        xcr0: original_xcr0,
    };

    // SAFETY: the checked, aligned runtime VMXON page contains this CPU's
    // revision ID. FEATURE_CONTROL permits VMX outside SMX and CR0/CR4 have
    // been normalized; failure restores the saved controls before cleanup.
    let vmxon_status = unsafe { vmx::vmxon(vmxon) };
    if vmxon_status != VmxStatus::Success {
        original.restore();
        return Err(Error::Instruction("VMXON", vmxon_status, u64::MAX));
    }
    #[cfg(feature = "host-xstate-test")]
    serial.write_bytes(b"thin-hv: host xstate clobber fixture armed\n");

    let result = configure_and_launch(
        vmcs,
        maps.eptp,
        block + MSR_BITMAP_PAGE * PAGE_SIZE,
        fixed_cr0,
        fixed_cr4,
        original_cr4,
        host_cr4,
        maps.host_cr3,
        maps.host_pat,
        &host_environment,
        handoff.bootstrap.entry,
        block + (GUEST_STACK_PAGE + GUEST_STACK_PAGES) * PAGE_SIZE - 8,
        basic.true_controls,
    );

    // This is reached only when VM entry failed.
    leave_failed_launch(&original, serial);
    result
}

/// A failed VMXOFF cannot authorize freeing a live VMXON/VMCS allocation.
fn leave_failed_launch(original: &FirmwareControls, serial: &mut SerialPort) {
    let status = leave_vmx();
    if status != VmxStatus::Success {
        let _ = writeln!(
            serial,
            "thin-hv: VMXOFF status={status:?} FAIL: retaining monitor storage"
        );
        loop {
            core::hint::spin_loop();
        }
    }
    original.restore();
}

/// Loads the staged test/Linux image, or Windows from another filesystem.
#[cfg(not(feature = "physical-direct-vmx"))]
fn load_selected_guest(
    parent_image: efi::Handle,
    parent_device: efi::Handle,
    system_table: *mut efi::SystemTable,
    utilities: *mut efi::protocols::device_path_utilities::Protocol,
) -> Result<(efi::Handle, ProfileId), Error> {
    match load_image_on_device(
        parent_image,
        system_table,
        parent_device,
        utilities,
        &GUEST_IMAGE_PATH,
    ) {
        Ok(image) => return Ok((image, LINUX_PROFILE)),
        Err(error) if error.is_missing_image() => {}
        Err(error) => return Err(error.into()),
    }

    let windows = match load_image_on_device(
        parent_image,
        system_table,
        parent_device,
        utilities,
        &WINDOWS_BOOT_IMAGE_PATH,
    ) {
        Ok(image) => image,
        Err(error) if error.is_missing_image() => load_image_from_other_filesystem(
            parent_image,
            parent_device,
            system_table,
            utilities,
            &WINDOWS_BOOT_IMAGE_PATH,
        )?,
        Err(error) => return Err(error.into()),
    };
    Ok((windows, WINDOWS_PROFILE))
}

/// Starts a runtime-driver copy whose code survives guest ExitBootServices.
fn start_runtime_monitor(
    parent_image: efi::Handle,
    parent_device: efi::Handle,
    system_table: *mut efi::SystemTable,
) -> Result<(), Error> {
    let utilities = device_path_utilities_protocol(system_table)?;
    #[cfg(not(feature = "physical-direct-vmx"))]
    let (guest, profile) =
        load_selected_guest(parent_image, parent_device, system_table, utilities)?;
    #[cfg(feature = "physical-direct-vmx")]
    let (guest, profile) = (
        crate::physical_chainload::load_selected(parent_image, system_table, &mut SerialPort)
            .map_err(Error::Platform)?,
        ProfileId(0),
    );
    let services = chainload::boot_services(system_table)?;
    let mut monitor_retired = false;
    let result = (|| {
        let monitor = load_image_on_device(
            parent_image,
            system_table,
            parent_device,
            utilities,
            &MONITOR_IMAGE_PATH,
        )?;
        let metadata = (|| {
            let loaded = loaded_image_protocol(monitor, system_table)?;
            // SAFETY: this non-null live LoadedImage protocol belongs to the
            // unstarted monitor. Copy memory types before publishing any handoff.
            let (code, data) = unsafe { ((*loaded).image_code_type, (*loaded).image_data_type) };
            if !runtime_image_types(code, data) {
                return Err(Error::Firmware(
                    "runtime monitor image types",
                    efi::Status::UNSUPPORTED.as_usize(),
                ));
            }
            Ok(loaded)
        })();
        let monitor_loaded = match metadata {
            Ok(loaded) => loaded,
            Err(error) => {
                chainload::unload_image(services, monitor)?;
                monitor_retired = true;
                return Err(error);
            }
        };
        let mut guest_handoff = RuntimeHandoff {
            guest,
            profile: profile.0,
            mode: RUNTIME_MODE,
        };
        // SAFETY: this driver has not started. The caller owns its load-option
        // metadata and keeps this aligned handoff alive throughout StartImage and
        // post-return cleanup. Preserve firmware's previous borrowed options.
        let original_options = unsafe {
            let original = (
                (*monitor_loaded).load_options_size,
                (*monitor_loaded).load_options,
            );
            (*monitor_loaded).load_options_size = core::mem::size_of::<RuntimeHandoff>() as u32;
            (*monitor_loaded).load_options = ptr::addr_of_mut!(guest_handoff).cast();
            original
        };
        let started = chainload::start_image(monitor, system_table);
        // A driver that returns an error is unloaded by Exit; a successful
        // driver, or one rejected before entry, may still be registered. Never
        // dereference the pre-StartImage protocol or blindly unload its handle.
        match loaded_image_protocol(monitor, system_table) {
            Ok(live) => {
                // SAFETY: a new firmware query proved this protocol is live.
                // The project driver cannot return after launching L1. Thus no
                // guest owns it here, and the stack handoff is still in scope.
                unsafe {
                    (*live).load_options_size = original_options.0;
                    (*live).load_options = original_options.1;
                }
                chainload::unload_image(services, monitor)?;
            }
            Err(chainload::Error::Firmware("HandleProtocol(LoadedImage)", status))
                if status == efi::Status::INVALID_PARAMETER.as_usize()
                    || status == efi::Status::UNSUPPORTED.as_usize() => {}
            Err(error) => return Err(error.into()),
        }
        monitor_retired = true;
        started?;
        // A normal Direct launch never returns. Even a warning/success from an
        // unexpected returning runtime driver is not proof of resident VMX.
        Err(Error::Firmware(
            "runtime monitor unexpectedly returned",
            efi::Status::ABORTED.as_usize(),
        ))
    })();
    // This code is reachable only before L1 entry. The guest's LoadImage handle
    // has never been started; attempt its cleanup even if monitor cleanup failed.
    chainload::unload_image(services, guest)?;
    if monitor_retired {
        SerialPort.write_bytes(
            b"thin-hv: runtime handoff cleanup PASS guest=unstarted monitor=retired\n",
        );
    }
    result
}

/// Reject a boot application masquerading as the runtime driver before it can
/// recursively load another monitor or publish disposable Boot Services state.
fn runtime_image_types(code: u32, data: u32) -> bool {
    code == efi::RUNTIME_SERVICES_CODE && data == efi::RUNTIME_SERVICES_DATA
}

/// Reads the target handle and profile supplied by the boot application copy.
fn runtime_handoff(
    loaded_image: *mut efi::protocols::loaded_image::Protocol,
) -> Option<(efi::Handle, ProfileId)> {
    // SAFETY: run obtained and checked this live, non-null LoadedImage protocol
    // before selecting the runtime path. Copy metadata before dereferencing the
    // caller-owned handoff, which remains live for the complete StartImage call.
    let (size, options) = unsafe {
        (
            (*loaded_image).load_options_size as usize,
            (*loaded_image).load_options,
        )
    };
    if size != core::mem::size_of::<RuntimeHandoff>()
        || options.is_null()
        || (options as usize).checked_add(size).is_none()
    {
        return None;
    }
    // SAFETY: the application copy keeps this fixed handoff live for the
    // complete nested StartImage call.
    let handoff = unsafe { ptr::read_unaligned(options.cast::<RuntimeHandoff>()) };
    validated_runtime_handoff(handoff)
}

/// Reject mixed physical/research components before any allocation or VMXON.
fn validated_runtime_handoff(handoff: RuntimeHandoff) -> Option<(efi::Handle, ProfileId)> {
    let profile = ProfileId(handoff.profile);
    (!handoff.guest.is_null() && handoff.mode == RUNTIME_MODE && valid_runtime_profile(profile))
        .then_some((handoff.guest, profile))
}

/// Physical handoffs carry no variable profile and cannot consume research mode.
fn valid_runtime_profile(profile: ProfileId) -> bool {
    #[cfg(feature = "physical-direct-vmx")]
    {
        profile.0 == 0
    }
    #[cfg(not(feature = "physical-direct-vmx"))]
    {
        matches!(profile, WINDOWS_PROFILE | LINUX_PROFILE)
    }
}

/// Fully validated roots and CPU-owned access metadata, without retained borrows
/// into temporary firmware buffers or the now hardware-owned paging arenas.
struct CarrierMaps {
    eptp: u64,
    host_cr3: u64,
    host_pat: u64,
    window: ept::HostWindow,
    mmio: MmioMap,
}

/// No root is published until both complete platform maps and cleanup succeed.
fn build_carrier_maps(
    system_table: *mut efi::SystemTable,
    block: u64,
    image: (u64, u64),
    ram: &FirmwareMap<205>,
    serial: &mut SerialPort,
) -> Result<CarrierMaps, Error> {
    platform_snapshot::with_snapshot(system_table, serial, |cpu, map, serial| {
        let reject = |stage: &'static str| chainload::Error::Firmware(stage, efi::Status::UNSUPPORTED.as_usize());
        let width = ram.physical_width();
        if cpu.physical_bits() != width.bits() {
            return Err(reject("carrier CPU/map physical width changed"));
        }
        let tables = platform_acpi::Tables::from_system_table(system_table, map)?;
        let mmio = platform_resources::platform_mmio(system_table, map, width, tables.as_ref(), serial)?;
        let mtrrs = cpu.mtrrs().map_err(|_| reject("carrier MTRR state"))?
            .ok_or_else(|| reject("carrier MTRRs unavailable"))?;
        // Every VMX/host/EPT page must really be WB, not merely advertise the
        // firmware WB cache capability or fall inside a q35 address bucket.
        for page in 0..MONITOR_PAGES {
            if mtrrs.memory_type(block + page as u64 * PAGE_SIZE)
                .map_err(|_| reject("carrier monitor MTRR conflict"))?
                != platform_memory::MemoryType::WriteBack {
                return Err(reject("carrier monitor memory is not WB"));
            }
        }
        let capabilities = PageCapabilities::from_vmx_capability(cpu.ept_caps()
            .ok_or_else(|| reject("carrier EPT unavailable"))?)
            .map_err(|_| reject("carrier EPT page capabilities"))?;
        let base = block + EPT_FIRST_PAGE * PAGE_SIZE;
        let host_base = block + HOST_TABLE_FIRST_PAGE * PAGE_SIZE;
        let private = [
            PhysicalRange::new(block, block + GUEST_STACK_PAGE * PAGE_SIZE, width)
                .map_err(|_| reject("carrier private prefix"))?,
            PhysicalRange::new(host_base, block + MONITOR_PAGES as u64 * PAGE_SIZE, width)
                .map_err(|_| reject("carrier private suffix"))?,
            PhysicalRange::new(image.0, image.1, width)
                .map_err(|_| reject("carrier private image"))?,
        ];
        // This post-AllocatePages map includes the complete retained monitor.
        // Intervening discovery calls allocate/free temporary RAM pools only;
        // they do not change physical RAM extents or cache attributes.
        let plan = PlatformMap::new(ram.descriptors(), &private, mmio.ranges(), mtrrs, capabilities)
            .map_err(|_| reject("carrier platform map"))?;
        let physical = EptPhys::new(base).ok_or_else(|| reject("carrier EPT address"))?;
        // SAFETY: the checked exclusive, zero-initialized monitor block contains
        // these EPT_PAGES contiguous aligned pages, disjoint from VMXON/VMCS,
        // stacks and Rust state. No VMCS is live yet; the hardware retains this
        // backing until terminal VMX teardown, never a returning firmware path.
        let pages = unsafe { core::slice::from_raw_parts_mut(base as *mut ept::EptPage, EPT_PAGES) };
        let built = ept::build_platform_identity(&plan, pages, physical).map_err(|error| {
            let _ = writeln!(serial, "thin-hv: direct platform EPT FAIL error={error:?}");
            reject("carrier platform EPT construction")
        })?;
        let host_policy = cpu.host_paging()?;
        let host_physical = HostPhys::new(host_base).ok_or_else(|| reject("carrier host address"))?;
        // SAFETY: this disjoint page-aligned arena is exclusively allocated WB
        // runtime RAM. No CPU uses either root yet. The builder's borrows end
        // before any hardware walk or scratch-PTE update, and all backing stays
        // resident after successful launch, including terminal error paths.
        let host_pages = unsafe { core::slice::from_raw_parts_mut(host_base as *mut ept::EptPage, HOST_TABLE_PAGES) };
        let host = ept::build_host_identity(&plan, host_pages, host_physical, host_policy)
            .map_err(|error| {
                let _ = writeln!(serial, "thin-hv: direct platform HOST FAIL error={error:?}");
                reject("carrier platform host construction")
            })?;
        let _ = writeln!(serial,
            "thin-hv: direct platform EPT PASS source=uefi+mtrr+gcd+acpi+pci tables={} leaves={} private_pages={} host_map=platform-ram bootstrap=firmware-runtime l0_image=private-copy physical_ready=0",
            built.table_pages(), built.leaf_count(), MONITOR_PAGES as u64 - GUEST_STACK_PAGES + (image.1 - image.0) / PAGE_SIZE);
        let _ = writeln!(serial,
            "thin-hv: direct platform HOST PASS tables={} leaves={} private_pages={} mmio_window=uc physical_ready=0",
            host.table_pages(), host.leaf_count(), HOST_TABLE_PAGES);
        Ok(CarrierMaps { eptp: built.eptp(), host_cr3: host.cr3(), host_pat: host.pat(), window: host.window(), mmio })
    }).map_err(Error::Platform)
}

/// Checks the live runtime allocation against a bounded firmware memory map.
fn validate_monitor_allocation(
    system_table: *mut efi::SystemTable,
    base: u64,
) -> Result<FirmwareMap<205>, Error> {
    let mut storage = [0_u64; 1024];
    let mut length = core::mem::size_of_val(&storage);
    let mut key = 0;
    let mut stride = 0;
    let mut version = 0;
    // SAFETY: launch holds the live firmware system table before ExitBootServices.
    // The aligned, initialized stack buffer and every scalar output remain valid
    // for the synchronous service call; no firmware state is changed.
    let status = unsafe {
        ((*(*system_table).boot_services).get_memory_map)(
            &mut length,
            storage.as_mut_ptr().cast(),
            &mut key,
            &mut stride,
            &mut version,
        )
    };
    if status.is_error() {
        return Err(Error::Firmware("monitor GetMemoryMap", status.as_usize()));
    }
    if length > core::mem::size_of_val(&storage) {
        return Err(Error::Firmware(
            "monitor memory-map size",
            efi::Status::COMPROMISED_DATA.as_usize(),
        ));
    }
    // SAFETY: the bounded length is inside the initialized storage array; the
    // byte slice is consumed locally and never survives this stack allocation.
    let bytes = unsafe { core::slice::from_raw_parts(storage.as_ptr().cast(), length) };
    let mut regions = [FirmwareDescriptor::default(); 205];
    let count =
        platform_memory::decode_uefi_map(bytes, stride, version, &mut regions).map_err(|_| {
            Error::Firmware(
                "monitor memory-map layout",
                efi::Status::COMPROMISED_DATA.as_usize(),
            )
        })?;
    if !monitor_allocation_is_wb(&regions[..count], base) {
        return Err(Error::Firmware(
            "monitor allocation requires unique writable WB runtime RAM",
            efi::Status::UNSUPPORTED.as_usize(),
        ));
    }
    let width = max_physical_address_bits()
        .and_then(|bits| PhysicalWidth::new(bits).ok())
        .ok_or(Error::Capability("RAM map physical width", 0))?;
    FirmwareMap::new(&regions[..count], width).map_err(|_| {
        Error::Firmware(
            "unsupported MSR RAM map",
            efi::Status::UNSUPPORTED.as_usize(),
        )
    })
}

/// Requires unique writable runtime RAM with WB capability; the carrier builder
/// separately checks effective MTRRs before publishing either platform root.
fn monitor_allocation_is_wb(regions: &[FirmwareDescriptor], base: u64) -> bool {
    let Some(end) = base.checked_add(MONITOR_ALLOCATION_PAGES as u64 * PAGE_SIZE) else {
        return false;
    };
    if base == 0 || !base.is_multiple_of(PAGE_SIZE) || end > IDENTITY_MAP_LIMIT {
        return false;
    }
    let mut covered = [0_u8; MONITOR_ALLOCATION_PAGES];
    for region in regions {
        let Some(region_end) = region
            .number_of_pages
            .checked_mul(PAGE_SIZE)
            .and_then(|bytes| region.physical_start.checked_add(bytes))
        else {
            return false;
        };
        if !region.physical_start.is_multiple_of(PAGE_SIZE) || region.number_of_pages == 0 {
            return false;
        }
        if region.physical_start >= end || region_end <= base {
            continue;
        }
        if region.memory_type != efi::RUNTIME_SERVICES_DATA
            || region.attributes & efi::MEMORY_WB == 0
            || region.attributes & (efi::MEMORY_RP | efi::MEMORY_WP | efi::MEMORY_RO) != 0
        {
            return false;
        }
        for (index, count) in covered.iter_mut().enumerate() {
            let page = base + index as u64 * PAGE_SIZE;
            if region.physical_start <= page && page < region_end {
                *count = count.saturating_add(1);
            }
        }
    }
    covered.iter().all(|&count| count == 1)
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
    host_pat: u64,
    host_environment: &HostEnvironment<'_>,
    guest_rip: u64,
    guest_rsp: u64,
    true_controls: bool,
) -> Result<(), Error> {
    // SAFETY: this CPU owns the checked WB carrier region and is in VMX root.
    // Firmware GS is still active: preparation uses raw HAL operations, never
    // the post-exit CPU-local diagnostic wrappers.
    unsafe {
        require("VMCLEAR", vmx::vmclear(vmcs_page))?;
        require("VMPTRLD", vmx::vmptrld(vmcs_page))?;
    }

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
    // ponytail: this CPU grants the complete nonzero namespace to one trusted
    // L1. Add tag remapping before a tagged carrier or second L1 shares it.
    if !VpidNamespace::carrier_supported(secondary) {
        return Err(Error::Capability(
            "VPID-tagged carrier",
            u64::from(secondary),
        ));
    }
    // L0 always restores private PAT/EFER. Direct entry explicitly reconstructs
    // L1's inherited values; neither guest may supply L0's execution environment.
    let exit_capability = unsafe { cpu::rdmsr(exit_msr) };
    let exit = vmx::adjust_controls(
        vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE
            | vmcs::VM_EXIT_SAVE_IA32_PAT
            | vmcs::VM_EXIT_SAVE_IA32_EFER
            | vmcs::VM_EXIT_LOAD_IA32_PAT
            | vmcs::VM_EXIT_LOAD_IA32_EFER,
        exit_capability,
    );
    let entry = vmx::adjust_controls(
        vmcs::VM_ENTRY_IA32E_MODE | vmcs::VM_ENTRY_LOAD_IA32_PAT | vmcs::VM_ENTRY_LOAD_IA32_EFER,
        unsafe { cpu::rdmsr(entry_msr) },
    );
    let required_exit_msrs = vmcs::VM_EXIT_SAVE_IA32_PAT
        | vmcs::VM_EXIT_SAVE_IA32_EFER
        | vmcs::VM_EXIT_LOAD_IA32_PAT
        | vmcs::VM_EXIT_LOAD_IA32_EFER;
    let required_entry_msrs = vmcs::VM_ENTRY_LOAD_IA32_PAT | vmcs::VM_ENTRY_LOAD_IA32_EFER;
    if exit & required_exit_msrs != required_exit_msrs
        || entry & required_entry_msrs != required_entry_msrs
    {
        return Err(Error::Capability(
            "private PAT/EFER controls",
            u64::from(exit),
        ));
    }
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

    write_guest_state(host_cr0, guest_cr4_hardware, guest_rsp, guest_rip)?;
    write_host_state(host_cr0, host_cr3, host_cr4, host_pat, host_environment)?;
    log_guest_state();

    // CpuMonitor::new already initialized this CPU's nested state before its
    // private GS pointer was bound. Do not access that pointer before VM exit.
    // SAFETY: all carrier fields and backing were prepared on this CPU in VMX
    // root. On immediate VMfail no host state was loaded; return for cleanup.
    let launch = unsafe { vmx::vmlaunch() };
    Err(Error::Instruction(
        "VMLAUNCH",
        launch,
        initial_vm_instruction_error(),
    ))
}

/// Writes current long-mode state as the initial guest state.
fn write_guest_state(cr0: u64, cr4: u64, stack: u64, entry: u64) -> Result<(), Error> {
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
        (vmcs::GUEST_RIP, entry),
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
fn write_host_state(
    cr0: u64,
    cr3: u64,
    cr4: u64,
    pat: u64,
    environment: &HostEnvironment<'_>,
) -> Result<(), Error> {
    // The same owned fields are cached by the Direct-VMCS patch manifest, so
    // both carrier exits and direct L2 exits enter the private environment.
    for (field, value) in environment.vmcs_fields() {
        write_vmcs(field, value)?;
    }
    // SAFETY: UEFI entered on an x86-64 CPU with architectural PAT/EFER support;
    // these read-only captures run at CPL0 before the first VM entry.
    let efer = unsafe { cpu::rdmsr(cpu::IA32_EFER) };
    for (field, value) in [
        (vmcs::HOST_CR0, cr0),
        (vmcs::HOST_CR3, cr3),
        (vmcs::HOST_CR4, cr4),
        (vmcs::HOST_IA32_PAT, pat),
        (vmcs::HOST_IA32_EFER, efer),
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
    // SAFETY: only initial carrier preparation calls this helper. This CPU
    // owns the current VMCS in VMX root, but firmware GS is still installed.
    let status = unsafe { vmx::vmwrite(field, value) };
    if status == VmxStatus::Success {
        Ok(())
    } else {
        Err(Error::Vmwrite(
            field,
            status,
            initial_vm_instruction_error(),
        ))
    }
}

fn require(instruction: &'static str, status: VmxStatus) -> Result<(), Error> {
    if status == VmxStatus::Success {
        Ok(())
    } else {
        Err(Error::Instruction(
            instruction,
            status,
            initial_vm_instruction_error(),
        ))
    }
}

fn vm_instruction_error() -> u64 {
    unsafe { vmcs_read(vmcs::VM_INSTRUCTION_ERROR) }.unwrap_or(u64::MAX)
}

/// Initial entry failure may have no current VMCS and has no private GS yet.
fn initial_vm_instruction_error() -> u64 {
    // SAFETY: preparation still runs in VMX root on the owning CPU; an absent
    // current VMCS returns VMfailInvalid instead of causing a host exception.
    unsafe { vmx::vmread(vmcs::VM_INSTRUCTION_ERROR) }.unwrap_or(u64::MAX)
}

fn log_guest_state() {
    // SAFETY: initial preparation owns the current carrier, before any guest
    // entry. Use the raw HAL because private GS is not installed until exit.
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

/// First non-root instruction stream.
extern "C" fn guest_entry() -> ! {
    GUEST_RAN.store(GUEST_MARKER, Ordering::Release);
    let system_table = SYSTEM_TABLE.load(Ordering::Acquire);
    let guest_image = GUEST_IMAGE.load(Ordering::Acquire);
    // The shared helper releases ExitData on both successful and failed returns.
    // The guest executes this bootstrap while Boot Services remain available.
    let status =
        chainload::start_image(guest_image, system_table).unwrap_or_else(chainload::Error::status);
    GUEST_STATUS.store(status.as_usize(), Ordering::Release);
    // SAFETY: this guest runs specifically under the VMCALL smoke handler.
    unsafe { vmx::vmcall() };
    loop {
        core::hint::spin_loop();
    }
}

/// Hardware VM-exit target for the smoke VMCS.
// SAFETY: hardware enters at CPL0 with private HOST_CR0.EM/TS=0, OSFXSR=1,
// HOST_GS_BASE pointing at this CPU's aligned, exclusively owned HostXstate,
// and the reserved host stack. GS scratch remains live on immediate VMfail.
// Every Rust call is bracketed by FXSAVE64/FXRSTOR64; no guest XCR0/XSS change
// is made here. FNINIT and private MXCSR mask guest FP exceptions inside L0.
// Restoring the live exit state on L2 reflection preserves VMX's *shared*
// extended-state semantics, rather than restoring a stale L1 snapshot.
#[unsafe(naked)]
extern "sysv64" fn vmexit_entry() -> ! {
    core::arch::naked_asm!(
        "fxsave64 gs:[0]",
        "fninit",
        "ldmxcsr gs:[{mxcsr}]",
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
        "fxrstor64 gs:[0]",
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
        "fxsave64 gs:[0]",
        "fninit",
        "ldmxcsr gs:[{mxcsr}]",
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
        "fxsave64 gs:[0]",
        "fninit",
        "ldmxcsr gs:[{mxcsr}]",
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
        "fxrstor64 gs:[0]",
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
        mxcsr = const host_state::HOST_XSTATE_MXCSR_OFFSET,
    );
}

/// Handles one VM exit and returns only when the guest can be resumed.
unsafe extern "sysv64" fn vmexit_dispatch(registers: *mut GuestRegisters) -> u64 {
    #[cfg(feature = "host-xstate-test")]
    clobber_host_xmm();
    // SAFETY: `vmexit_entry` passes its live, uniquely owned stack frame.
    let registers = unsafe { &mut *registers };
    let nested_run = current_cpu().nested_run.lock().take();
    if let Some(run) = nested_run {
        // SAFETY: a hardware exit entered L0 with the direct VMCS current; this
        // read-only telemetry access neither changes fields nor emulation policy.
        let reason = unsafe { vmcs_read(vmcs::VM_EXIT_REASON) }.unwrap_or(u64::MAX);
        record_diagnostic(DiagnosticEvent::NestedExit(reason));
        reflect_l2_vmexit(&run, reason, registers);
        record_diagnostic(DiagnosticEvent::Reflected(reason));
        return VMEXIT_ACTION_RESUME;
    }

    // SAFETY: the hardware exit selected the live carrier VMCS for this BSP.
    let reason = unsafe { vmcs_read(vmcs::VM_EXIT_REASON) }.unwrap_or(u64::MAX);
    complete_reflected_msr_load(reason, registers);
    record_diagnostic(DiagnosticEvent::L1Exit(reason));
    let action = dispatch_l1_exit(registers, reason);
    record_diagnostic(DiagnosticEvent::L0Handled { reason, action });
    action
}

/// Existing L1 emulation, separated only to count successfully handled exits.
fn dispatch_l1_exit(registers: &mut GuestRegisters, reason: u64) -> u64 {
    // Retire queued guest writes before L1 changes VMCS/lifetime ownership,
    // even on a subsequently faulting instruction. Entry uses the already
    // necessary Direct selection to flush instead of adding another round trip.
    if reason & (1 << 31) == 0
        && matches!(
            reason & 0xffff,
            EXIT_REASON_VMCLEAR | EXIT_REASON_VMPTRLD | EXIT_REASON_VMXON | EXIT_REASON_VMXOFF
        )
        && !retire_idle_guest_snapshot()
    {
        stop_unexpected_exit(
            b"retiring direct guest snapshot failed",
            reason,
            0,
            0,
            0,
            registers,
        );
    }
    let qualification = unsafe { vmcs_read(vmcs::EXIT_QUALIFICATION) }.unwrap_or(u64::MAX);
    let guest_rip = unsafe { vmcs_read(vmcs::GUEST_RIP) }.unwrap_or(u64::MAX);
    let instruction_len = unsafe { vmcs_read(vmcs::VM_EXIT_INSTRUCTION_LEN) }.unwrap_or(u64::MAX);

    // VM-entry event fields persist in the VMCS after delivery.
    let clear_event = unsafe { vmcs_write(vmcs::VM_ENTRY_INTR_INFO_FIELD, 0) };
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
        let interruption = unsafe { vmcs_read(vmcs::VM_EXIT_INTR_INFO) }.unwrap_or(0);
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
                && (unsafe { vmcs_write(vmcs::VM_ENTRY_INTR_INFO_FIELD, interruption) })
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
        #[cfg(feature = "host-xstate-test")]
        if cpuid_exit_count() == 1 && !probe_q35_host_window() {
            stop_unexpected_exit(
                b"QEMU host MMIO window probe failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }
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
        if leaf == 1 || (leaf == 7 && subleaf == 0) {
            let Some(cr4) = l1_visible_cr4() else {
                stop_unexpected_exit(
                    b"reading CPUID virtual CR4 failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            };
            result = if leaf == 1 {
                xstate::leaf1_for_cr4(result, cr4)
            } else {
                xstate::leaf7_for_cr4(result, cr4)
            };
        }
        if leaf == 1 {
            result.ecx |= 1 << 5;
            result.ecx &= !(1 << 31);
        }
        // Leaf 0D's enabled-area sizes depend on XCR0 and IA32_XSS, not CR4.
        // VMX does not switch either register and this L0 path changes neither
        // while answering CPUID, so the hardware result uses the live guest
        // values. If L0 gains private XCR0/XSS, synthesize these sizes from the
        // saved guest values instead; never use the private host values.
        registers.rax = u64::from(result.eax);
        registers.rbx = u64::from(result.ebx);
        registers.rcx = u64::from(result.ecx);
        registers.rdx = u64::from(result.edx);

        advance_guest_rip(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }

    if reason & (1 << 31) == 0 && reason & 0xffff == EXIT_REASON_XSETBV {
        let Some(cr4) = l1_visible_cr4() else {
            stop_unexpected_exit(
                b"reading XSETBV virtual CR4 failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        };
        // SAFETY: the current carrier VMCS belongs to this BSP and contains
        // the exiting L1 state. VM86 always has CPL3 regardless of CS bits.
        let privilege = unsafe {
            vmcs_read(vmcs::GUEST_RFLAGS).and_then(|flags| {
                vmcs_read(vmcs::GUEST_CS_SELECTOR).map(|cs| {
                    if flags & (1 << 17) != 0 {
                        3
                    } else {
                        (cs & 3) as u8
                    }
                })
            })
        };
        let Ok(cpl) = privilege else {
            stop_unexpected_exit(
                b"reading XSETBV privilege failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        };
        let value = (u64::from(registers.rdx as u32) << 32) | u64::from(registers.rax as u32);
        let available = cpu::cpuid(1, 0).ecx & xstate::CPUID_XSAVE != 0;
        let capabilities = cpu::cpuid(0xd, 0);
        let supported = (u64::from(capabilities.edx) << 32) | u64::from(capabilities.eax);
        match xstate::validate_xsetbv(available, cr4, cpl, registers.rcx as u32, value, supported) {
            Err(XsetbvFault::InvalidOpcode) => {
                inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
                return VMEXIT_ACTION_RESUME;
            }
            Err(XsetbvFault::GeneralProtection) => {
                inject_general_protection(
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
                return VMEXIT_ACTION_RESUME;
            }
            Ok(()) => {}
        }
        // SAFETY: CPL0 host CR4.OSXSAVE was enabled before VMXON. The same
        // CPU's advertised bitmap, XCR index, guest privilege and every XCR0
        // dependency were validated above; no invalid guest input reaches XSETBV.
        unsafe { cpu::xsetbv(0, value) };
        advance_guest_rip(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
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
        // VMXON creates architectural state on this L1 CPU, even with no
        // current VMCS. Hardware sees L0's forced VMXE=1 and cannot enforce
        // this check after shadowing; only VMXOFF permits L1 to clear VMXE.
        if value & CR4_VMX_ENABLE == 0 && current_cpu().vcpu.lock().in_vmx_operation() {
            inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
            return VMEXIT_ACTION_RESUME;
        }
        let fixed = (value | CR4_VMX_ENABLE | unsafe { cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED0) })
            & unsafe { cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED1) };
        for (field, field_value) in [(vmcs::GUEST_CR4, fixed), (vmcs::CR4_READ_SHADOW, value)] {
            let status = unsafe { vmcs_write(field, field_value) };
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
        record_diagnostic(DiagnosticEvent::L0Handled {
            reason,
            action: VMEXIT_ACTION_RESUME,
        });
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
    let rflags = unsafe { vmcs_read(vmcs::GUEST_RFLAGS) }.ok()?;
    let interruptibility = unsafe { vmcs_read(vmcs::GUEST_INTERRUPTIBILITY_INFO) }.ok()?;
    let activity = unsafe { vmcs_read(vmcs::GUEST_ACTIVITY_STATE) }.ok()?;
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
        let Ok(value) = (unsafe { vmcs_read(field) }) else {
            return false;
        };
        let value = if enabled { value | mask } else { value & !mask };
        if unsafe { vmcs_write(field, value) } != VmxStatus::Success {
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
    #[cfg(feature = "host-xstate-test")]
    if msr == vmx::IA32_VMX_BASIC {
        // SAFETY: capability emulation runs inside the owning CPU's private
        // GS/IDT/IST and saved-XSTATE bracket, with IF clear. This reserved MSR
        // deliberately faults; the next valid capability read must still work.
        // No WRMSR, guest-state change, or hot-path logging is introduced.
        if unsafe { host_state::try_rdmsr(u32::MAX) }.is_some() {
            return None;
        }
    }
    let hardware = match msr {
        vmx::IA32_VMX_VMFUNC | vmx::IA32_VMX_PROCBASED_CTLS3 => 0,
        // SAFETY: this is a read-only VMX capability access in the owning
        // CPU's private GS/IDT/IST, at CPL0 with IF clear. An absent model-
        // specific register returns None and the caller injects L1 #GP(0).
        _ => unsafe { host_state::try_rdmsr(msr) }?,
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
    let cr4_shadow = unsafe { vmcs_read(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmcs_read(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let state = *current_cpu().vcpu.lock();
    if state.in_vmx_operation() {
        complete_vmx_instruction(
            l1_vmx_failure(&state, VMXERR_VMXON_IN_ROOT),
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return;
    }
    let cr0 = unsafe { vmcs_read(vmcs::GUEST_CR0) }.unwrap_or(0);
    if !vmx_control_registers_valid(cr0, cr4_shadow) {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    let Some(region_address) =
        read_l1_vmx_pointer(reason, qualification, guest_rip, instruction_len, registers)
    else {
        return;
    };

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
            if with_cpu_runtime(|runtime| runtime.vpids.acquire(region, invalidate_namespace))
                .is_none_or(|result| result.is_err())
            {
                stop_unexpected_exit(
                    b"acquiring CPU VPID namespace failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            }
            current_cpu().vcpu.lock().record_vmxon_success(region);
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
    let state = *current_cpu().vcpu.lock();
    let cr4_shadow = unsafe { vmcs_read(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmcs_read(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
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
    let released = state.vmxon_region().is_some_and(|owner| {
        with_cpu_runtime(|runtime| runtime.vpids.release(owner, invalidate_namespace))
            .is_some_and(|result| result.is_ok())
    });
    if !released {
        stop_unexpected_exit(
            b"releasing CPU VPID namespace failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    current_cpu().vcpu.lock().record_vmxoff_success();
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
    let state = *current_cpu().vcpu.lock();
    let cr4_shadow = unsafe { vmcs_read(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmcs_read(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    let Some(address) =
        read_l1_vmx_pointer(reason, qualification, guest_rip, instruction_len, registers)
    else {
        return;
    };
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
    let Some(result) = execute_l1_vmx_instruction(&state, || {
        // SAFETY: the aligned, in-range L1-owned VMCS is neither the carrier nor
        // its VMXON page; any retained host patch was materialized above.
        unsafe { vmx::vmclear(region) }
    }) else {
        stop_unexpected_exit(
            b"executing VMCLEAR with L1 VMCS failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    if result == VmInstructionResult::Vmsucceed {
        current_cpu().vcpu.lock().record_vmclear_success(region);
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
    let Some(carrier) = validate_private_vmcs_address(carrier_address) else {
        stop_unexpected_exit(
            b"invalid VMPTRLD carrier",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };

    let state = *current_cpu().vcpu.lock();
    let cr4_shadow = unsafe { vmcs_read(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmcs_read(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    let Some(address) =
        read_l1_vmx_pointer(reason, qualification, guest_rip, instruction_len, registers)
    else {
        return;
    };
    // SAFETY: this CPL0 VM-exit handler runs after successful hardware VMXON;
    // the VMX_BASIC MSR is present and its physical-address restriction is stable.
    let basic = vmx::VmxBasic::from_msr(unsafe { cpu::rdmsr(vmx::IA32_VMX_BASIC) });
    let Some(region) = validate_l1_vmcs_address(address)
        .filter(|_| !basic.physical_address_width_32 || address < 1_u64 << 32)
    else {
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
    // SAFETY: trusted L1 supplies WB VMCS RAM, not MMIO, as required by VMX.
    // The aligned operand's entire page fits the identity map and physical width;
    // reading its architectural header does not access the opaque VMCS body.
    // The monitor's secondary-control capability gate establishes this MSR,
    // and the VM-exit handler runs at CPL0.
    let revision: u32;
    // SAFETY: validate_l1_vmcs_address proved the complete aligned foreign RAM
    // page and excluded L0-owned storage. Integer-address assembly also supports
    // physical zero. The owning L1 is stopped and only the revision word is read.
    unsafe {
        core::arch::asm!("mov {revision:e}, [{address}]", revision = out(reg) revision, address = in(reg) region.get(), options(nostack, preserves_flags));
    }
    // SAFETY: the monitor's secondary-controls capability gate established this
    // MSR and this VM-exit handler executes at CPL0.
    let secondary_capability = unsafe { cpu::rdmsr(vmx::IA32_VMX_PROCBASED_CTLS2) };
    let Some(l1_secondary_capability) =
        restrict_vmx_capability(vmx::IA32_VMX_PROCBASED_CTLS2, secondary_capability)
    else {
        stop_unexpected_exit(
            b"VMPTRLD secondary capability changed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    if !vmcs_revision_is_supported(revision, basic.revision_id, l1_secondary_capability) {
        complete_vmx_instruction(
            l1_vmx_failure(&state, VMXERR_VMPTRLD_INCORRECT_REVISION),
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return;
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

    let Some(result) = execute_l1_vmx_instruction(&state, || {
        // SAFETY: the operand is an aligned, in-range trusted L1 allocation,
        // distinct from the carrier and VMXON pages. Hardware validates its
        // revision before making it current, retaining the old VMCS on failure.
        unsafe { vmcs_load(region) }
    }) else {
        stop_unexpected_exit(
            b"executing VMPTRLD with L1 VMCS failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    if result == VmInstructionResult::Vmsucceed {
        current_cpu().vcpu.lock().record_vmptrld_success(region);
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

/// Stores L1's tracked current-VMCS pointer without switching hardware VMCSes.
fn handle_l1_vmptrst(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    let state = *current_cpu().vcpu.lock();
    let cr4_shadow = unsafe { vmcs_read(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmcs_read(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    let current = state
        .current_vmcs()
        .map_or(u64::MAX, |vmcs| vmcs.address().get());
    if let Err(fault) = l1_memory_operand(qualification, registers)
        .and_then(|linear| write_l1_linear_u64(linear, current))
    {
        inject_l1_operand_fault(
            fault,
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return;
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
    let state = *current_cpu().vcpu.lock();
    let cr4_shadow = unsafe { vmcs_read(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }
    let cs = unsafe { vmcs_read(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return VMEXIT_ACTION_RESUME;
    }
    if !owns_l1_vpid_namespace(&state) {
        stop_unexpected_exit(
            b"nested entry has no CPU VPID namespace lease",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
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
    let Some(carrier) = validate_private_vmcs_address(carrier_address) else {
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
    let cached_direct = match *current_cpu().direct_patch.lock() {
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
        let mut cached = current_cpu().carrier_patch.lock();
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
        unsafe { vmcs_read(vmcs::GUEST_INTERRUPTIBILITY_INFO) }.unwrap_or(u64::MAX) & 8;
    // SAFETY: the owning CPU's carrier is still current. These are the stopped
    // L1's saved MSRs, not the private host values installed for Rust execution.
    let inherited = unsafe {
        vmcs_read(vmcs::GUEST_IA32_PAT)
            .ok()
            .zip(vmcs_read(vmcs::GUEST_IA32_EFER).ok())
    };
    let Some((pat, efer)) = inherited else {
        stop_unexpected_exit(
            b"reading inherited L1 MSRs failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    // Reuse this exact stopped-carrier EFER read for the mode check. There is
    // no intervening guest execution or write to EFER; never cache L1's LMA.
    let host_limits = current_cpu().host_limits_for_efer(efer);
    if with_cpu_runtime(|state| state.inherited = PatEfer { pat, efer }).is_none() {
        stop_unexpected_exit(
            b"capturing inherited L1 MSRs failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }

    if unsafe { vmcs_load(current.address()) } != VmxStatus::Success {
        stop_unexpected_exit(
            b"selecting L1 VMCS for VMLAUNCH failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    // Complete prior successful L1 VMWRITEs before any physical entry attempt,
    // including one that will fail controls/host/guest validation. No stale
    // exit snapshot survives immediate VMfail or a subsequent hardware exit.
    if !flush_current_idle_guest_snapshot(current.address()) {
        stop_unexpected_exit(
            b"flushing direct guest writes before entry failed",
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
    let exit_controls = direct_patch_value(&saved_direct, VmcsField::VmExitControls);
    let Some(exit_controls) = exit_controls else {
        stop_unexpected_exit(
            b"reading L1 host exit controls failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    let Some(msr_controls) = checked_direct_msr_lists(&saved_direct, host_limits.physical_bits)
    else {
        stop_unexpected_exit(
            b"reading original MSR-list controls failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    let cache_entry_policy = match *current_cpu().entry_policy.lock() {
        Some((address, controls)) if address == current.address() && controls == exit_controls => {
            false
        }
        None => true,
        Some(_) => stop_unexpected_exit(
            b"cached entry policy does not match original controls",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        ),
    };
    let supported_controls = if cache_entry_policy {
        let Some(supported) = l1_direct_controls_supported(&saved_direct) else {
            stop_unexpected_exit(
                b"reading original control capabilities failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        };
        supported
    } else {
        true
    };
    let invalid_controls = msr_controls.is_err() || !supported_controls;
    let host_check = host_validation::validate(host_limits, exit_controls, |field| {
        direct_patch_value(&saved_direct, field)
    });
    match (invalid_controls, host_check) {
        (false, Ok(())) => {}
        (true, _)
        | (_, Err(host_validation::Error::Field(_) | host_validation::Error::AddressSpaceSize)) => {
            // Restore originals before asking hardware to record a guaranteed
            // failed entry: otherwise L0's zeroed MSR-list counts could hide an
            // invalid-control error which has priority over invalid host state.
            if !restore_direct_vmcs(&saved_direct) {
                stop_unexpected_exit(
                    b"materializing rejected L1 entry failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            }
            *current_cpu().direct_patch.lock() = None;
            *current_cpu().entry_policy.lock() = None;
            let mut accesses = vmx::VmcsAccessCounts::default();
            // SAFETY: the BSP owns this current direct VMCS, outside SMM; all
            // original fields are materialized. A proven-invalid control or
            // HOST_CS=0 guarantees early VMfail without guest/MSR loading.
            // Hardware preserves launch-state/control/host error priority; the
            // helper restores its guard field before carrier selection.
            let result = unsafe {
                let status = if invalid_controls {
                    vmx::reject_control_entry(
                        instruction == VmEntryInstruction::Vmresume,
                        &mut accesses,
                    )
                } else {
                    vmx::reject_host_entry(
                        instruction == VmEntryInstruction::Vmresume,
                        &mut accesses,
                    )
                };
                record_diagnostic(DiagnosticEvent::VmcsAccessBatch(accesses));
                match status {
                    Some(VmxStatus::FailInvalid) => Some(VmInstructionResult::VmfailInvalid),
                    Some(VmxStatus::FailValid) => vmcs_read(vmcs::VM_INSTRUCTION_ERROR)
                        .ok()
                        .and_then(|error| u32::try_from(error).ok())
                        .map(VmInstructionResult::VmfailValid),
                    _ => None,
                }
            };
            let Some(result) = result else {
                stop_unexpected_exit(
                    b"recording rejected L1 entry failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            };
            // SAFETY: the reserved carrier remains owned/live and the guarded
            // failure did not clear or launch either VMCS. Restore its selection
            // before changing L1 flags/RIP; completion reads the preserved error.
            if unsafe { vmcs_load(carrier) } != VmxStatus::Success {
                stop_unexpected_exit(
                    b"restoring carrier after entry rejection failed",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            }
            complete_vmx_instruction(
                result,
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
            return VMEXIT_ACTION_RESUME;
        }
        (_, Err(host_validation::Error::Limits | host_validation::Error::Missing(_))) => {
            stop_unexpected_exit(
                b"invalid L0 host validation metadata",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }
    }
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
        *current_cpu().direct_patch.lock() = Some((current.address(), saved_direct));
    }
    if prepare_direct_msr_fields(&saved_direct).is_none() {
        stop_unexpected_exit(
            b"preparing private MSR fields failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    if cache_entry_policy {
        *current_cpu().entry_policy.lock() = Some((current.address(), exit_controls));
    }

    let mut active = current_cpu().nested_run.lock();
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

/// Validates original MSR-list control ranges before Direct fields are patched.
/// The outer Option reports an L0 VMREAD/manifest invariant failure; an inner
/// error is L1's invalid controls and must produce architectural error 7.
fn checked_direct_msr_lists(
    saved: &[u64; DIRECT_VMCS_PATCH_MANIFEST.len()],
    physical_bits: u8,
) -> Option<Result<(), nested_vmx::msr_list::Error>> {
    use nested_vmx::MsrMirrorMetadata;
    use nested_vmx::msr_list::Error;
    use nested_vmx::msr_list::List;
    if !(12..=52).contains(&physical_bits) {
        return None;
    }
    let store_address = direct_patch_value(saved, VmcsField::VmExitMsrStoreAddress)?;
    let store_count =
        u32::try_from(direct_patch_value(saved, VmcsField::VmExitMsrStoreCount)?).ok()?;
    let load_address = direct_patch_value(saved, VmcsField::VmExitMsrLoadAddress)?;
    let load_count =
        u32::try_from(direct_patch_value(saved, VmcsField::VmExitMsrLoadCount)?).ok()?;
    let entry_address = direct_patch_value(saved, VmcsField::VmEntryMsrLoadAddress)?;
    let entry_count =
        u32::try_from(direct_patch_value(saved, VmcsField::VmEntryMsrLoadCount)?).ok()?;
    Some((|| {
        let metadata = MsrMirrorMetadata::new(store_address, store_count, load_address, load_count)
            .ok_or(Error::Count)?;
        metadata.checked_lists(physical_bits)?;
        List::new(entry_address, entry_count, physical_bits)?;
        Ok(())
    })())
}

/// Hardware sees unmasked physical capabilities, so it cannot validate the
/// narrower contract advertised to L1. Cache only a completely valid original
/// set; every successful write of any of these five control words invalidates it.
fn l1_direct_controls_supported(saved: &[u64; DIRECT_VMCS_PATCH_MANIFEST.len()]) -> Option<bool> {
    let true_controls = l1_vmx_capability(vmx::IA32_VMX_BASIC)? & (1 << 55) != 0;
    // SAFETY: this CPU owns the current stopped Direct VMCS. Its execution
    // controls are never patched by L0; hardware returns the original L1 word.
    let primary =
        u32::try_from(unsafe { vmcs_read(vmcs::CPU_BASED_VM_EXEC_CONTROL) }.ok()?).ok()?;
    for (field, legacy, true_msr) in [
        (
            vmcs::PIN_BASED_VM_EXEC_CONTROL,
            vmx::IA32_VMX_PINBASED_CTLS,
            vmx::IA32_VMX_TRUE_PINBASED_CTLS,
        ),
        (
            vmcs::CPU_BASED_VM_EXEC_CONTROL,
            vmx::IA32_VMX_PROCBASED_CTLS,
            vmx::IA32_VMX_TRUE_PROCBASED_CTLS,
        ),
        (
            vmcs::SECONDARY_VM_EXEC_CONTROL,
            vmx::IA32_VMX_PROCBASED_CTLS2,
            vmx::IA32_VMX_PROCBASED_CTLS2,
        ),
        (
            vmcs::VM_ENTRY_CONTROLS,
            vmx::IA32_VMX_ENTRY_CTLS,
            vmx::IA32_VMX_TRUE_ENTRY_CTLS,
        ),
        (
            vmcs::VM_EXIT_CONTROLS,
            vmx::IA32_VMX_EXIT_CTLS,
            vmx::IA32_VMX_TRUE_EXIT_CTLS,
        ),
    ] {
        if field == vmcs::SECONDARY_VM_EXEC_CONTROL
            && primary & vmcs::PRIMARY_EXEC_ACTIVATE_SECONDARY_CONTROLS == 0
        {
            continue; // Architecturally ignored, even if its stored bits are invalid.
        }
        let original = match field {
            vmcs::VM_ENTRY_CONTROLS => direct_patch_value(saved, VmcsField::VmEntryControls)?,
            vmcs::VM_EXIT_CONTROLS => direct_patch_value(saved, VmcsField::VmExitControls)?,
            vmcs::CPU_BASED_VM_EXEC_CONTROL => u64::from(primary),
            // SAFETY: the owned/current Direct VMCS retains these unpatched
            // mandatory execution fields; no guest runs during the check.
            _ => unsafe { vmcs_read(field) }.ok()?,
        };
        let word = ControlProvenance::new(u32::try_from(original).ok()?, 0);
        if !word.requested_supported(l1_vmx_capability(if true_controls {
            true_msr
        } else {
            legacy
        })?) {
            return Some(false);
        }
    }
    Some(true)
}

/// Capture static host-validation capabilities once on the owning CPU.
/// Leaf 7 SHSTK/IBT/LAM, NX and address widths are feature availability, not
/// CR4-dependent OSXSAVE/OSPKE or XCR0/XSS-dependent CPUID. No live guest mode
/// is read here. Rebuild after any physical CPU reset or ownership transition.
///
/// # Safety
/// Caller is pinned at CPL0 on a CPUID.VMX-capable CPU, before its first L0
/// entry, with validated physical-address width and readable VMX fixed MSRs.
unsafe fn capture_host_validation_limits(physical_bits: u8) -> host_validation::Limits {
    let features = cpu::cpuid(7, 0);
    let extended = cpu::cpuid(0x8000_0001, 0);
    let linear_bits = if cpu::cpuid(0x8000_0000, 0).eax >= 0x8000_0008 {
        ((cpu::cpuid(0x8000_0008, 0).eax >> 8) & 255) as u8
    } else {
        48
    };
    let cet_allowed = (if features.ecx & (1 << 7) != 0 { 3 } else { 0 })
        | (if features.edx & (1 << 20) != 0 {
            0x3c | (!0_u64 << 10)
        } else {
            0
        });
    // SAFETY: CPL0 and CPUID.VMX establish all four read-only fixed-bit MSRs.
    // Only this pinned physical CPU's static capabilities are sampled; there
    // is no current-VMCS, guest-state or firmware-lifetime dependency.
    unsafe {
        host_validation::Limits {
            cr0_fixed0: cpu::rdmsr(vmx::IA32_VMX_CR0_FIXED0),
            cr0_fixed1: cpu::rdmsr(vmx::IA32_VMX_CR0_FIXED1),
            cr4_fixed0: cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED0),
            cr4_fixed1: cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED1),
            physical_bits,
            linear_bits,
            lam: features.eax >= 1 && cpu::cpuid(7, 1).eax & (1 << 26) != 0,
            efer_allowed: 0x501
                | if extended.edx & (1 << 20) != 0 {
                    1 << 11
                } else {
                    0
                },
            cet_allowed,
            l1_ia32e: false,
        }
    }
}

/// Reads every field whose direct-VMCS value must survive L0 patching.
fn read_direct_patch_fields() -> Option<[u64; DIRECT_VMCS_PATCH_MANIFEST.len()]> {
    let mut values = [0; DIRECT_VMCS_PATCH_MANIFEST.len()];
    for (value, patch) in values.iter_mut().zip(DIRECT_VMCS_PATCH_MANIFEST) {
        *value = unsafe { vmcs_read(patch.field as u32) }.ok()?;
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
            PatchKind::MsrControls | PatchKind::GuestMsrState | PatchKind::EntryMsrLoad => continue,
        };
        if saved_direct[index] == value {
            continue;
        }
        if unsafe { vmcs_write(patch.field as u32, value) } != VmxStatus::Success {
            return false;
        }
    }
    true
}

/// Restores every L1-visible field before exposing a retained direct VMCS.
fn restore_direct_vmcs(values: &[u64; DIRECT_VMCS_PATCH_MANIFEST.len()]) -> bool {
    for (value, patch) in values.iter().zip(DIRECT_VMCS_PATCH_MANIFEST) {
        if unsafe { vmcs_write(patch.field as u32, *value) } != VmxStatus::Success {
            return false;
        }
    }
    true
}

/// Loads the L2 values original controls specify, while ensuring that any
/// subsequent exit restores private L0 PAT/EFER. Hardware support for these
/// forced bits was required before the initial carrier launch.
fn prepare_direct_msr_fields(saved: &[u64; DIRECT_VMCS_PATCH_MANIFEST.len()]) -> Option<()> {
    let entry = u32::try_from(direct_patch_value(saved, VmcsField::VmEntryControls)?).ok()?;
    let exit = u32::try_from(direct_patch_value(saved, VmcsField::VmExitControls)?).ok()?;
    let entry_controls = ControlProvenance::new(
        entry,
        vmcs::VM_ENTRY_LOAD_IA32_PAT | vmcs::VM_ENTRY_LOAD_IA32_EFER,
    );
    let exit_controls = ControlProvenance::new(
        exit,
        vmcs::VM_EXIT_SAVE_IA32_PAT
            | vmcs::VM_EXIT_SAVE_IA32_EFER
            | vmcs::VM_EXIT_LOAD_IA32_PAT
            | vmcs::VM_EXIT_LOAD_IA32_EFER,
    );
    let guest = PatEfer {
        pat: direct_patch_value(saved, VmcsField::GuestIa32Pat)?,
        efer: direct_patch_value(saved, VmcsField::GuestIa32Efer)?,
    };
    // SAFETY: this CPU owns the current Direct VMCS; guest CR0 is not patched.
    let cr0 = unsafe { vmcs_read(vmcs::GUEST_CR0) }.ok()?;
    let entry_address = direct_patch_value(saved, VmcsField::VmEntryMsrLoadAddress)?;
    let entry_count =
        u32::try_from(direct_patch_value(saved, VmcsField::VmEntryMsrLoadCount)?).ok()?;
    let store_count =
        u32::try_from(direct_patch_value(saved, VmcsField::VmExitMsrStoreCount)?).ok()?;
    let (loaded, mirror_address, mirror_count, capture_address, capture_count) =
        with_cpu_runtime(|state| {
            let list = MsrList::new(
                entry_address,
                entry_count,
                state.ram.physical_width().bits(),
            )
            .ok()?;
            let (address, count) = state.prepare_entry(list)?;
            let (capture_address, capture_count) = state.prepare_capture(store_count)?;
            state.entry_loaded = state.inherited.entry(guest, entry, cr0);
            Some((
                state.entry_loaded,
                address,
                count,
                capture_address,
                capture_count,
            ))
        })??;
    for (field, value) in [
        (vmcs::VM_ENTRY_MSR_LOAD_ADDR, mirror_address),
        (vmcs::VM_ENTRY_MSR_LOAD_COUNT, u64::from(mirror_count)),
        (vmcs::VM_EXIT_MSR_STORE_ADDR, capture_address),
        (vmcs::VM_EXIT_MSR_STORE_COUNT, u64::from(capture_count)),
        (vmcs::GUEST_IA32_PAT, loaded.pat),
        (vmcs::GUEST_IA32_EFER, loaded.efer),
        (
            vmcs::VM_ENTRY_CONTROLS,
            u64::from(entry_controls.effective()),
        ),
        (vmcs::VM_EXIT_CONTROLS, u64::from(exit_controls.effective())),
    ] {
        // SAFETY: only the owned Direct VMCS changes. Original fields remain in
        // the patch manifest; no hardware entry occurs until all writes succeed.
        if unsafe { vmcs_write(field, value) } != VmxStatus::Success {
            return None;
        }
    }
    Some(())
}

/// Recovers L1-visible MSRs without confusing forced saves with requested ones.
/// Late failed entry does not save guest state or an exit MSR-store list.
fn reflected_direct_msrs(run: &NestedRun, reason: u64) -> Option<PatEfer> {
    let controls = u32::try_from(direct_patch_value(
        &run.saved_direct,
        VmcsField::VmExitControls,
    )?)
    .ok()?;
    let host = PatEfer {
        pat: direct_patch_value(&run.saved_direct, VmcsField::HostIa32Pat)?,
        efer: direct_patch_value(&run.saved_direct, VmcsField::HostIa32Efer)?,
    };
    let live = if reason & (1 << 31) != 0 {
        match reason & 0xffff {
            // Guest checking/loading is concurrent. A not-yet-loaded original
            // L1 value is a valid deterministic choice for undefined components.
            33 => with_cpu_runtime(|state| state.inherited)?,
            34 => {
                // SAFETY: this CPU owns the failed-entry Direct VMCS; hardware
                // supplies the one-based index of the first failing list item.
                let qualification = unsafe { vmcs_read(vmcs::EXIT_QUALIFICATION) }.ok()?;
                with_cpu_runtime(|state| {
                    state
                        .entry_loaded
                        .failed_load(&state.entry[..state.entry_count as usize], qualification)
                })??
            }
            _ => return None,
        }
    } else {
        // SAFETY: a normal Direct exit completed the forced PAT/EFER saves into
        // this CPU-owned VMCS before hardware loaded L0's private host values.
        let captured = unsafe {
            PatEfer {
                pat: vmcs_read(vmcs::GUEST_IA32_PAT).ok()?,
                efer: vmcs_read(vmcs::GUEST_IA32_EFER).ok()?,
            }
        };
        let mut cache = current_cpu().direct_patch.lock();
        let (address, values) = cache.as_mut()?;
        if *address != run.direct {
            return None;
        }
        for (bit, field, value) in [
            (
                vmcs::VM_EXIT_SAVE_IA32_PAT,
                vmcs::GUEST_IA32_PAT,
                captured.pat,
            ),
            (
                vmcs::VM_EXIT_SAVE_IA32_EFER,
                vmcs::GUEST_IA32_EFER,
                captured.efer,
            ),
        ] {
            if controls & bit != 0 && !write_direct_patch_field(values, field, value) {
                return None;
            }
        }
        captured
    };
    Some(live.exit(host, controls))
}

/// Materializes one retained direct VMCS while VMCS01 is current.
fn materialize_direct_patch(only: Option<VmcsPhys>) -> bool {
    let mut cached = current_cpu().direct_patch.lock();
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
    let Some(carrier) = validate_private_vmcs_address(carrier_address) else {
        return false;
    };
    if carrier == direct
        || unsafe { vmcs_load(direct) } != VmxStatus::Success
        || !restore_direct_vmcs(&values)
        || unsafe { vmcs_load(carrier) } != VmxStatus::Success
    {
        return false;
    }
    *cached = None;
    *current_cpu().entry_policy.lock() = None;
    true
}

/// Reflects a hardware L2 exit through VMCS01 into Linux KVM's host RIP.
fn reflect_l2_vmexit(run: &NestedRun, reason: u64, registers: &GuestRegisters) {
    let mut current = u64::MAX;
    if unsafe { vmx::vmptrst(&mut current) } != VmxStatus::Success || current != run.direct.get() {
        stop_nested_exit(b"unexpected direct VMCS on L2 exit", run.direct, registers);
    }
    let snapshot = ExitSnapshot::capture(run.direct, |field| {
        if field == vmcs::VM_EXIT_REASON {
            Some(reason)
        } else {
            // SAFETY: this CPU owns the current stopped direct VMCS. These are
            // mandatory exit/guest fields (EPT is a launch prerequisite), not
            // opaque VMCS memory. Capturing the idle state changes no fields.
            unsafe { vmcs_read(field) }.ok()
        }
    });
    let Some(snapshot) = snapshot else {
        stop_nested_exit(
            b"capturing direct exit information failed",
            run.direct,
            registers,
        );
    };
    if with_cpu_runtime(|state| state.exit_snapshot = Some(snapshot)).is_none() {
        stop_nested_exit(
            b"publishing direct exit snapshot failed",
            run.direct,
            registers,
        );
    }
    let Some(l1_msrs) = reflected_direct_msrs(run, reason) else {
        stop_nested_exit(
            b"reflecting original MSR controls failed",
            run.direct,
            registers,
        );
    };
    let mirrors = with_cpu_runtime(|state| {
        let list = |address, count| {
            MsrList::new(
                direct_patch_value(&run.saved_direct, address)?,
                u32::try_from(direct_patch_value(&run.saved_direct, count)?).ok()?,
                state.ram.physical_width().bits(),
            )
            .ok()
        };
        let store = list(
            VmcsField::VmExitMsrStoreAddress,
            VmcsField::VmExitMsrStoreCount,
        )?;
        let load = list(
            VmcsField::VmExitMsrLoadAddress,
            VmcsField::VmExitMsrLoadCount,
        )?;
        // Late failed entry loads host state/MSRs, but must not store guest MSRs.
        if reason & (1 << 31) == 0 && state.store_guest_msrs(store)?.is_err() {
            return Some(Err(()));
        }
        // The lists may overlap: read host items only after all stores finished.
        Some(Ok(state.prepare_host_load(load, run.direct)?))
    })
    .flatten();
    let (host_address, host_count) = match mirrors {
        Some(Ok(mirror)) => mirror,
        Some(Err(())) => nested_vmx_abort(run.direct, 1, registers),
        None => stop_nested_exit(b"unsupported exit MSR-list backing", run.direct, registers),
    };
    if unsafe { vmcs_load(run.carrier) } != VmxStatus::Success {
        stop_nested_exit(
            b"restoring carrier after L2 exit failed",
            run.direct,
            registers,
        );
    }

    if write_reflected_l1_state(run, l1_msrs.pat, l1_msrs.efer).is_none() {
        stop_nested_exit(b"reflecting L1 host state failed", run.direct, registers);
    }
    // The private carrier starts with count=0. The only nonzero writer is
    // this block; complete_reflected_msr_load clears it on the first L1 exit,
    // before dispatch can attempt another nested entry. prepare_host_load
    // rejects an outstanding owner/count even for an empty list. Thus an empty
    // reflection cannot leave an earlier load armed; its address is ignored.
    // This does NOT apply to the Direct VMCS's separate L2 entry mirror.
    if host_count != 0 {
        for (field, value) in [
            (vmcs::VM_ENTRY_MSR_LOAD_ADDR, host_address),
            (vmcs::VM_ENTRY_MSR_LOAD_COUNT, u64::from(host_count)),
        ] {
            // SAFETY: the owned carrier is current. The stable per-CPU mirror is
            // published only after its complete copy; no lock spans hardware entry.
            if unsafe { vmcs_write(field, value) } != VmxStatus::Success {
                stop_nested_exit(
                    b"publishing reflected host MSR list failed",
                    run.direct,
                    registers,
                );
            }
        }
    }

    // VMX does not switch CR2 or XSTATE between L1 and L2. The exit stub protects
    // the live extended state from L0; it must not restore an older L1 image.
    // L0 does not touch CR2 except to deliver an intentional L1 #PF. Genuine
    // root page faults never resume, so no stale snapshot may erase guest CR2.
}

/// A deferred L1 host list belongs to one reflection, not every carrier entry.
/// Hardware's failed list load becomes the original L1 VMCS's VMX abort 4.
fn complete_reflected_msr_load(reason: u64, registers: &GuestRegisters) {
    let Some(owner) = with_cpu_runtime(|state| state.host_owner) else {
        stop_unexpected_exit(b"missing per-CPU MSR state", reason, 0, 0, 0, registers);
    };
    let Some(owner) = owner else {
        return;
    };
    if reason & (1 << 31) != 0 && reason & 0xffff == 34 {
        nested_vmx_abort(owner, 4, registers);
    }
    if reason & (1 << 31) != 0 {
        stop_unexpected_exit(
            b"invalid reflected carrier state",
            reason,
            0,
            0,
            0,
            registers,
        );
    }
    for field in [vmcs::VM_ENTRY_MSR_LOAD_COUNT, vmcs::VM_ENTRY_MSR_LOAD_ADDR] {
        // SAFETY: this is the first exit of the owned carrier after the list was
        // consumed. No guest executes while its next entry is being prepared.
        if unsafe { vmcs_write(field, 0) } != VmxStatus::Success {
            stop_nested_exit(b"clearing reflected MSR list failed", owner, registers);
        }
    }
    if with_cpu_runtime(|state| {
        state.host_owner = None;
        state.host_count = 0;
    })
    .is_none()
    {
        stop_nested_exit(b"clearing reflected MSR owner failed", owner, registers);
    }
}

/// Intel VMX abort is an L1 terminal CPU state, not VMfail or a root exception.
/// Record the architectural indicator, retain all private state and park this
/// BSP until reset. No VMCS may be used again on this aborted virtual CPU.
fn nested_vmx_abort(direct: VmcsPhys, code: u32, registers: &GuestRegisters) -> ! {
    let recorded = with_cpu_runtime(|state| {
        let Some(address) = direct.get().checked_add(4) else {
            return false;
        };
        if state.aborted || !state.allows_list_access(address, 4, true) {
            return false;
        }
        state.aborted = true;
        // SAFETY: the aligned abort indicator is foreign writable VMCS RAM,
        // outside L0 storage. Its sole L1 CPU is stopped permanently; no later
        // hardware entry or Rust VMCS access can race this terminal publication.
        unsafe { ptr::write_volatile(address as *mut u32, code) };
        true
    });
    if recorded != Some(true) {
        stop_nested_exit(b"recording nested VMX abort failed", direct, registers);
    }
    let mut serial = SerialPort;
    serial.init();
    serial.write_bytes(b"thin-hv: nested VMX abort");
    write_raw_field(&mut serial, b"code", u64::from(code));
    write_raw_field(&mut serial, b"vmcs", direct.get());
    write_raw_newline(&mut serial);
    // No lock is held, VMX remains active, and no guest instruction can run.
    // NMI/root-event support is a separate physical-qualification requirement.
    halt_with_guest_xstate()
}

/// Stops after recovering L2's exit diagnostics only on an error path.
fn stop_nested_exit(message: &'static [u8], direct: VmcsPhys, registers: &GuestRegisters) -> ! {
    if unsafe { vmcs_load(direct) } != VmxStatus::Success {
        stop_unexpected_exit(message, u64::MAX, u64::MAX, u64::MAX, u64::MAX, registers);
    }
    let reason = unsafe { vmcs_read(vmcs::VM_EXIT_REASON) }.unwrap_or(u64::MAX);
    let qualification = unsafe { vmcs_read(vmcs::EXIT_QUALIFICATION) }.unwrap_or(u64::MAX);
    let guest_rip = unsafe { vmcs_read(vmcs::GUEST_RIP) }.unwrap_or(u64::MAX);
    let instruction_len = unsafe { vmcs_read(vmcs::VM_EXIT_INSTRUCTION_LEN) }.unwrap_or(u64::MAX);
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
        // SAFETY: the owned carrier is current; all reflected fields above
        // come from validated original L1 host state or specified VM-exit resets.
        if unsafe { vmcs_write_reflected(field, value) } != VmxStatus::Success {
            return None;
        }
    }
    Some(())
}

/// Completes an immediate direct VM-entry failure and resumes VMCS01.
unsafe extern "sysv64" fn nested_vmentry_failed(registers: *const GuestRegisters, rflags: u64) {
    #[cfg(feature = "host-xstate-test")]
    clobber_host_xmm();
    // SAFETY: `vmexit_entry` passes its live, uniquely owned saved-GPR frame.
    let registers = unsafe { &*registers };
    let Some(run) = current_cpu().nested_run.lock().take() else {
        stop_unexpected_exit(b"missing failed VMLAUNCH state", 20, 0, 0, 0, registers);
    };
    record_diagnostic(DiagnosticEvent::EntryFailure(run.outer_reason));
    let result = if rflags & 1 != 0 {
        VmInstructionResult::VmfailInvalid
    } else if rflags & (1 << 6) != 0 {
        let Some(error) = (unsafe { vmcs_read(vmcs::VM_INSTRUCTION_ERROR) })
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
    if unsafe { vmcs_load(run.carrier) } != VmxStatus::Success {
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
    let state = *current_cpu().vcpu.lock();
    let cr4_shadow = unsafe { vmcs_read(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmcs_read(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
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

    let Some(instruction_info) = (unsafe { vmcs_read(vmcs::VMX_INSTRUCTION_INFO) })
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
        let linear = l1_memory_operand(qualification, registers);
        let Ok(linear) = linear else {
            inject_l1_operand_fault(
                DataFault::InvalidState,
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
            return;
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
            let value = match memory_linear
                .ok_or(DataFault::InvalidState)
                .and_then(read_l1_linear_u64)
            {
                Ok(value) => value,
                Err(fault) => {
                    inject_l1_operand_fault(
                        fault,
                        reason,
                        qualification,
                        guest_rip,
                        instruction_len,
                        registers,
                    );
                    return;
                }
            };
            Some(value)
        }
    } else {
        None
    };
    // VMWRITE source-memory faults precede unsupported-field validation (SDM
    // Vol. 3C, VMWRITE); do not skip the source access for a wide field operand.
    let Some(field) = field_value.and_then(|value| u32::try_from(value).ok()) else {
        complete_vmx_instruction(
            l1_vmx_failure(&state, VMXERR_UNSUPPORTED_VMCS_COMPONENT),
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return;
    };
    // The exposed VMX_MISC[29] is zero: VM-exit information remains read-only
    // even when this physical CPU permits VMWRITE to those fields. Probe field
    // existence first so reserved/unsupported encodings return error 12, not 13.
    if write && vmcs_field_is_read_only(field) {
        let Some(result) = execute_l1_vmx_instruction(&state, || {
            // SAFETY: the helper selects L1's valid VMCS; register-form VMREAD
            // validates this encoding without changing the addressed field.
            match unsafe { vmcs_read(field) } {
                Ok(_) => VmxStatus::Success,
                Err(status) => status,
            }
        }) else {
            stop_unexpected_exit(
                b"validating read-only VMWRITE field failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        };
        complete_vmx_instruction(
            if result == VmInstructionResult::Vmsucceed {
                l1_vmx_failure(&state, VMXERR_VMWRITE_READ_ONLY_COMPONENT)
            } else {
                result
            },
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
        return;
    }
    // Full source access and privilege/pointer checks precede this shortcut.
    // Hardware capture proved these four exact mandatory guest fields exist;
    // VMWRITE does not validate their entry values. Retain width-truncated L1
    // writes until entry or an L1 ownership boundary selects the SAME VMCS.
    // Unknown/read-only encodings still follow their hardware/error paths.
    let retained_access = with_cpu_runtime(|state| {
        if let Some(value) = write_value {
            state
                .exit_snapshot
                .as_mut()
                .is_some_and(|snapshot| snapshot.queue_write(current.address(), field, value))
                .then_some(None)
        } else {
            state
                .exit_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.read(current.address(), field))
                .map(Some)
        }
    })
    .flatten();
    let shadowed = if retained_access.is_some() {
        retained_access
    } else {
        let mut cached = current_cpu().direct_patch.lock();
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
        if !write {
            record_diagnostic(DiagnosticEvent::L1VmreadHardware(field));
        }
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
        let Some(carrier) = validate_private_vmcs_address(carrier_address) else {
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
            || unsafe { vmcs_load(current.address()) } != VmxStatus::Success
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
            (unsafe { vmcs_write(field, value) }, None)
        } else {
            match unsafe { vmcs_read(field) } {
                Ok(value) => (VmxStatus::Success, Some(value)),
                Err(status) => (status, None),
            }
        };
        let hardware_error = if status == VmxStatus::FailValid {
            (unsafe { vmcs_read(vmcs::VM_INSTRUCTION_ERROR) })
                .ok()
                .and_then(|value| u32::try_from(value).ok())
        } else {
            None
        };
        if unsafe { vmcs_load(carrier) } != VmxStatus::Success {
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
    // Every retained writable field was queued above, before any hardware
    // fallback. Other encodings cannot mutate the snapshot's four guest values.
    if status == VmxStatus::Success
        && write_value.is_some()
        && matches!(
            field,
            vmcs::VM_ENTRY_MSR_LOAD_COUNT
                | vmcs::VM_EXIT_CONTROLS
                | vmcs::VM_ENTRY_CONTROLS
                | vmcs::PIN_BASED_VM_EXEC_CONTROL
                | vmcs::CPU_BASED_VM_EXEC_CONTROL
                | vmcs::SECONDARY_VM_EXEC_CONTROL
        )
    {
        *current_cpu().entry_policy.lock() = None;
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
        } else if let Err(fault) = memory_linear
            .ok_or(DataFault::InvalidState)
            .and_then(|linear| write_l1_linear_u64(linear, value))
        {
            inject_l1_operand_fault(
                fault,
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
            return;
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
    let state = *current_cpu().vcpu.lock();
    let cr4_shadow = unsafe { vmcs_read(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmcs_read(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    let instruction_info = unsafe { vmcs_read(vmcs::VMX_INSTRUCTION_INFO) }
        .ok()
        .and_then(|value| u32::try_from(value).ok());
    let fs_base = unsafe { vmcs_read(vmcs::GUEST_FS_BASE) }.unwrap_or(u64::MAX);
    let gs_base = unsafe { vmcs_read(vmcs::GUEST_GS_BASE) }.unwrap_or(u64::MAX);
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
    // Both types are required by the conservative capability policy. Unsupported
    // types fail before reading the descriptor, even if its pointer would fault.
    if kind != 1 && kind != 2 {
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
    let descriptor = match read_l1_linear_u128(linear) {
        Ok((ept_pointer, reserved)) => vmx::InveptDescriptor {
            ept_pointer,
            reserved,
        },
        Err(fault) => {
            inject_l1_operand_fault(
                fault,
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
            return;
        }
    };
    let Some(result) = execute_l1_vmx_instruction(&state, || {
        // SAFETY: L0 remains in VMX root operation; supported INVEPT receives a
        // local aligned descriptor copied from the checked L1 operand. Hardware
        // validates its type/EPTP while L1's current VMCS owns any error result.
        unsafe { vmx::invept(kind, &descriptor) }
    }) else {
        stop_unexpected_exit(
            b"executing INVEPT with L1 VMCS failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
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
    let state = *current_cpu().vcpu.lock();
    let cr4_shadow = unsafe { vmcs_read(vmcs::CR4_READ_SHADOW) }.unwrap_or(0);
    if cr4_shadow & CR4_VMX_ENABLE == 0 || !state.in_vmx_operation() {
        inject_invalid_opcode(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }
    let cs = unsafe { vmcs_read(vmcs::GUEST_CS_SELECTOR) }.unwrap_or(u64::MAX);
    if cs & 3 != 0 {
        inject_general_protection(reason, qualification, guest_rip, instruction_len, registers);
        return;
    }

    let instruction_info = unsafe { vmcs_read(vmcs::VMX_INSTRUCTION_INFO) }
        .ok()
        .and_then(|value| u32::try_from(value).ok());
    let fs_base = unsafe { vmcs_read(vmcs::GUEST_FS_BASE) }.unwrap_or(u64::MAX);
    let gs_base = unsafe { vmcs_read(vmcs::GUEST_GS_BASE) }.unwrap_or(u64::MAX);
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
    let descriptor = match read_l1_linear_u128(linear) {
        Ok((first, address)) => vmx::InvvpidDescriptor::from_words(first, address),
        Err(fault) => {
            inject_l1_operand_fault(
                fault,
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
            return;
        }
    };
    if !owns_l1_vpid_namespace(&state) {
        stop_unexpected_exit(
            b"INVVPID has no CPU namespace lease",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
    // This CPU's entire nonzero namespace belongs to the current trusted L1.
    // No translation is needed; hardware retains all advertised type, reserved
    // operand, canonical-address and VPID-zero behavior. The carrier is untagged.
    let Some(result) = execute_l1_vmx_instruction(&state, || {
        // SAFETY: L0 remains in VMX root operation; supported INVVPID receives a
        // local aligned descriptor. Hardware validates reserved bits and VPID
        // while L1's current VMCS owns any error result.
        unsafe { vmx::invvpid(kind, &descriptor) }
    }) else {
        stop_unexpected_exit(
            b"executing INVVPID with L1 VMCS failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
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

/// Local invalidation at an exclusive namespace boundary, not an L1 instruction.
/// Called only with the carrier current and all guest execution stopped.
fn invalidate_namespace() -> bool {
    // SAFETY: this CPU remains at CPL0 in VMX root with its owned carrier
    // current. Setup forbids a tagged carrier and requires INVVPID all-context
    // support. The aligned all-zero descriptor is valid for type 2; no other
    // owner shares nonzero tags on this CPU, and no guest runs during reuse.
    unsafe { vmx::invvpid(2, &vmx::InvvpidDescriptor::default()) == VmxStatus::Success }
}

/// The private GS binding identifies the owning pinned physical CPU. VMXON
/// and VMXOFF transfer its namespace only after complete hardware invalidation.
fn owns_l1_vpid_namespace(state: &VcpuState) -> bool {
    state
        .vmxon_region()
        .is_some_and(|owner| with_cpu_runtime(|runtime| runtime.vpids.owns(owner)) == Some(true))
}

/// Commit bounded idle guest writes while their exact Direct VMCS is current.
/// Failure retains the snapshot/remaining dirty bits and cannot authorize entry.
fn flush_current_idle_guest_snapshot(current: VmcsPhys) -> bool {
    with_cpu_runtime(|state| {
        if let Some(snapshot) = state.exit_snapshot.as_mut() {
            if !snapshot.flush(current, |field, value| {
                // SAFETY: the caller selected this CPU's stopped Direct VMCS;
                // flush checks its exact owner. Successful hardware capture
                // established each mandatory field. VMWRITE validates no guest
                // value here; no guest or other CPU can mutate this VMCS.
                unsafe { vmcs_write(field, value) == VmxStatus::Success }
            }) {
                return false;
            }
        }
        state.exit_snapshot = None;
        true
    }) == Some(true)
}

/// Flush before an L1 VMCS selection/clear/VMX lifetime instruction. Temporary
/// L0 selections for unrelated field/error/invalidation accesses need not flush:
/// those operations neither consume nor expose these four shadowed guest fields.
fn retire_idle_guest_snapshot() -> bool {
    let Some(owner) = with_cpu_runtime(|state| {
        if let Some(snapshot) = state.exit_snapshot.as_ref() {
            if snapshot.has_pending_writes() {
                return Some(snapshot.owner());
            }
        }
        state.exit_snapshot = None;
        None
    }) else {
        return false;
    };
    let Some(owner) = owner else {
        return true;
    };
    if current_cpu()
        .vcpu
        .lock()
        .current_vmcs()
        .map(|current| current.address())
        != Some(owner)
    {
        return false;
    }
    with_l1_current_vmcs(Some(owner), || flush_current_idle_guest_snapshot(owner)) == Some(true)
}

/// Runs an operation with L1's current VMCS selected and restores the carrier.
///
/// A missing L1 pointer intentionally leaves the carrier current: callers must
/// translate a hardware VMfailValid into L1's VMfailInvalid in that case. The
/// operation may itself change or clear the current pointer (VMPTRLD/VMCLEAR).
fn with_l1_current_vmcs<T>(current: Option<VmcsPhys>, operation: impl FnOnce() -> T) -> Option<T> {
    let mut carrier_address = u64::MAX;
    // SAFETY: only the BSP VM-exit handler calls this helper, in VMX root mode;
    // the local destination is writable and no other CPU owns these VMCSs.
    if unsafe { vmx::vmptrst(&mut carrier_address) } != VmxStatus::Success {
        return None;
    }
    let carrier = validate_private_vmcs_address(carrier_address)?;
    if let Some(current) = current {
        if current == carrier {
            return None;
        }
        // SAFETY: VcpuState records only a VMCS successfully loaded by this BSP,
        // and that L1-owned allocation remains live while it is current in L1.
        if unsafe { vmcs_load(current) } != VmxStatus::Success {
            return None;
        }
    }
    let result = operation();
    // SAFETY: this is the reserved carrier captured above; no operation passed
    // here clears/frees it. Restore even if the operation returned an error.
    if unsafe { vmcs_load(carrier) } != VmxStatus::Success {
        return None;
    }
    Some(result)
}

/// Executes a VMX instruction and captures its error before selecting carrier.
fn execute_l1_vmx_instruction(
    state: &VcpuState,
    instruction: impl FnOnce() -> VmxStatus,
) -> Option<VmInstructionResult> {
    with_l1_current_vmcs(
        state.current_vmcs().map(|current| current.address()),
        || {
            match instruction() {
                VmxStatus::Success => Some(VmInstructionResult::Vmsucceed),
                VmxStatus::FailInvalid => Some(VmInstructionResult::VmfailInvalid),
                VmxStatus::FailValid => {
                    // SAFETY: VMfailValid guarantees a valid hardware current VMCS;
                    // no intervening failing instruction has overwritten its error.
                    let error = unsafe { vmcs_read(vmcs::VM_INSTRUCTION_ERROR) }.ok()?;
                    Some(l1_vmx_failure(state, u32::try_from(error).ok()?))
                }
            }
        },
    )?
}

/// Commits a synthetic error to the opaque hardware VMCS, on the cold fail path.
fn publish_l1_instruction_error(error: u32) -> Option<()> {
    let current = current_cpu().vcpu.lock().current_vmcs()?.address();
    let mut carrier_address = u64::MAX;
    // SAFETY: completion runs in the BSP's root-mode carrier exit handler, with
    // an exclusive writable local destination for its current VMCS pointer.
    if unsafe { vmx::vmptrst(&mut carrier_address) } != VmxStatus::Success {
        return None;
    }
    let carrier = validate_private_vmcs_address(carrier_address)?;
    // run_direct_monitor reserves VMXON as page 0 and carrier as page 1 of the
    // same live runtime allocation. Per-pCPU bring-up must retain this ownership
    // relationship or pass the owning CPU's VMXON address explicitly.
    let vmxon = VmxonPhys::new(carrier.get().checked_sub(PAGE_SIZE)?)?;
    let invalid_revision = validate_private_vmcs_address(
        vmxon
            .get()
            .checked_add(ERROR_REVISION_PAGE.checked_mul(PAGE_SIZE)?)?,
    )?;
    with_l1_current_vmcs(Some(current), || {
        // SAFETY: the helper selected L1's valid hardware VMCS; VMREAD does not
        // modify the opaque error field on success.
        if unsafe { vmcs_read(vmcs::VM_INSTRUCTION_ERROR) }.ok() == Some(u64::from(error)) {
            return Some(());
        }
        // SAFETY: IA32_VMX_MISC exists on this VMX-enabled CPU and this read
        // occurs at CPL0. This is the physical capability, not L1's masked MSR.
        let writable_exit_fields = unsafe { cpu::rdmsr(vmx::IA32_VMX_MISC) } & (1 << 29) != 0;
        let recorded = if writable_exit_fields {
            // SAFETY: physical IA32_VMX_MISC[29] explicitly permits VMWRITE to
            // VM-exit information, including the current VMCS's error field.
            (unsafe { vmcs_write(vmcs::VM_INSTRUCTION_ERROR, u64::from(error)) })
                == VmxStatus::Success
        } else {
            let failure = vmx::RecordedFailure::from_error(error)?;
            let mut accesses = vmx::VmcsAccessCounts::default();
            // SAFETY: this BSP is in root mode with L1's valid current VMCS;
            // vmxon is its active page by the allocation invariant above.
            // INVVPID support is required by Direct-VMX capabilities; physical
            // VMX_MISC[29] was clear, so the error-13 instruction cannot succeed.
            // The disjoint runtime-owned invalid_revision page was initialized
            // with BASIC.revision_id XOR 1 before VMXON and is never activated,
            // rewritten, or freed while the monitor runs. Its complete address
            // range and the required WB memory type were checked at allocation.
            let status =
                unsafe { vmx::record_failure(failure, vmxon, invalid_revision, &mut accesses) };
            record_diagnostic(DiagnosticEvent::VmcsAccessBatch(accesses));
            status == VmxStatus::FailValid
        };
        if !recorded {
            return None;
        }
        // SAFETY: the checked instruction retained the selected VMCS; verify
        // its error before making the result observable to L1.
        (unsafe { vmcs_read(vmcs::VM_INSTRUCTION_ERROR) }.ok() == Some(u64::from(error)))
            .then_some(())
    })?
}

/// Converts a VMfail error according to whether L1 has a current VMCS.
fn l1_vmx_failure(state: &VcpuState, error: u32) -> VmInstructionResult {
    if state.current_vmcs().is_some() {
        VmInstructionResult::VmfailValid(error)
    } else {
        VmInstructionResult::VmfailInvalid
    }
}

/// VMCS field type 1 is read-only VM-exit information; hardware checks encoding.
const fn vmcs_field_is_read_only(field: u32) -> bool {
    (field >> 10) & 3 == 1
}

/// Decodes and reads one nested-VMX m64 pointer operand.
fn read_l1_vmx_pointer(
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) -> Option<u64> {
    let result = l1_memory_operand(qualification, registers).and_then(read_l1_linear_u64);
    match result {
        Ok(pointer) => Some(pointer),
        Err(fault) => {
            inject_l1_operand_fault(
                fault,
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
            None
        }
    }
}

/// Decodes only the effective address: each instruction decides when to access
/// it, preserving VMfail versus memory-fault priority (notably VMREAD/INVEPT).
fn l1_memory_operand(qualification: u64, registers: &GuestRegisters) -> Result<u64, DataFault> {
    // SAFETY: the BSP's carrier is current throughout L1 instruction emulation.
    let (information, fs_base, gs_base) = unsafe {
        (
            vmcs_read(vmcs::VMX_INSTRUCTION_INFO),
            vmcs_read(vmcs::GUEST_FS_BASE),
            vmcs_read(vmcs::GUEST_GS_BASE),
        )
    };
    vmx::memory_operand_address_64(
        u32::try_from(information.map_err(|_| DataFault::InvalidState)?)
            .map_err(|_| DataFault::InvalidState)?,
        qualification,
        |register| guest_gpr(registers, register),
        fs_base.map_err(|_| DataFault::InvalidState)?,
        gs_base.map_err(|_| DataFault::InvalidState)?,
    )
    .ok_or(DataFault::InvalidState)
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
    if with_cpu_runtime(|state| state.allows_vmx_region(address)) != Some(true) {
        return None;
    }
    VmcsPhys::new(address)
}

/// Only the owning CPU's allocated carrier and deliberate error-recording VMCS
/// are valid private operands. Guest VMX operands must use the disjoint L1 check.
fn validate_private_vmcs_address(address: u64) -> Option<VmcsPhys> {
    with_cpu_runtime(|state| state.private_vmcs(address))?
}

/// Reads an m64 operand through L1's current long-mode page tables.
fn read_l1_linear_u64(linear: u64) -> Result<u64, DataFault> {
    let state = l1_data_access().ok_or(DataFault::InvalidState)?;
    let linear = state.operand_range(linear, 8)?;
    read_l1_operand_word(linear, state)
}

/// Reads an m128 after validating its whole range, before either word's walk.
fn read_l1_linear_u128(linear: u64) -> Result<(u64, u64), DataFault> {
    let state = l1_data_access().ok_or(DataFault::InvalidState)?;
    let linear = state.operand_range(linear, 16)?;
    Ok((
        read_l1_operand_word(linear, state)?,
        read_l1_operand_word(linear + 8, state)?,
    ))
}

/// Captures architectural L1 paging inputs, not L0's private control registers.
fn l1_data_access() -> Option<DataAccess> {
    let cr4 = l1_visible_cr4()?;
    let mut pkru = 0;
    if cr4 & (1 << 22) != 0 {
        let host_cr4 = cpu::read_cr4();
        // SAFETY: guest PKE implies hardware PKU support. All private L0 page
        // tables map supervisor pages, so temporarily enabling PKE cannot let
        // the guest's PKRU deny L0 data accesses. Read the shared live register
        // without changing it, then restore host CR4 before leaving this block.
        unsafe {
            core::arch::asm!(
                "mov cr4, {enabled}", "rdpkru", "mov cr4, {original}",
                enabled = in(reg) host_cr4 | (1 << 22), original = in(reg) host_cr4,
                in("ecx") 0_u32, out("eax") pkru, out("edx") _,
                options(nostack, preserves_flags)
            );
        }
    }
    // SAFETY: the carrier is current; all VMREADs describe the stopped L1.
    // PKRS exists if guest CR4.PKS is set (validated by VM entry). L0 does not
    // write this live guest register. IA32_PKRS is architectural MSR 0x6e1.
    unsafe {
        Some(DataAccess {
            cr0: xstate::visible_cr(
                vmcs_read(vmcs::GUEST_CR0).ok()?,
                vmcs_read(vmcs::CR0_GUEST_HOST_MASK).ok()?,
                vmcs_read(vmcs::CR0_READ_SHADOW).ok()?,
            ),
            cr3: vmcs_read(vmcs::GUEST_CR3).ok()?,
            cr4,
            efer: vmcs_read(vmcs::GUEST_IA32_EFER).ok()?,
            rflags: vmcs_read(vmcs::GUEST_RFLAGS).ok()?,
            cpl: (vmcs_read(vmcs::GUEST_CS_SELECTOR).ok()? & 3) as u8,
            physical_bits: current_cpu().physical_bits,
            page_1g: cpu::cpuid(0x8000_0001, 0).edx & (1 << 26) != 0,
            pkru,
            pkrs: if cr4 & (1 << 24) != 0 {
                cpu::rdmsr(0x6e1) as u32
            } else {
                0
            },
        })
    }
}

/// Resolves one byte, retaining inaccessible L0 backing as a distinct failure.
fn l1_operand_physical(
    runtime: &CpuRuntimeState,
    linear: u64,
    state: DataAccess,
    write: bool,
) -> Result<u64, DataFault> {
    let physical = paging::translate_data(linear, state, write, |address, update| {
        runtime.paging_word(address, update)
    })?;
    runtime
        .operand_backing(physical, write)
        .ok_or(DataFault::Backing(physical))?;
    Ok(physical)
}

/// Reads an already range-checked word, including a discontiguous page crossing.
fn read_l1_operand_word(linear: u64, state: DataAccess) -> Result<u64, DataFault> {
    with_cpu_runtime(|runtime| {
        let mut value = 0;
        for index in 0..8 {
            let physical = l1_operand_physical(runtime, linear + index, state, false)?;
            value |= u64::from(
                runtime
                    .operand_byte(physical, None)
                    .ok_or(DataFault::Backing(physical))?,
            ) << (index * 8);
        }
        Ok(value)
    })
    .ok_or(DataFault::InvalidState)?
}

/// Checks all destination pages before any payload store, avoiding partial m64
/// writes when the second page faults. The stopped BSP is the only L1 CPU;
/// enabling SMP requires synchronization with concurrent page-table updates.
fn write_l1_linear_u64(linear: u64, value: u64) -> Result<(), DataFault> {
    let state = l1_data_access().ok_or(DataFault::InvalidState)?;
    let linear = state.operand_range(linear, 8)?;
    with_cpu_runtime(|runtime| {
        let mut addresses = [0; 8];
        for (index, physical) in addresses.iter_mut().enumerate() {
            *physical = l1_operand_physical(runtime, linear + index as u64, state, true)?;
        }
        for (index, physical) in addresses.into_iter().enumerate() {
            runtime
                .operand_byte(physical, Some((value >> (index * 8)) as u8))
                .ok_or(DataFault::Backing(physical))?;
        }
        Ok(())
    })
    .ok_or(DataFault::InvalidState)?
}

/// Delivers the precise synchronous operand exception without advancing RIP.
fn inject_l1_operand_fault(
    fault: DataFault,
    reason: u64,
    qualification: u64,
    guest_rip: u64,
    instruction_len: u64,
    registers: &GuestRegisters,
) {
    let (vector, error) = match fault {
        DataFault::Address => {
            // SAFETY: carrier exit information identifies the faulting L1
            // instruction. Long mode ignores segment limits, but SS retains
            // its distinct noncanonical-address #SS(0) exception.
            let information = unsafe { vmcs_read(vmcs::VMX_INSTRUCTION_INFO) };
            let Ok(information) = information else {
                stop_unexpected_exit(
                    b"operand segment unavailable",
                    reason,
                    qualification,
                    guest_rip,
                    instruction_len,
                    registers,
                );
            };
            (if (information >> 15) & 7 == 2 { 12 } else { 13 }, 0)
        }
        DataFault::Page { linear, error } => {
            // SAFETY: CR2 is not switched by VMX and L0 does not use it. Publish
            // the faulting L1 address immediately before injecting its #PF;
            // no intervening L0 data access may fault in a valid private map.
            unsafe {
                core::arch::asm!("mov cr2, {linear}", linear = in(reg) linear, options(nostack, preserves_flags));
            }
            (14, u64::from(error))
        }
        DataFault::InvalidState | DataFault::Backing(_) => {
            // A missing L0 physical mapping is not an architectural guest #PF.
            // Unknown physical holes/private backing or an invalid L0 context
            // remain distinct from a fabricated nonpresent guest page-table entry.
            stop_unexpected_exit(
                b"L0 operand backing/context unavailable",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }
    };
    // SAFETY: this BSP owns the current carrier; the hardware-exception event
    // has an error code and preserves the faulting RIP and guest VMX flags.
    let injected = unsafe {
        vmcs_write(vmcs::VM_ENTRY_EXCEPTION_ERROR_CODE, error) == VmxStatus::Success
            && vmcs_write(
                vmcs::VM_ENTRY_INTR_INFO_FIELD,
                (1 << 31) | (1 << 11) | (3 << 8) | vector,
            ) == VmxStatus::Success
    };
    if !injected {
        stop_unexpected_exit(
            b"injecting L1 operand fault failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    }
}

/// Firmware-side discovery on the calling CPU. Runtime reads the checked width
/// from its own CpuMonitor, not a mutable cross-CPU CPUID cache.
fn max_physical_address_bits() -> Option<u8> {
    let bits = if cpu::cpuid(0x8000_0000, 0).eax < 0x8000_0008 {
        36
    } else {
        u8::try_from(cpu::cpuid(0x8000_0008, 0).eax & 0xff).ok()?
    };
    if !(12..=52).contains(&bits) {
        return None;
    }
    Some(bits)
}

/// Validates the VMXON GPA and its direct-hardware revision identifier.
fn validate_l1_vmxon_region(address: u64) -> Option<VmxonPhys> {
    let expected = with_cpu_runtime(|state| {
        state
            .allows_vmx_region(address)
            .then_some(state.basic.revision_id)
    })??;
    let region = VmxonPhys::new(address)?;
    let revision: u32;
    // SAFETY: the entire aligned region is validated foreign RAM. This integer
    // address load permits a valid VMXON page at zero without a Rust null pointer.
    unsafe {
        core::arch::asm!("mov {revision:e}, [{address}]", revision = out(reg) revision, address = in(reg) address, options(nostack, preserves_flags));
    }
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
    if let Some(error) = result.instruction_error() {
        if publish_l1_instruction_error(error).is_none() {
            stop_unexpected_exit(
                b"publishing L1 VM-instruction error failed",
                reason,
                qualification,
                guest_rip,
                instruction_len,
                registers,
            );
        }
    }
    // SAFETY: instruction/error completion has restored this CPU's stopped
    // carrier. This is its current L1 flags value, never a previous-exit cache.
    let Some(rflags) = (unsafe { vmcs_read(vmcs::GUEST_RFLAGS) }).ok() else {
        stop_unexpected_exit(
            b"VMREAD(GUEST_RFLAGS) failed",
            reason,
            qualification,
            guest_rip,
            instruction_len,
            registers,
        );
    };
    let completed = result.apply_to_rflags(rflags);
    // SAFETY: no guest execution or VMCS switch occurs after the read above.
    // GUEST_RFLAGS is a mandatory supported field; an identical write has no
    // architectural side effect. Error publication and RIP advancement remain
    // unconditional, and changed flags still take the hardware error path.
    if completed != rflags
        && unsafe { vmcs_write(vmcs::GUEST_RFLAGS, completed) } != VmxStatus::Success
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
        4 => unsafe { vmcs_read(vmcs::GUEST_RSP) }.ok()?,
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
        4 => return unsafe { vmcs_write(vmcs::GUEST_RSP, value) } == VmxStatus::Success,
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

/// Reads L1's architectural CR4, including bits L0 masks through a read shadow.
fn l1_visible_cr4() -> Option<u64> {
    // SAFETY: only the BSP's L1-exit dispatcher calls this with its carrier
    // VMCS current; these VMREADs cannot access another CPU's VMCS state.
    unsafe {
        Some(xstate::visible_cr(
            vmcs_read(vmcs::GUEST_CR4).ok()?,
            vmcs_read(vmcs::CR4_GUEST_HOST_MASK).ok()?,
            vmcs_read(vmcs::CR4_READ_SHADOW).ok()?,
        ))
    }
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
        // SAFETY: the BSP's carrier VMCS is current. The fields describe a
        // hardware #GP(0) on the unchanged L1 RIP, not an L0 exception.
        let status = unsafe { vmcs_write(field, value) };
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
    // SAFETY: the BSP's carrier VMCS is current; this is a hardware #UD event
    // with no error code and does not change the faulting L1 RIP.
    let status = unsafe { vmcs_write(vmcs::VM_ENTRY_INTR_INFO_FIELD, INJECT_INVALID_OPCODE) };
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
    let status = unsafe { vmcs_write(vmcs::GUEST_RIP, next_rip) };
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
    let cpuid_exits = cpuid_exit_count();
    let mut serial = SerialPort;
    serial.init();
    serial.write_bytes(b"thin-hv: VMEXIT");
    write_raw_field(&mut serial, b"cpu", u64::from(cpu_id));
    write_raw_field(&mut serial, b"level", 1);
    write_raw_field(&mut serial, b"reason", reason);
    write_raw_field(&mut serial, b"qualification", qualification);
    write_raw_field(&mut serial, b"guest_rip", guest_rip);
    write_raw_field(&mut serial, b"instruction_len", instruction_len);
    write_raw_field(&mut serial, b"cpuid_exits", cpuid_exits);
    write_raw_newline(&mut serial);
}

/// Completes the bounded smoke test after the guest's VMCALL.
fn finish_vmcall(reason: u64) -> ! {
    record_diagnostic(DiagnosticEvent::GuestReturned);
    let (marker, guest_status) = with_cpu_runtime(|state| state.bootstrap)
        .flatten()
        .map(|bootstrap| {
            // SAFETY: pre-entry PE/firmware validation proved these distinct aligned
            // atomics belong to retained, writable L1 bootstrap data, mapped by the
            // private RAM root. L1 is stopped on this CPU. Load shared results, never
            // the unrelated copied L0 statics; no borrow survives this expression.
            unsafe {
                (
                    (*(bootstrap.marker as *const AtomicU64)).load(Ordering::Acquire),
                    (*(bootstrap.status as *const AtomicUsize)).load(Ordering::Acquire),
                )
            }
        })
        .unwrap_or((0, usize::MAX));
    #[cfg(feature = "host-exception-test")]
    if reason & 0xffff == EXIT_REASON_VMCALL
        && marker == GUEST_MARKER
        && guest_status == efi::Status::SUCCESS.as_usize()
    {
        let mut serial = SerialPort;
        serial.init();
        serial.write_bytes(b"thin-hv: host exception test guest returned\n");
        serial.write_bytes(b"thin-hv: host exception test armed\n");
        // SAFETY: this separately built QEMU-only fault fixture deliberately
        // raises #UD in VMX root after a successful guest return. The private
        // IDT/IST handler must stop without returning or invoking firmware.
        unsafe { core::arch::asm!("ud2", options(noreturn, nomem, nostack)) };
    }
    let vmxoff = leave_vmx();
    let mut serial = SerialPort;
    serial.init();
    log_diagnostic_summary(&mut serial);
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
    halt_with_guest_xstate()
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
    let instruction_info = unsafe { vmcs_read(vmcs::VMX_INSTRUCTION_INFO) }.unwrap_or(u64::MAX);
    let cpuid_exits = cpuid_exit_count();
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
    write_raw_field(&mut serial, b"cpuid_exits", cpuid_exits);
    write_raw_field(&mut serial, b"rax", registers.rax);
    write_raw_field(&mut serial, b"rbx", registers.rbx);
    write_raw_field(&mut serial, b"rcx", registers.rcx);
    write_raw_field(&mut serial, b"rdx", registers.rdx);
    if reason & 0xffff == 48 {
        // SAFETY: the current VMCS just reported an EPT violation. Its GPA is
        // architecturally defined; GLA is valid only when qualification bit 7
        // is set. Read addresses only, never guest or firmware payload bytes.
        let (physical, linear) = unsafe {
            (
                vmcs_read(vmcs::GUEST_PHYSICAL_ADDRESS).ok(),
                (qualification & (1 << 7) != 0)
                    .then(|| vmcs_read(vmcs::GUEST_LINEAR_ADDRESS).ok())
                    .flatten(),
            )
        };
        if let Some(address) = physical {
            write_raw_field(&mut serial, b"ept_gpa", address);
        }
        if let Some(address) = linear {
            write_raw_field(&mut serial, b"ept_gla", address);
        }
        write_raw_field(&mut serial, b"rsi", registers.rsi);
        write_raw_field(&mut serial, b"rdi", registers.rdi);
    }
    write_raw_newline(&mut serial);
    let vmxoff = leave_vmx();
    serial.write_bytes(b"thin-hv: VMXOFF status=");
    write_raw_vmx_status(&mut serial, vmxoff);
    write_raw_newline(&mut serial);
    halt_with_guest_xstate()
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
    let reason = unsafe { vmcs_read(vmcs::VM_EXIT_REASON) }.unwrap_or(u64::MAX);
    let qualification = unsafe { vmcs_read(vmcs::EXIT_QUALIFICATION) }.unwrap_or(u64::MAX);
    let guest_rip = unsafe { vmcs_read(vmcs::GUEST_RIP) }.unwrap_or(u64::MAX);
    let instruction_len = unsafe { vmcs_read(vmcs::VM_EXIT_INSTRUCTION_LEN) }.unwrap_or(u64::MAX);
    let cpuid_exits = cpuid_exit_count();
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
    write_raw_field(&mut serial, b"cpuid_exits", cpuid_exits);
    write_raw_field(&mut serial, b"rax", registers.rax);
    write_raw_field(&mut serial, b"rbx", registers.rbx);
    write_raw_field(&mut serial, b"rcx", registers.rcx);
    write_raw_field(&mut serial, b"rdx", registers.rdx);
    write_raw_newline(&mut serial);
    let vmxoff = leave_vmx();
    serial.write_bytes(b"thin-hv: VMXOFF status=");
    write_raw_vmx_status(&mut serial, vmxoff);
    write_raw_newline(&mut serial);
    halt_with_guest_xstate()
}

/// Final post-exit action: retain private maps/tables and restore the last live
/// guest FP state after all diagnostics. Never used on initial launch failure,
/// where firmware still owns GS and no exit snapshot exists.
// SAFETY: only post-VM-exit terminal paths call this. HOST_GS_BASE and its
// reserved writable scratch remain installed even after VMXOFF; CR0.EM/TS=0
// and CR4.OSFXSR=1 remain private. No Rust runs after restoring guest registers.
#[unsafe(naked)]
extern "sysv64" fn halt_with_guest_xstate() -> ! {
    core::arch::naked_asm!("fxrstor64 gs:[0]", "cli", "2:", "hlt", "jmp 2b");
}

/// Tests the live scratch map while the assembly stub protects guest XSTATE.
/// Reads only q35's immutable host-bridge identity and an absent PCI function.
/// This explicit QEMU-only fixture must never run on a physical motherboard.
#[cfg(feature = "host-xstate-test")]
fn probe_q35_host_window() -> bool {
    let passed = with_cpu_runtime(|state| {
        let window = state.window?;
        // q35/OVMF regression layout only, not a production resource discovery
        // rule. The real inventory must independently classify each byte MMIO.
        for (address, expected) in [
            (0xe000_0000, 0x29c0_8086_u32),
            (0xe000_1000, u32::MAX),
            (0xe000_0000, 0x29c0_8086),
        ] {
            let mut value = 0_u32;
            for index in 0..4 {
                let physical = address + index;
                if state.operand_backing(physical, false) != Some(true) {
                    return None;
                }
                value |= u32::from(state.operand_byte(physical, None)?) << (index * 8);
                // SAFETY: the owning CPU retains this aligned WB private PTE;
                // operand_byte ended its temporary mapping and no table borrow
                // or other CPU aliases the entry. This checks cleanup, not MMIO.
                if unsafe { ptr::read_volatile(window.pte_address() as *const u64) } != 0 {
                    return None;
                }
            }
            if value != expected {
                return None;
            }
        }
        Some(())
    }) == Some(Some(()));
    if passed {
        // One fixed record on the first CPUID of an explicit test build only.
        // No product identifiers or device contents are emitted or modified.
        SerialPort.write_bytes(
            b"thin-hv: host MMIO window PASS reads=12 mappings=12 pages=2 returns=1 pte_clear=1\n",
        );
    }
    passed
}

/// Test-only deliberate corruption after the assembly stub saved live XSTATE.
#[cfg(feature = "host-xstate-test")]
extern "sysv64" fn clobber_host_xmm() {
    // SAFETY: called only inside an exit's saved-state bracket with OSFXSR=1
    // and masked private MXCSR. All clobbers are declared; no AVX instruction
    // or guest memory is used. The outer stub restores all sixteen registers.
    unsafe {
        core::arch::asm!(
            "pxor xmm0, xmm0", "pxor xmm1, xmm1", "pxor xmm2, xmm2", "pxor xmm3, xmm3",
            "pxor xmm4, xmm4", "pxor xmm5, xmm5", "pxor xmm6, xmm6", "pxor xmm7, xmm7",
            "pxor xmm8, xmm8", "pxor xmm9, xmm9", "pxor xmm10, xmm10", "pxor xmm11, xmm11",
            "pxor xmm12, xmm12", "pxor xmm13, xmm13", "pxor xmm14, xmm14", "pxor xmm15, xmm15",
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
            options(nomem, nostack, preserves_flags),
        );
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

/// Leaves VMX operation, retaining the private host environment on terminal
/// post-exit paths. Only initial launch failure may restore firmware controls.
fn leave_vmx() -> VmxStatus {
    // SAFETY: the owning BSP enabled VMXON and has not left VMX. On failure all
    // callers retain the allocation; this does not free or restore host state.
    unsafe { vmx::vmxoff() }
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_driver_rejects_disposable_code_or_data_before_handoff() {
        use r_efi::efi;
        for code in [
            efi::LOADER_CODE,
            efi::LOADER_DATA,
            efi::BOOT_SERVICES_CODE,
            efi::BOOT_SERVICES_DATA,
            efi::RUNTIME_SERVICES_CODE,
            efi::RUNTIME_SERVICES_DATA,
            u32::MAX,
        ] {
            for data in [
                efi::LOADER_DATA,
                efi::BOOT_SERVICES_DATA,
                efi::RUNTIME_SERVICES_CODE,
                efi::RUNTIME_SERVICES_DATA,
                u32::MAX,
            ] {
                assert_eq!(
                    super::runtime_image_types(code, data),
                    code == efi::RUNTIME_SERVICES_CODE && data == efi::RUNTIME_SERVICES_DATA
                );
            }
        }
    }

    #[test]
    fn runtime_handoff_rejects_cross_backend_profiles_and_null_guest() {
        for mode in [0, 1, 2, u32::MAX] {
            for profile in [0, 1, 2, 3, u32::MAX] {
                // Opaque non-null token only: this pure validator never accesses
                // the represented image, protocol, or any physical memory.
                let guest = 1_usize as super::efi::Handle;
                let valid = mode == super::RUNTIME_MODE
                    && if cfg!(feature = "physical-direct-vmx") {
                        profile == 0
                    } else {
                        matches!(profile, 1 | 2)
                    };
                assert_eq!(
                    super::validated_runtime_handoff(super::RuntimeHandoff {
                        guest,
                        profile,
                        mode
                    })
                    .is_some(),
                    valid
                );
                assert!(
                    super::validated_runtime_handoff(super::RuntimeHandoff {
                        guest: core::ptr::null_mut(),
                        profile,
                        mode
                    })
                    .is_none()
                );
            }
        }
    }

    #[test]
    fn monitor_error_page_requires_disjoint_complete_writable_runtime_ram() {
        use super::FirmwareDescriptor;
        use r_efi::efi;

        let base = 0x10_0000;
        let mut region = FirmwareDescriptor {
            memory_type: efi::RUNTIME_SERVICES_DATA,
            physical_start: base,
            number_of_pages: super::MONITOR_ALLOCATION_PAGES as u64,
            attributes: efi::MEMORY_WB,
        };
        assert!(super::monitor_allocation_is_wb(&[region], base));
        assert!(!super::monitor_allocation_is_wb(&[region, region], base));
        region.number_of_pages -= 1;
        assert!(!super::monitor_allocation_is_wb(&[region], base));
        region.number_of_pages += 1;
        for attributes in [
            0,
            efi::MEMORY_WB | efi::MEMORY_RO,
            efi::MEMORY_WB | efi::MEMORY_RP,
            efi::MEMORY_WB | efi::MEMORY_WP,
        ] {
            region.attributes = attributes;
            assert!(!super::monitor_allocation_is_wb(&[region], base));
        }
        region.attributes = efi::MEMORY_WB;
        region.memory_type = efi::BOOT_SERVICES_DATA;
        assert!(!super::monitor_allocation_is_wb(&[region], base));
        region.memory_type = efi::RUNTIME_SERVICES_DATA;
        for base in [0, 1, super::IDENTITY_MAP_LIMIT, u64::MAX - 4095] {
            region.physical_start = base;
            assert!(!super::monitor_allocation_is_wb(&[region], base));
        }
        region.physical_start = 1 << 32;
        assert!(super::monitor_allocation_is_wb(&[region], 1 << 32));
        for base in [1 << 31, 6 << 30, 16 << 30, 1 << 40] {
            region.physical_start = base;
            assert!(super::monitor_allocation_is_wb(&[region], base));
        }
        assert!(super::ERROR_REVISION_PAGE > 1);
        assert_eq!(super::ERROR_REVISION_PAGE + 1, super::CPU_STATE_FIRST_PAGE);
        assert_eq!(
            super::CPU_STATE_FIRST_PAGE + super::CPU_STATE_PAGES as u64,
            super::MONITOR_PAGES as u64
        );
    }

    #[test]
    fn physical_backing_keeps_holes_mmio_and_monitor_pages_out_of_ram_accesses() {
        let width = super::PhysicalWidth::new(48).unwrap();
        let block = 0x100000;
        let end = block + super::MONITOR_PAGES as u64 * 4096;
        let high = 16 << 30;
        let ram = super::FirmwareMap::new(
            &[
                super::FirmwareDescriptor {
                    memory_type: 7,
                    physical_start: 0,
                    number_of_pages: 2,
                    attributes: 8,
                },
                super::FirmwareDescriptor {
                    memory_type: 6,
                    physical_start: block,
                    number_of_pages: super::MONITOR_PAGES as u64,
                    attributes: 8 | (1 << 63),
                },
                super::FirmwareDescriptor {
                    memory_type: 7,
                    physical_start: high,
                    number_of_pages: 2,
                    attributes: 8,
                },
                super::FirmwareDescriptor {
                    memory_type: 9,
                    physical_start: high + 8192,
                    number_of_pages: 1,
                    attributes: 8 | 0x20000,
                },
                super::FirmwareDescriptor {
                    memory_type: 7,
                    physical_start: high + 12288,
                    number_of_pages: 1,
                    attributes: 8 | 0x2000,
                },
            ],
            width,
        )
        .unwrap();
        let mut mmio = super::MmioMap::empty(width).unwrap();
        let device = 56 << 40;
        mmio.insert(
            super::PhysicalRange::new(device, device + 4096, width).unwrap(),
            width,
        )
        .unwrap();
        let mut state = super::CpuRuntimeState::new(
            ram,
            super::vmx::VmxBasic::from_msr(0),
            [(block, end), (high, high + 4096)],
            mmio,
            None,
            None,
        );
        assert!(state.allows_vmx_region(high + 4096));
        state.basic.physical_address_width_32 = true;
        assert!(!state.allows_vmx_region(high + 4096));
        assert!(state.allows_list_access(high + 4096, 4096, true));
        assert!(state.allows_vmx_region(0));
        assert!(!state.allows_vmx_region(1));
        for page in [1, super::ERROR_REVISION_PAGE] {
            let address = block + page * 4096;
            assert!(state.private_vmcs(address).is_some());
            assert!(!state.allows_list_access(address, 4096, true));
        }
        for address in [
            block,
            block + super::EPT_FIRST_PAGE * 4096,
            high + 4096,
            end,
            u64::MAX,
        ] {
            assert!(state.private_vmcs(address).is_none());
        }
        for write in [false, true] {
            assert_eq!(state.operand_backing(0, write), Some(false));
            assert_eq!(state.operand_backing(high + 4096, write), Some(false));
            assert_eq!(state.operand_backing(device, write), Some(true));
            for address in [
                8192,
                block,
                end - 1,
                high,
                high + 12288,
                device + 4096,
                u64::MAX,
            ] {
                assert_eq!(state.operand_backing(address, write), None, "{address:#x}");
            }
            let stack = block + super::GUEST_STACK_PAGE * 4096;
            assert!(state.allows_operand_ram(stack, super::GUEST_STACK_PAGES * 4096, write));
            assert!(!state.allows_operand_ram(stack - 1, 2, write));
            assert!(!state.allows_list_access(stack, 16, write));
            assert!(!state.allows_list_access(device, 8, write));
            assert!(!state.allows_operand_ram(high + 8191, 2, true));
        }
        assert_eq!(state.operand_backing(high + 8192, false), Some(false));
        assert_eq!(state.operand_backing(high + 8192, true), None);
        assert_eq!(state.paging_word(device, 0), None);
    }

    #[test]
    fn cpu_monitor_state_is_bounded_disjoint_and_initially_empty() {
        let make = || {
            let ram = super::FirmwareMap::new(
                &[super::FirmwareDescriptor {
                    memory_type: 7,
                    physical_start: 0,
                    number_of_pages: 16,
                    attributes: super::efi::MEMORY_WB,
                }],
                super::PhysicalWidth::new(48).unwrap(),
            )
            .unwrap();
            let mmio = super::MmioMap::empty(ram.physical_width()).unwrap();
            super::CpuRuntimeState::new(
                ram,
                super::vmx::VmxBasic::from_msr(0),
                [(0x3000, 0x4000), (0x5000, 0x5101)],
                mmio,
                None,
                None,
            )
        };
        let mut first = make();
        let second = make();
        assert!(!core::ptr::eq(&first, &second));
        for state in [&first, &second] {
            assert_eq!(state.inherited, super::PatEfer { pat: 0, efer: 0 });
            assert_eq!(state.entry_count, 0);
            assert_eq!(state.entry.as_ptr() as usize & 15, 0);
            assert!(
                state
                    .entry
                    .iter()
                    .all(|entry| *entry == super::MsrEntry::new(0, 0))
            );
            for (address, bytes) in [(0, 16), (0x2ff0, 16), (0x4000, 16), (0x5110, 16)] {
                assert!(state.allows_list_access(address, bytes, false));
            }
            for (address, bytes) in [
                (0, 0),
                (0x2ff0, 32),
                (0x3000, 16),
                (0x5000, 16),
                (0x5100, 16),
                (0x10000, 16),
                (u64::MAX, 2),
                (super::IDENTITY_MAP_LIMIT, 16),
            ] {
                assert!(!state.allows_list_access(address, bytes, false));
            }
        }
        assert_eq!(
            first.prepare_entry(super::MsrList::new(u64::MAX, 0, 48).unwrap()),
            Some((0, 0))
        );
        assert_eq!(
            first.prepare_entry(super::MsrList::new(0x3000, 1, 48).unwrap()),
            None
        );
        let owner = super::VmcsPhys::new(0x2000).unwrap();
        let empty = super::MsrList::new(u64::MAX, 0, 48).unwrap();
        first.host_load[0] = super::MsrEntry::new(0x277, 0x0606);
        for (pending_owner, pending_count) in [(Some(owner), 0), (None, 1), (Some(owner), 1)] {
            first.host_owner = pending_owner;
            first.host_count = pending_count;
            assert_eq!(first.prepare_host_load(empty, owner), None);
            assert_eq!(first.host_owner, pending_owner);
            assert_eq!(first.host_count, pending_count);
        }
        // Models completion's metadata cleanup; native MSR tests also verify
        // actual carrier entries with alternating nonempty/empty host lists.
        first.host_owner = None;
        first.host_count = 0;
        for _ in 0..3 {
            assert_eq!(first.prepare_host_load(empty, owner), Some((0, 0)));
            assert_eq!(first.host_owner, None);
            assert_eq!(first.host_count, 0);
            assert_eq!(first.host_load[0], super::MsrEntry::new(0x277, 0x0606));
        }
        assert_eq!(second.host_owner, None);
        assert_eq!(second.host_count, 0);
        assert_ne!(first.entry.as_ptr(), second.entry.as_ptr());
        assert!(core::mem::size_of::<super::CpuMonitor>() <= super::CPU_STATE_PAGES * 4096);
        let limits = super::host_validation::Limits {
            cr0_fixed0: 0x80000021,
            cr0_fixed1: u32::MAX.into(),
            cr4_fixed0: 1 << 13,
            cr4_fixed1: 0x3f_ffff,
            physical_bits: first.ram.physical_width().bits(),
            linear_bits: 48,
            lam: false,
            efer_allowed: 0xd01,
            cet_allowed: 0,
            l1_ia32e: false,
        };
        let second_limits = super::host_validation::Limits {
            physical_bits: second.ram.physical_width().bits(),
            linear_bits: 57,
            lam: true,
            cet_allowed: 3,
            ..limits
        };
        let first = super::CpuMonitor::new(first, limits);
        let second = super::CpuMonitor::new(second, second_limits);
        for efer in [0, 0x100, 0x500, 0xd01, 0x800, 0x400, 0] {
            for owner in [&first, &second] {
                let entry = owner.host_limits_for_efer(efer);
                assert_eq!(entry.l1_ia32e, efer & 0x400 != 0);
                assert!(!owner.host_limits.l1_ia32e);
                assert_eq!(entry.physical_bits, owner.physical_bits);
                assert_eq!(entry.cr0_fixed0, limits.cr0_fixed0);
                assert_eq!(entry.cr0_fixed1, limits.cr0_fixed1);
                assert_eq!(entry.cr4_fixed0, limits.cr4_fixed0);
                assert_eq!(entry.cr4_fixed1, limits.cr4_fixed1);
                assert_eq!(entry.efer_allowed, limits.efer_allowed);
                assert_eq!(entry.linear_bits, owner.host_limits.linear_bits);
                assert_eq!(entry.lam, owner.host_limits.lam);
                assert_eq!(entry.cet_allowed, owner.host_limits.cet_allowed);
            }
        }
        assert_eq!(first.host_limits.linear_bits, 48);
        assert_eq!(second.host_limits.linear_bits, 57);
        let vmxon = super::VmxonPhys::new(0x1000).unwrap();
        let direct = super::VmcsPhys::new(0x2000).unwrap();
        first.vcpu.lock().record_vmxon_success(vmxon);
        first.vcpu.lock().record_vmptrld_success(direct);
        let saved = [0; super::DIRECT_VMCS_PATCH_MANIFEST.len()];
        *first.carrier_patch.lock() = Some(saved);
        *first.direct_patch.lock() = Some((direct, saved));
        *first.entry_policy.lock() = Some((direct, 0));
        *first.nested_run.lock() = Some(super::NestedRun {
            carrier: super::VmcsPhys::new(0x3000).unwrap(),
            direct,
            saved_direct: saved,
            l1_interruptibility: 0,
            outer_reason: 20,
            outer_qualification: 0,
            outer_rip: 0,
            outer_instruction_len: 3,
        });
        // VMCS telemetry must not recursively acquire MSR/runtime metadata.
        let first_runtime = first.runtime.lock();
        first
            .diagnostics
            .lock()
            .record(super::DiagnosticEvent::L1Exit(10));
        assert_eq!(
            first.physical_bits,
            first_runtime.ram.physical_width().bits()
        );
        assert_eq!(first.diagnostics.lock().values.cpuid_exits, 1);
        assert_eq!(second.diagnostics.lock().values.cpuid_exits, 0);
        assert!(first.vcpu.lock().current_vmcs().is_some());
        assert!(second.vcpu.lock().current_vmcs().is_none());
        assert!(second.carrier_patch.lock().is_none());
        assert!(second.direct_patch.lock().is_none());
        assert!(second.entry_policy.lock().is_none());
        assert!(second.nested_run.lock().is_none());
        assert_ne!(
            first_runtime.entry.as_ptr(),
            second.runtime.lock().entry.as_ptr()
        );
        assert_ne!(
            core::ptr::from_ref(&*first.diagnostics.lock()),
            core::ptr::from_ref(&*second.diagnostics.lock())
        );
    }

    #[test]
    fn paging_ad_update_preserves_concurrent_foreign_word_updates() {
        use std::sync::Barrier;
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;

        // A host-owned atomic word stands in for foreign aligned RAM. This test
        // executes only the scalar memory-access helper, never VMX/CR/MSR/GS.
        let word = AtomicU64::new(3);
        let address = core::ptr::from_ref(&word) as usize as u64;
        let width = super::PhysicalWidth::new(48).unwrap();
        let ram = super::FirmwareMap::new(
            &[super::FirmwareDescriptor {
                memory_type: 7,
                physical_start: address & !4095,
                number_of_pages: 1,
                attributes: super::efi::MEMORY_WB,
            }],
            width,
        )
        .unwrap();
        let state = super::CpuRuntimeState::new(
            ram,
            super::vmx::VmxBasic::from_msr(0),
            [(0x1000, 0x2000), (0x3000, 0x4000)],
            super::MmioMap::empty(width).unwrap(),
            None,
            None,
        );
        const ITERATIONS: u64 = 65_536;
        assert_eq!(state.paging_word(address, 1), None);
        assert_eq!(state.paging_word(address, u64::MAX), None);
        assert_eq!(state.paging_word(address + 1, 1 << 5), None);
        assert_eq!(word.load(Ordering::SeqCst), 3);
        let start = Barrier::new(2);
        std::thread::scope(|threads| {
            threads.spawn(|| {
                start.wait();
                for _ in 0..ITERATIONS {
                    word.fetch_add(1 << 12, Ordering::SeqCst);
                }
            });
            start.wait();
            for _ in 0..ITERATIONS {
                assert!(state.paging_word(address, (1 << 5) | (1 << 6)).is_some());
            }
        });
        assert_eq!(word.load(Ordering::SeqCst), (ITERATIONS << 12) | 0x63);
        assert_eq!(
            state.paging_word(address, 0),
            Some((ITERATIONS << 12) | 0x63)
        );
    }

    #[test]
    fn vmx_failure_requires_l1_current_vmcs_not_the_hardware_carrier() {
        use nested_vmx::VcpuState;
        use nested_vmx::VmInstructionResult;
        use x86_64_hal::addr::VmcsPhys;
        use x86_64_hal::addr::VmxonPhys;

        let mut state = VcpuState::new();
        let vmcs = VmcsPhys::new(0x2000).unwrap();
        state.record_vmxon_success(VmxonPhys::new(0x1000).unwrap());
        for error in [2, 3, 9, 10, 11, 12, 13, 15, 28] {
            assert_eq!(
                super::l1_vmx_failure(&state, error),
                VmInstructionResult::VmfailInvalid
            );
        }
        state.record_vmptrld_success(vmcs);
        for error in [2, 3, 9, 10, 11, 12, 13, 15, 28] {
            assert_eq!(
                super::l1_vmx_failure(&state, error),
                VmInstructionResult::VmfailValid(error)
            );
        }
        state.record_vmclear_success(vmcs);
        assert_eq!(
            super::l1_vmx_failure(&state, 15),
            VmInstructionResult::VmfailInvalid
        );
    }

    #[test]
    fn vmcs_read_only_classification_does_not_imply_field_existence() {
        use x86_64_hal::vmcs;

        for field in [
            vmcs::VM_INSTRUCTION_ERROR,
            vmcs::VM_EXIT_REASON,
            vmcs::EXIT_QUALIFICATION,
        ] {
            assert!(super::vmcs_field_is_read_only(field));
            assert!(super::vmcs_field_is_read_only(field | (1 << 15)));
        }
        for field in [
            vmcs::GUEST_RIP,
            vmcs::HOST_RIP,
            vmcs::VM_ENTRY_INTR_INFO_FIELD,
        ] {
            assert!(!super::vmcs_field_is_read_only(field));
        }
    }

    use super::DIRECT_VMCS_PATCH_MANIFEST;
    use super::DiagnosticEvent;
    use super::ExitCounterValues;
    use super::ExitDiagnostics;
    use super::INJECT_EXTERNAL_INTERRUPT;
    use super::VmcsField;
    use super::acknowledged_external_interrupt;
    use super::max_physical_address_bits;
    use super::read_direct_patch_field;
    use super::write_direct_patch_field;
    use core::sync::atomic::Ordering;

    #[test]
    fn direct_manifest_replaces_every_initialized_private_host_field_once() {
        use nested_vmx::PatchKind;
        use x86_64_hal::host_state::HOST_ENVIRONMENT_BYTES;
        use x86_64_hal::host_state::HOST_PAGE_BYTES;
        use x86_64_hal::host_state::HostEnvironment;
        use x86_64_hal::host_state::HostStack;

        #[repr(C, align(4096))]
        struct TablesAndIst([u8; HOST_ENVIRONMENT_BYTES]);
        #[repr(C, align(4096))]
        struct NormalStack([u8; 4 * HOST_PAGE_BYTES]);

        let mut storage = TablesAndIst([0; HOST_ENVIRONMENT_BYTES]);
        let mut stack = NormalStack([0; 4 * HOST_PAGE_BYTES]);
        let host_stack =
            HostStack::new(stack.0.as_mut_ptr() as usize as u64, stack.0.len() as u64).unwrap();
        // SAFETY: these distinct, page-aligned test buffers remain owned and
        // unmoved while the environment is inspected. No CPU or VMCS references
        // them, and this host test never installs the tables or enters the
        // exception handler. Initialization only writes the owned storage.
        let environment =
            unsafe { HostEnvironment::initialize(&mut storage.0, host_stack, 48) }.unwrap();
        let mut carrier_values = [u64::MAX; DIRECT_VMCS_PATCH_MANIFEST.len()];
        for (field, host_value) in environment.vmcs_fields() {
            let mut matches = DIRECT_VMCS_PATCH_MANIFEST
                .iter()
                .enumerate()
                .filter(|(_, patch)| patch.field as u32 == field);
            let (index, patch) = matches
                .next()
                .unwrap_or_else(|| panic!("private host field {field:#x} is not patched"));
            assert!(
                matches.next().is_none(),
                "duplicate private host field {field:#x}"
            );
            assert_eq!(
                patch.kind,
                PatchKind::HostState,
                "private host field {field:#x} must copy the L0 carrier value"
            );
            assert!(write_direct_patch_field(
                &mut carrier_values,
                field,
                host_value
            ));
            assert_eq!(carrier_values[index], host_value);
            assert_eq!(
                read_direct_patch_field(&carrier_values, field),
                Some(host_value)
            );
        }
    }

    #[test]
    fn diagnostics_classify_l1_l2_and_failed_entries_without_reflection_confusion() {
        let mut diagnostics = ExitDiagnostics::new();
        for reason in [
            super::EXIT_REASON_CPUID,
            super::EXIT_REASON_INVEPT,
            super::EXIT_REASON_INVVPID,
        ] {
            diagnostics.record(DiagnosticEvent::L1Exit(reason));
            diagnostics.record(DiagnosticEvent::L0Handled {
                reason,
                action: super::VMEXIT_ACTION_RESUME,
            });
        }
        diagnostics.record(DiagnosticEvent::L1Exit(
            super::EXIT_REASON_EXTERNAL_INTERRUPT,
        ));
        diagnostics.record(DiagnosticEvent::L0Handled {
            reason: super::EXIT_REASON_VMLAUNCH,
            action: super::VMEXIT_ACTION_VMLAUNCH,
        });
        diagnostics.record(DiagnosticEvent::NestedExit(
            super::EXIT_REASON_EXTERNAL_INTERRUPT,
        ));
        diagnostics.record(DiagnosticEvent::Reflected(
            super::EXIT_REASON_EXTERNAL_INTERRUPT,
        ));
        let failed_entry = (1 << 31) | super::EXIT_REASON_INTERRUPT_WINDOW;
        diagnostics.record(DiagnosticEvent::NestedExit(failed_entry));
        diagnostics.record(DiagnosticEvent::Reflected(failed_entry));
        diagnostics.record(DiagnosticEvent::EntryFailure(super::EXIT_REASON_VMRESUME));
        diagnostics.record(DiagnosticEvent::NestedExit(u64::MAX));
        let values = diagnostics.values;
        assert_eq!(values.l1_exits, 4);
        assert_eq!(values.l0_only_handled_exits, 4);
        assert_eq!(values.direct_entry_attempts, 1);
        assert_eq!(values.observed_l2_entries, 1);
        assert_eq!(values.reflected_l2_exits, 2);
        assert_eq!(values.external_interrupt_exits, 2);
        assert_eq!(values.interrupt_window_exits, 0);
        assert_eq!(values.nested_entry_failures, 2);
        assert_eq!(
            (values.cpuid_exits, values.invept, values.invvpid),
            (1, 1, 1)
        );
        assert_eq!(values.last_reason, u64::MAX);
        assert_eq!(values.last_phase, 4);
        assert_eq!(diagnostics.sequence.load(Ordering::Relaxed) & 1, 0);
        diagnostics.record(DiagnosticEvent::NestedExit(super::EXIT_REASON_INVEPT));
        assert_eq!(
            diagnostics.values.invept, 1,
            "L2-reflected instruction is not L0's INVEPT"
        );
        diagnostics.record(DiagnosticEvent::GuestReturned);
        assert_eq!(diagnostics.values.last_phase, 7);
    }

    #[test]
    fn diagnostics_count_vmcs_attempts_without_changing_exit_context() {
        let mut diagnostics = ExitDiagnostics::new();
        diagnostics.record(DiagnosticEvent::NestedExit(48));
        for event in [
            DiagnosticEvent::VmcsRead,
            DiagnosticEvent::VmcsWrite,
            DiagnosticEvent::VmcsLoad,
            DiagnosticEvent::ReflectedStateWrite,
        ] {
            diagnostics.record(event);
        }
        let values = diagnostics.values;
        assert_eq!(values.vmread_attempts, 1);
        assert_eq!(values.vmwrite_attempts, 2);
        assert_eq!(values.vmptrld_attempts, 1);
        assert_eq!(values.reflected_state_writes, 1);
        assert_eq!((values.last_phase, values.last_reason), (4, 48));
        assert_eq!(diagnostics.sequence.load(Ordering::Relaxed), 10);
        diagnostics.record(DiagnosticEvent::VmcsAccessBatch(
            super::vmx::VmcsAccessCounts {
                reads: 2,
                writes: 3,
                loads: 4,
            },
        ));
        assert_eq!(diagnostics.values.vmread_attempts, 3);
        assert_eq!(diagnostics.values.vmwrite_attempts, 5);
        assert_eq!(diagnostics.values.vmptrld_attempts, 5);
        assert_eq!(diagnostics.values.reflected_state_writes, 1);
        assert_eq!(diagnostics.values.last_reason, 48);
        diagnostics.record(DiagnosticEvent::VmcsAccessBatch(
            super::vmx::VmcsAccessCounts {
                reads: u64::MAX,
                writes: u64::MAX,
                loads: u64::MAX,
            },
        ));
        assert_eq!(diagnostics.values.vmread_attempts, u64::MAX);
        assert_eq!(diagnostics.values.vmwrite_attempts, u64::MAX);
        assert_eq!(diagnostics.values.vmptrld_attempts, u64::MAX);
    }

    #[test]
    fn diagnostics_all_counters_saturate_without_wrapping() {
        let almost = u64::MAX - 1;
        let mut values = ExitCounterValues {
            l1_exits: almost,
            direct_entry_attempts: almost,
            observed_l2_entries: almost,
            reflected_l2_exits: almost,
            l0_only_handled_exits: almost,
            external_interrupt_exits: almost,
            interrupt_window_exits: almost,
            invept: almost,
            invvpid: almost,
            nested_entry_failures: almost,
            cpuid_exits: almost,
            vmread_attempts: almost,
            vmwrite_attempts: almost,
            vmptrld_attempts: almost,
            reflected_state_writes: almost,
            last_phase: 0,
            last_reason: u64::MAX,
            l1_reasons: [almost; super::DIAGNOSTIC_REASON_BINS],
            l2_reasons: [almost; super::DIAGNOSTIC_REASON_BINS],
            l1_vmread_hardware: [almost; super::DIAGNOSTIC_READ_FIELDS.len() + 1],
        };
        for _ in 0..3 {
            for reason in [
                super::EXIT_REASON_CPUID,
                super::EXIT_REASON_INVEPT,
                super::EXIT_REASON_INVVPID,
                super::EXIT_REASON_EXTERNAL_INTERRUPT,
                super::EXIT_REASON_INTERRUPT_WINDOW,
            ] {
                values.record(DiagnosticEvent::L1Exit(reason));
            }
            values.record(DiagnosticEvent::L0Handled {
                reason: super::EXIT_REASON_VMRESUME,
                action: super::VMEXIT_ACTION_VMRESUME,
            });
            values.record(DiagnosticEvent::NestedExit(super::EXIT_REASON_CPUID));
            values.record(DiagnosticEvent::Reflected(super::EXIT_REASON_CPUID));
            values.record(DiagnosticEvent::EntryFailure(super::EXIT_REASON_VMLAUNCH));
            for event in [
                DiagnosticEvent::VmcsRead,
                DiagnosticEvent::VmcsWrite,
                DiagnosticEvent::VmcsLoad,
                DiagnosticEvent::ReflectedStateWrite,
            ] {
                values.record(event);
            }
        }
        for counter in [
            values.l1_exits,
            values.direct_entry_attempts,
            values.observed_l2_entries,
            values.reflected_l2_exits,
            values.l0_only_handled_exits,
            values.external_interrupt_exits,
            values.interrupt_window_exits,
            values.invept,
            values.invvpid,
            values.nested_entry_failures,
            values.cpuid_exits,
            values.vmread_attempts,
            values.vmwrite_attempts,
            values.vmptrld_attempts,
            values.reflected_state_writes,
            values.l1_reasons[super::EXIT_REASON_CPUID as usize],
            values.l2_reasons[super::EXIT_REASON_CPUID as usize],
        ] {
            assert_eq!(counter, u64::MAX);
        }
    }

    #[test]
    fn diagnostics_snapshot_layout_and_sequence_exhaustion_are_fail_closed() {
        assert_eq!(core::mem::size_of::<ExitDiagnostics>(), 1344);
        assert_eq!(core::mem::offset_of!(ExitDiagnostics, sequence), 24);
        assert_eq!(core::mem::offset_of!(ExitDiagnostics, values), 40);
        assert_eq!(
            core::mem::offset_of!(ExitCounterValues, vmread_attempts),
            88
        );
        assert_eq!(core::mem::offset_of!(ExitCounterValues, last_phase), 120);
        assert_eq!(core::mem::offset_of!(ExitCounterValues, l1_reasons), 136);
        assert_eq!(core::mem::offset_of!(ExitCounterValues, l2_reasons), 656);
        assert_eq!(
            core::mem::offset_of!(ExitCounterValues, l1_vmread_hardware),
            1176
        );
        assert_eq!(super::DIAGNOSTIC_MAGIC.to_le_bytes(), *b"THVSTAT1");
        assert_eq!(super::diagnostic_next_sequence(0), Some((1, 2)));
        assert_eq!(super::diagnostic_next_sequence(1), None);
        assert_eq!(super::diagnostic_next_sequence(u64::MAX - 1), None);
        let mut diagnostics = ExitDiagnostics::new();
        diagnostics.sequence.store(u64::MAX - 3, Ordering::Relaxed);
        diagnostics.record(DiagnosticEvent::L1Exit(super::EXIT_REASON_CPUID));
        assert_eq!(diagnostics.sequence.load(Ordering::Relaxed), u64::MAX - 1);
        diagnostics.record(DiagnosticEvent::L1Exit(super::EXIT_REASON_CPUID));
        assert_eq!(diagnostics.sequence.load(Ordering::Relaxed), u64::MAX);
        diagnostics.record(DiagnosticEvent::L1Exit(super::EXIT_REASON_CPUID));
        assert_eq!(diagnostics.sequence.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(diagnostics.values.l1_exits, 3);
    }

    #[test]
    fn diagnostics_histograms_separate_layers_and_do_not_double_count_reflection() {
        let mut values = ExitCounterValues::new();
        for reason in [0, 1, 7, 25, 31, 55, 63, 64, 79, 0xffff] {
            values.record(DiagnosticEvent::L1Exit(reason));
            values.record(DiagnosticEvent::NestedExit(reason));
            values.record(DiagnosticEvent::Reflected(reason));
        }
        for reason in [u64::MAX, (1 << 31) | 33] {
            values.record(DiagnosticEvent::L1Exit(reason));
            values.record(DiagnosticEvent::NestedExit(reason));
        }
        assert_eq!(values.l1_reasons.iter().sum::<u64>(), 10);
        assert_eq!(values.l2_reasons.iter().sum::<u64>(), 10);
        assert_eq!(values.l1_reasons[25], 1);
        assert_eq!(values.l2_reasons[31], 1);
        assert_eq!(values.l1_reasons[33], 0);
        assert_eq!(values.l1_reasons[64], 3);
        assert_eq!(values.l2_reasons[64], 3);
        assert_eq!(values.observed_l2_entries, 10);
        assert_eq!(values.reflected_l2_exits, 10);
    }

    #[test]
    fn diagnostics_vmread_misses_use_exact_encodings_and_preserve_exit_phase() {
        let mut values = ExitCounterValues::new();
        values.record(DiagnosticEvent::L1Exit(super::EXIT_REASON_VMREAD));
        for (bin, field) in super::DIAGNOSTIC_READ_FIELDS.into_iter().enumerate() {
            values.record(DiagnosticEvent::L1VmreadHardware(field));
            assert_eq!(values.l1_vmread_hardware[bin], 1);
        }
        for field in [
            u32::MAX,
            super::vmcs::GUEST_RIP + 1,
            super::vmcs::GUEST_IA32_PAT + 1,
        ] {
            values.record(DiagnosticEvent::L1VmreadHardware(field));
        }
        assert_eq!(values.l1_vmread_hardware[15], 3);
        values.l1_vmread_hardware[0] = u64::MAX;
        values.record(DiagnosticEvent::L1VmreadHardware(super::vmcs::GUEST_RIP));
        assert_eq!(values.l1_vmread_hardware[0], u64::MAX);
        assert_eq!(values.last_phase, 1);
        assert_eq!(values.last_reason, super::EXIT_REASON_VMREAD);
        assert_eq!(values.vmread_attempts, 0);
    }

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
    fn physical_address_width_discovery_matches_cpuid() {
        let bits = max_physical_address_bits().expect("x86-64 physical-address width");
        assert!((12..=52).contains(&bits));
        assert_eq!(max_physical_address_bits(), Some(bits));
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
