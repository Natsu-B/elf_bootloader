#![no_std]
#![forbid(unsafe_code)]
//! Heap-free policy and state for trusted nested VMX.
//!
//! Instruction decoding, guest-memory access, and VMX instructions remain in
//! the architecture and monitor integration layers. This crate only records
//! architectural state and the deliberately small feature policy exposed to a
//! trusted L1 hypervisor.

use x86_64_hal::addr::VmcsPhys;
use x86_64_hal::addr::VmxonPhys;
use x86_64_hal::vmx;
use x86_64_hal::vmx::restrict_controls;

/// Carry flag in RFLAGS.
const RFLAGS_CF: u64 = 1 << 0;
/// Parity flag in RFLAGS.
const RFLAGS_PF: u64 = 1 << 2;
/// Auxiliary-carry flag in RFLAGS.
const RFLAGS_AF: u64 = 1 << 4;
/// Zero flag in RFLAGS.
const RFLAGS_ZF: u64 = 1 << 6;
/// Sign flag in RFLAGS.
const RFLAGS_SF: u64 = 1 << 7;
/// Overflow flag in RFLAGS.
const RFLAGS_OF: u64 = 1 << 11;
/// Arithmetic status flags VMX instructions define on completion.
const VMX_STATUS_RFLAGS: u64 =
    RFLAGS_CF | RFLAGS_PF | RFLAGS_AF | RFLAGS_ZF | RFLAGS_SF | RFLAGS_OF;

/// `VM_INSTRUCTION_ERROR` for VMCLEAR with an invalid physical address.
pub const VMXERR_VMCLEAR_INVALID_ADDRESS: u32 = 2;
/// `VM_INSTRUCTION_ERROR` for VMCLEAR targeting the active VMXON region.
pub const VMXERR_VMCLEAR_VMXON_POINTER: u32 = 3;
/// `VM_INSTRUCTION_ERROR` for VMLAUNCH with a launched current VMCS.
pub const VMXERR_VMLAUNCH_NONCLEAR_VMCS: u32 = 4;
/// `VM_INSTRUCTION_ERROR` for VMRESUME with a clear current VMCS.
pub const VMXERR_VMRESUME_NONLAUNCHED_VMCS: u32 = 5;

/// Architectural completion status of an emulated VMX instruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmInstructionResult {
    /// `VMsucceed`: CF=0 and ZF=0.
    Vmsucceed,
    /// `VMfailInvalid`: CF=1 and ZF=0.
    VmfailInvalid,
    /// `VMfailValid`: CF=0, ZF=1, and the current VMCS receives the error.
    VmfailValid(u32),
}

impl VmInstructionResult {
    /// Applies the exact VMX completion-status transformation to RFLAGS.
    ///
    /// CF, PF, AF, ZF, SF, and OF are cleared first. `VMfailInvalid` then sets
    /// CF, while `VMfailValid` sets ZF. Every other RFLAGS bit is preserved.
    #[must_use]
    pub const fn apply_to_rflags(self, rflags: u64) -> u64 {
        let cleared = rflags & !VMX_STATUS_RFLAGS;
        match self {
            Self::Vmsucceed => cleared,
            Self::VmfailInvalid => cleared | RFLAGS_CF,
            Self::VmfailValid(_) => cleared | RFLAGS_ZF,
        }
    }

    /// Returns the value for `VM_INSTRUCTION_ERROR`, when one is defined.
    #[must_use]
    pub const fn instruction_error(self) -> Option<u32> {
        match self {
            Self::VmfailValid(error) => Some(error),
            Self::Vmsucceed | Self::VmfailInvalid => None,
        }
    }
}

/// VMCS launch state maintained for the current direct VMCS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmcsLaunchState {
    /// VMCLEAR has established the clear state.
    Clear,
    /// A successful VMLAUNCH has established the launched state.
    Launched,
}

/// The VM-entry instruction requested by L1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmEntryInstruction {
    /// Enter a clear VMCS for the first time.
    Vmlaunch,
    /// Re-enter a launched VMCS.
    Vmresume,
}

/// Metadata for the VMCS currently selected by L1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CurrentVmcs {
    /// Direct hardware VMCS physical address.
    address: VmcsPhys,
    /// Clear or launched state known by the instruction adapter.
    launch_state: VmcsLaunchState,
}

impl CurrentVmcs {
    /// Returns the direct hardware VMCS address.
    #[must_use]
    pub const fn address(self) -> VmcsPhys {
        self.address
    }

    /// Returns the current launch state.
    #[must_use]
    pub const fn launch_state(self) -> VmcsLaunchState {
        self.launch_state
    }
}

/// Trusted nested-VMX state owned by one L1 virtual CPU.
///
/// VMCS launch state for a newly selected address is supplied by the direct
/// VMCS adapter. This keeps page tracking and guest-memory access out of the
/// policy crate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VcpuState {
    /// L1's active VMXON region, if it is in VMX operation.
    vmxon_region: Option<VmxonPhys>,
    /// L1's current VMCS and its known launch state.
    current_vmcs: Option<CurrentVmcs>,
}

impl VcpuState {
    /// Creates state for a virtual CPU outside VMX operation.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            vmxon_region: None,
            current_vmcs: None,
        }
    }

    /// Returns whether L1 is in VMX operation.
    #[must_use]
    pub const fn in_vmx_operation(&self) -> bool {
        self.vmxon_region.is_some()
    }

    /// Returns L1's VMXON region.
    #[must_use]
    pub const fn vmxon_region(&self) -> Option<VmxonPhys> {
        self.vmxon_region
    }

    /// Returns L1's current direct VMCS.
    #[must_use]
    pub const fn current_vmcs(&self) -> Option<CurrentVmcs> {
        self.current_vmcs
    }

    /// Records a successful VMXON.
    pub fn record_vmxon_success(&mut self, region: VmxonPhys) {
        debug_assert!(self.vmxon_region.is_none());
        self.vmxon_region = Some(region);
        self.current_vmcs = None;
    }

    /// Records a successful VMXOFF.
    pub fn record_vmxoff_success(&mut self) {
        self.vmxon_region = None;
        self.current_vmcs = None;
    }

    /// Records a successful VMPTRLD.
    ///
    /// `launch_state` comes from the adapter's direct-VMCS bookkeeping. The
    /// CPU remains the authority for field and revision validation.
    pub fn record_vmptrld_success(&mut self, address: VmcsPhys, launch_state: VmcsLaunchState) {
        debug_assert!(self.in_vmx_operation());
        debug_assert!(
            self.vmxon_region
                .is_none_or(|region| region.get() != address.get())
        );
        self.current_vmcs = Some(CurrentVmcs {
            address,
            launch_state,
        });
    }

    /// Records a successful VMCLEAR.
    ///
    /// VMCLEAR invalidates the current-VMCS pointer when it names the current
    /// VMCS. Launch state for a non-current page stays in the adapter that
    /// owns the direct VMCS pages.
    pub fn record_vmclear_success(&mut self, address: VmcsPhys) {
        if self
            .current_vmcs
            .is_some_and(|current| current.address == address)
        {
            self.current_vmcs = None;
        }
    }

    /// Checks the current VMCS launch state for VMLAUNCH or VMRESUME.
    ///
    /// The decoder must handle the non-VMX-operation #UD case before calling
    /// this function. A missing current VMCS produces `VMfailInvalid`.
    #[must_use]
    pub const fn entry_result(&self, instruction: VmEntryInstruction) -> VmInstructionResult {
        let Some(current) = self.current_vmcs else {
            return VmInstructionResult::VmfailInvalid;
        };
        match (instruction, current.launch_state) {
            (VmEntryInstruction::Vmlaunch, VmcsLaunchState::Clear)
            | (VmEntryInstruction::Vmresume, VmcsLaunchState::Launched) => {
                VmInstructionResult::Vmsucceed
            }
            (VmEntryInstruction::Vmlaunch, VmcsLaunchState::Launched) => {
                VmInstructionResult::VmfailValid(VMXERR_VMLAUNCH_NONCLEAR_VMCS)
            }
            (VmEntryInstruction::Vmresume, VmcsLaunchState::Clear) => {
                VmInstructionResult::VmfailValid(VMXERR_VMRESUME_NONLAUNCHED_VMCS)
            }
        }
    }

    /// Records a successful hardware VM entry.
    ///
    /// Returns `false` without changing state if the recorded precondition is
    /// inconsistent with a successful entry.
    pub fn record_entry_success(&mut self, instruction: VmEntryInstruction) -> bool {
        if self.entry_result(instruction) != VmInstructionResult::Vmsucceed {
            return false;
        }
        if instruction == VmEntryInstruction::Vmlaunch {
            let Some(current) = self.current_vmcs.as_mut() else {
                return false;
            };
            current.launch_state = VmcsLaunchState::Launched;
        }
        true
    }
}

/// Pin-based external-interrupt exiting.
pub const PIN_EXTERNAL_INTERRUPT_EXITING: u32 = 1 << 0;
/// Pin-based NMI exiting.
pub const PIN_NMI_EXITING: u32 = 1 << 3;
/// Pin-based posted-interrupt processing, deliberately hidden.
pub const PIN_POSTED_INTERRUPTS: u32 = 1 << 7;

/// Primary interrupt-window exiting.
pub const PRIMARY_INTERRUPT_WINDOW_EXITING: u32 = 1 << 2;
/// Primary TSC-offsetting control.
pub const PRIMARY_USE_TSC_OFFSETTING: u32 = 1 << 3;
/// Primary HLT exiting.
pub const PRIMARY_HLT_EXITING: u32 = 1 << 7;
/// Primary INVLPG exiting.
pub const PRIMARY_INVLPG_EXITING: u32 = 1 << 9;
/// Primary MWAIT exiting.
pub const PRIMARY_MWAIT_EXITING: u32 = 1 << 10;
/// Primary RDPMC exiting.
pub const PRIMARY_RDPMC_EXITING: u32 = 1 << 11;
/// Primary CR3-load exiting.
pub const PRIMARY_CR3_LOAD_EXITING: u32 = 1 << 15;
/// Primary CR3-store exiting.
pub const PRIMARY_CR3_STORE_EXITING: u32 = 1 << 16;
/// Primary CR8-load exiting.
pub const PRIMARY_CR8_LOAD_EXITING: u32 = 1 << 19;
/// Primary CR8-store exiting.
pub const PRIMARY_CR8_STORE_EXITING: u32 = 1 << 20;
/// Primary TPR shadowing, deliberately hidden with `APICv`.
pub const PRIMARY_TPR_SHADOW: u32 = 1 << 21;
/// Primary MOV-DR exiting.
pub const PRIMARY_MOV_DR_EXITING: u32 = 1 << 23;
/// Primary unconditional I/O exiting.
pub const PRIMARY_UNCONDITIONAL_IO_EXITING: u32 = 1 << 24;
/// Primary I/O-bitmap use.
pub const PRIMARY_USE_IO_BITMAPS: u32 = 1 << 25;
/// Primary MSR-bitmap use.
pub const PRIMARY_USE_MSR_BITMAPS: u32 = 1 << 28;
/// Primary MONITOR exiting.
pub const PRIMARY_MONITOR_EXITING: u32 = 1 << 29;
/// Primary secondary-control activation.
pub const PRIMARY_ACTIVATE_SECONDARY_CONTROLS: u32 = 1 << 31;

/// Secondary APIC-access virtualization, deliberately hidden.
pub const SECONDARY_VIRTUALIZE_APIC_ACCESSES: u32 = 1 << 0;
/// Secondary EPT enable.
pub const SECONDARY_ENABLE_EPT: u32 = 1 << 1;
/// Secondary x2APIC virtualization, deliberately hidden.
pub const SECONDARY_VIRTUALIZE_X2APIC: u32 = 1 << 4;
/// Secondary VPID enable, deliberately hidden.
pub const SECONDARY_ENABLE_VPID: u32 = 1 << 5;
/// Secondary unrestricted-guest enable.
pub const SECONDARY_UNRESTRICTED_GUEST: u32 = 1 << 7;
/// Secondary APIC-register virtualization, deliberately hidden.
pub const SECONDARY_APIC_REGISTER_VIRTUALIZATION: u32 = 1 << 8;
/// Secondary virtual-interrupt delivery, deliberately hidden.
pub const SECONDARY_VIRTUAL_INTERRUPT_DELIVERY: u32 = 1 << 9;
/// Secondary VMFUNC enable, deliberately hidden.
pub const SECONDARY_ENABLE_VMFUNC: u32 = 1 << 13;
/// Secondary VMCS shadowing, deliberately hidden.
pub const SECONDARY_VMCS_SHADOWING: u32 = 1 << 14;
/// Secondary PML enable, deliberately hidden.
pub const SECONDARY_ENABLE_PML: u32 = 1 << 17;
/// Secondary TSC scaling, deliberately hidden.
pub const SECONDARY_TSC_SCALING: u32 = 1 << 25;

/// VM-exit save-debug-controls control.
pub const EXIT_SAVE_DEBUG_CONTROLS: u32 = 1 << 2;
/// VM-exit host-address-space-size control.
pub const EXIT_HOST_ADDRESS_SPACE_SIZE: u32 = 1 << 9;
/// VM-exit interrupt acknowledgement.
pub const EXIT_ACKNOWLEDGE_INTERRUPT: u32 = 1 << 15;
/// VM-entry load-debug-controls control.
pub const ENTRY_LOAD_DEBUG_CONTROLS: u32 = 1 << 2;
/// VM-entry IA-32e guest-mode control.
pub const ENTRY_IA32E_MODE: u32 = 1 << 9;

/// Stock x86-64 KVM's required allowed-one pin controls.
pub const KVM_REQUIRED_PIN_CONTROLS: u32 = PIN_EXTERNAL_INTERRUPT_EXITING | PIN_NMI_EXITING;
/// Stock x86-64 KVM's required allowed-one primary controls.
pub const KVM_REQUIRED_PRIMARY_CONTROLS: u32 = PRIMARY_INTERRUPT_WINDOW_EXITING
    | PRIMARY_USE_TSC_OFFSETTING
    | PRIMARY_HLT_EXITING
    | PRIMARY_INVLPG_EXITING
    | PRIMARY_MWAIT_EXITING
    | PRIMARY_RDPMC_EXITING
    | PRIMARY_CR3_LOAD_EXITING
    | PRIMARY_CR3_STORE_EXITING
    | PRIMARY_CR8_LOAD_EXITING
    | PRIMARY_CR8_STORE_EXITING
    | PRIMARY_MOV_DR_EXITING
    | PRIMARY_UNCONDITIONAL_IO_EXITING
    | PRIMARY_MONITOR_EXITING;
/// Stock x86-64 KVM's required allowed-one VM-exit controls.
pub const KVM_REQUIRED_EXIT_CONTROLS: u32 =
    EXIT_SAVE_DEBUG_CONTROLS | EXIT_HOST_ADDRESS_SPACE_SIZE | EXIT_ACKNOWLEDGE_INTERRUPT;
/// Stock x86-64 KVM's required allowed-one VM-entry controls.
pub const KVM_REQUIRED_ENTRY_CONTROLS: u32 = ENTRY_LOAD_DEBUG_CONTROLS | ENTRY_IA32E_MODE;

/// Conservative allowed-one pin controls exposed to trusted L1.
pub const TRUSTED_PIN_CONTROLS: u32 = KVM_REQUIRED_PIN_CONTROLS;
/// Conservative allowed-one primary controls exposed to trusted L1.
pub const TRUSTED_PRIMARY_CONTROLS: u32 = KVM_REQUIRED_PRIMARY_CONTROLS
    | PRIMARY_USE_IO_BITMAPS
    | PRIMARY_USE_MSR_BITMAPS
    | PRIMARY_ACTIVATE_SECONDARY_CONTROLS;
/// Conservative allowed-one secondary controls exposed to trusted L1.
pub const TRUSTED_SECONDARY_CONTROLS: u32 = SECONDARY_ENABLE_EPT | SECONDARY_UNRESTRICTED_GUEST;
/// Conservative allowed-one VM-exit controls exposed to trusted L1.
pub const TRUSTED_EXIT_CONTROLS: u32 = KVM_REQUIRED_EXIT_CONTROLS;
/// Conservative allowed-one VM-entry controls exposed to trusted L1.
pub const TRUSTED_ENTRY_CONTROLS: u32 = KVM_REQUIRED_ENTRY_CONTROLS;

/// EPT supports four-level walks.
pub const EPT_PAGE_WALK_4: u64 = 1 << 6;
/// EPTP supports write-back memory type.
pub const EPTP_WRITE_BACK: u64 = 1 << 14;
/// INVEPT is available.
pub const EPT_INVEPT: u64 = 1 << 20;
/// Single-context INVEPT is available.
pub const EPT_INVEPT_SINGLE_CONTEXT: u64 = 1 << 25;
/// Global-context INVEPT is available.
pub const EPT_INVEPT_GLOBAL_CONTEXT: u64 = 1 << 26;
/// EPT capability bits stock KVM requires in order to enable EPT.
pub const KVM_REQUIRED_EPT_CAPABILITIES: u64 =
    EPT_PAGE_WALK_4 | EPTP_WRITE_BACK | EPT_INVEPT | EPT_INVEPT_GLOBAL_CONTEXT;
/// EPT/VPID capabilities exposed to trusted L1; upper VPID bits remain zero.
pub const TRUSTED_EPT_VPID_CAPABILITIES: u64 =
    KVM_REQUIRED_EPT_CAPABILITIES | EPT_INVEPT_SINGLE_CONTEXT;
/// VMFUNC functions exposed to trusted L1; VMFUNC is deliberately hidden.
pub const TRUSTED_VMFUNC_CAPABILITIES: u64 = 0;
/// `IA32_VMX_BASIC` fields safe for direct hardware VMCS use.
const TRUSTED_BASIC_MASK: u64 = 0x00bc_1fff_7fff_ffff;
/// `IA32_VMX_MISC` features implemented by the direct trusted path.
const TRUSTED_MISC_MASK: u64 = 0x160;

/// Restricts one VMX capability MSR to the trusted direct-VMCS contract.
///
/// The caller supplies the hardware value. `IA32_VMX_VMFUNC` and tertiary
/// controls deliberately return zero without depending on that value.
#[must_use]
pub const fn restrict_vmx_capability(msr: u32, hardware: u64) -> Option<u64> {
    match msr {
        vmx::IA32_VMX_BASIC => {
            let basic = vmx::VmxBasic::from_msr(hardware);
            if basic.region_size == 0
                || basic.region_size > 4096
                || basic.memory_type != 6
                || basic.physical_address_width_32
                || !basic.true_controls
            {
                None
            } else {
                Some(hardware & TRUSTED_BASIC_MASK)
            }
        }
        vmx::IA32_VMX_PINBASED_CTLS | vmx::IA32_VMX_TRUE_PINBASED_CTLS => {
            restrict_control_capability(hardware, TRUSTED_PIN_CONTROLS, KVM_REQUIRED_PIN_CONTROLS)
        }
        vmx::IA32_VMX_PROCBASED_CTLS | vmx::IA32_VMX_TRUE_PROCBASED_CTLS => {
            restrict_control_capability(
                hardware,
                TRUSTED_PRIMARY_CONTROLS,
                KVM_REQUIRED_PRIMARY_CONTROLS,
            )
        }
        vmx::IA32_VMX_EXIT_CTLS | vmx::IA32_VMX_TRUE_EXIT_CTLS => {
            restrict_control_capability(hardware, TRUSTED_EXIT_CONTROLS, KVM_REQUIRED_EXIT_CONTROLS)
        }
        vmx::IA32_VMX_ENTRY_CTLS | vmx::IA32_VMX_TRUE_ENTRY_CTLS => restrict_control_capability(
            hardware,
            TRUSTED_ENTRY_CONTROLS,
            KVM_REQUIRED_ENTRY_CONTROLS,
        ),
        vmx::IA32_VMX_PROCBASED_CTLS2 => restrict_control_capability(
            hardware,
            TRUSTED_SECONDARY_CONTROLS,
            TRUSTED_SECONDARY_CONTROLS,
        ),
        vmx::IA32_VMX_EPT_VPID_CAP => restrict_ept_vpid_capability(hardware),
        vmx::IA32_VMX_MISC => Some(hardware & TRUSTED_MISC_MASK),
        vmx::IA32_VMX_CR0_FIXED0
        | vmx::IA32_VMX_CR0_FIXED1
        | vmx::IA32_VMX_CR4_FIXED0
        | vmx::IA32_VMX_CR4_FIXED1
        | vmx::IA32_VMX_VMCS_ENUM => Some(hardware),
        vmx::IA32_VMX_VMFUNC | vmx::IA32_VMX_PROCBASED_CTLS3 => Some(0),
        _ => None,
    }
}

/// Restricts one hardware VMX control MSR to this crate's allowed-one policy.
///
/// Returns `None` when hardware cannot expose every control required by the
/// intended L1. Hardware-required-one controls are retained.
#[must_use]
pub const fn restrict_control_capability(
    hardware: u64,
    policy_allowed: u32,
    l1_required: u32,
) -> Option<u64> {
    let advertised = restrict_controls(hardware, policy_allowed);
    if (advertised >> 32) as u32 & l1_required == l1_required {
        Some(advertised)
    } else {
        None
    }
}

/// Restricts `IA32_VMX_EPT_VPID_CAP` to direct EPT and no VPID.
#[must_use]
pub const fn restrict_ept_vpid_capability(hardware: u64) -> Option<u64> {
    let advertised = hardware & TRUSTED_EPT_VPID_CAPABILITIES;
    if advertised & KVM_REQUIRED_EPT_CAPABILITIES == KVM_REQUIRED_EPT_CAPABILITIES {
        Some(advertised)
    } else {
        None
    }
}

/// One VM-execution control word with L1 and L0 provenance retained.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ControlProvenance {
    /// Bits requested by L1 and visible in its VMCS.
    l1_requested: u32,
    /// Extra exits or controls required only by L0.
    l0_required: u32,
}

impl ControlProvenance {
    /// Creates an effective control word without losing ownership information.
    #[must_use]
    pub const fn new(l1_requested: u32, l0_required: u32) -> Self {
        Self {
            l1_requested,
            l0_required,
        }
    }

    /// Returns the bits requested by L1.
    #[must_use]
    pub const fn l1_requested(self) -> u32 {
        self.l1_requested
    }

    /// Returns the bits forced by L0.
    #[must_use]
    pub const fn l0_required(self) -> u32 {
        self.l0_required
    }

    /// Returns the controls written to the direct hardware VMCS.
    #[must_use]
    pub const fn effective(self) -> u32 {
        self.l1_requested | self.l0_required
    }

    /// Classifies an exit caused by one or more control bits.
    ///
    /// If L1 requested any causing bit, the exit is architecturally visible
    /// to L1 even when L0 also forced the same bit.
    #[must_use]
    pub const fn exit_disposition(self, causing_controls: u32) -> ExitDisposition {
        if self.l1_requested & causing_controls != 0 {
            ExitDisposition::ReflectToL1
        } else {
            ExitDisposition::L0Only
        }
    }
}

/// Whether a hardware VM exit is visible to L1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitDisposition {
    /// Synthesize the nested VM exit requested by L1.
    ReflectToL1,
    /// Handle the extra exit in L0 and resume L2.
    L0Only,
}

impl ExitDisposition {
    /// Decides whether mirrored VM-exit MSR-store values become visible to L1.
    #[must_use]
    pub const fn msr_store_commit(self) -> MsrStoreCommit {
        match self {
            Self::ReflectToL1 => MsrStoreCommit::CommitToL1,
            Self::L0Only => MsrStoreCommit::Discard,
        }
    }
}

/// Action for values captured in L0's VM-exit MSR-store mirror.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MsrStoreCommit {
    /// Copy captured values to L1's original store list before reflection.
    CommitToL1,
    /// Do not expose an L0-only exit through L1's store-list memory.
    Discard,
}

/// Maximum MSR-list count advertised by the minimal policy.
pub const MAX_MSR_MIRROR_ENTRIES: u32 = 512;

/// Saved L1 MSR-list metadata while direct-VMCS fields point at L0 mirrors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MsrMirrorMetadata {
    /// L1's original VM-exit MSR-store list address.
    store_address: u64,
    /// Number of entries in L1's original store list.
    store_count: u32,
    /// L1's original VM-exit MSR-load list address.
    load_address: u64,
    /// Number of entries in L1's original load list.
    load_count: u32,
}

impl MsrMirrorMetadata {
    /// Creates bounded list metadata.
    ///
    /// Returns `None` if either count exceeds the fixed 512-entry policy.
    #[must_use]
    pub const fn new(
        exit_store_address: u64,
        exit_store_count: u32,
        exit_load_address: u64,
        exit_load_count: u32,
    ) -> Option<Self> {
        if exit_store_count > MAX_MSR_MIRROR_ENTRIES || exit_load_count > MAX_MSR_MIRROR_ENTRIES {
            None
        } else {
            Some(Self {
                store_address: exit_store_address,
                store_count: exit_store_count,
                load_address: exit_load_address,
                load_count: exit_load_count,
            })
        }
    }

    /// Returns L1's original store-list address and count.
    #[must_use]
    pub const fn exit_store(self) -> (u64, u32) {
        (self.store_address, self.store_count)
    }

    /// Returns L1's original load-list address and count.
    #[must_use]
    pub const fn exit_load(self) -> (u64, u32) {
        (self.load_address, self.load_count)
    }
}

/// VMCS fields that direct-VMCS entry must save and replace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum VmcsField {
    /// Host ES selector.
    HostEsSelector = 0x0c00,
    /// Host CS selector.
    HostCsSelector = 0x0c02,
    /// Host SS selector.
    HostSsSelector = 0x0c04,
    /// Host DS selector.
    HostDsSelector = 0x0c06,
    /// Host FS selector.
    HostFsSelector = 0x0c08,
    /// Host GS selector.
    HostGsSelector = 0x0c0a,
    /// Host TR selector.
    HostTrSelector = 0x0c0c,
    /// L1's VM-exit MSR-store address.
    VmExitMsrStoreAddress = 0x2006,
    /// L1's VM-exit MSR-load address.
    VmExitMsrLoadAddress = 0x2008,
    /// Host `IA32_PAT`.
    HostIa32Pat = 0x2c00,
    /// Host `IA32_EFER`.
    HostIa32Efer = 0x2c02,
    /// Host `IA32_PERF_GLOBAL_CTRL`.
    HostIa32PerfGlobalControl = 0x2c04,
    /// L1's VM-exit MSR-store count.
    VmExitMsrStoreCount = 0x400e,
    /// L1's VM-exit MSR-load count.
    VmExitMsrLoadCount = 0x4010,
    /// Host `IA32_SYSENTER_CS`.
    HostIa32SysenterCs = 0x4c00,
    /// Host CR0.
    HostCr0 = 0x6c00,
    /// Host CR3.
    HostCr3 = 0x6c02,
    /// Host CR4.
    HostCr4 = 0x6c04,
    /// Host FS base.
    HostFsBase = 0x6c06,
    /// Host GS base.
    HostGsBase = 0x6c08,
    /// Host TR base.
    HostTrBase = 0x6c0a,
    /// Host GDTR base.
    HostGdtrBase = 0x6c0c,
    /// Host IDTR base.
    HostIdtrBase = 0x6c0e,
    /// Host `IA32_SYSENTER_ESP`.
    HostIa32SysenterEsp = 0x6c10,
    /// Host `IA32_SYSENTER_EIP`.
    HostIa32SysenterEip = 0x6c12,
    /// L0 VM-exit stack pointer.
    HostRsp = 0x6c14,
    /// L0 VM-exit instruction pointer.
    HostRip = 0x6c16,
    /// Host supervisor CET state.
    HostSCet = 0x6c18,
    /// Host shadow-stack pointer.
    HostSsp = 0x6c1a,
    /// Host interrupt shadow-stack-table address.
    HostInterruptSspTable = 0x6c1c,
}

/// Reason a direct-VMCS field is saved and replaced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatchKind {
    /// Prevent hardware VM exit from jumping directly into L1 host state.
    HostState,
    /// Redirect L2 MSR-store side effects into an L0-owned mirror.
    ExitMsrStore,
    /// Ensure hardware restores L0 MSRs before executing L0 code.
    ExitMsrLoad,
}

/// One entry in the direct-VMCS patch manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectVmcsPatch {
    /// VMCS field to save and replace before nested entry.
    pub field: VmcsField,
    /// Safety purpose of the replacement.
    pub kind: PatchKind,
}

impl DirectVmcsPatch {
    /// Declares a host-state replacement.
    const fn host(field: VmcsField) -> Self {
        Self {
            field,
            kind: PatchKind::HostState,
        }
    }

    /// Declares an exit MSR-store redirection.
    const fn store(field: VmcsField) -> Self {
        Self {
            field,
            kind: PatchKind::ExitMsrStore,
        }
    }

    /// Declares an exit MSR-load replacement.
    const fn load(field: VmcsField) -> Self {
        Self {
            field,
            kind: PatchKind::ExitMsrLoad,
        }
    }
}

/// Complete direct-VMCS host and VM-exit-MSR patch manifest.
pub const DIRECT_VMCS_PATCH_MANIFEST: [DirectVmcsPatch; 30] = [
    DirectVmcsPatch::host(VmcsField::HostEsSelector),
    DirectVmcsPatch::host(VmcsField::HostCsSelector),
    DirectVmcsPatch::host(VmcsField::HostSsSelector),
    DirectVmcsPatch::host(VmcsField::HostDsSelector),
    DirectVmcsPatch::host(VmcsField::HostFsSelector),
    DirectVmcsPatch::host(VmcsField::HostGsSelector),
    DirectVmcsPatch::host(VmcsField::HostTrSelector),
    DirectVmcsPatch::host(VmcsField::HostIa32Pat),
    DirectVmcsPatch::host(VmcsField::HostIa32Efer),
    DirectVmcsPatch::host(VmcsField::HostIa32PerfGlobalControl),
    DirectVmcsPatch::host(VmcsField::HostIa32SysenterCs),
    DirectVmcsPatch::host(VmcsField::HostCr0),
    DirectVmcsPatch::host(VmcsField::HostCr3),
    DirectVmcsPatch::host(VmcsField::HostCr4),
    DirectVmcsPatch::host(VmcsField::HostFsBase),
    DirectVmcsPatch::host(VmcsField::HostGsBase),
    DirectVmcsPatch::host(VmcsField::HostTrBase),
    DirectVmcsPatch::host(VmcsField::HostGdtrBase),
    DirectVmcsPatch::host(VmcsField::HostIdtrBase),
    DirectVmcsPatch::host(VmcsField::HostIa32SysenterEsp),
    DirectVmcsPatch::host(VmcsField::HostIa32SysenterEip),
    DirectVmcsPatch::host(VmcsField::HostRsp),
    DirectVmcsPatch::host(VmcsField::HostRip),
    DirectVmcsPatch::host(VmcsField::HostSCet),
    DirectVmcsPatch::host(VmcsField::HostSsp),
    DirectVmcsPatch::host(VmcsField::HostInterruptSspTable),
    DirectVmcsPatch::store(VmcsField::VmExitMsrStoreAddress),
    DirectVmcsPatch::store(VmcsField::VmExitMsrStoreCount),
    DirectVmcsPatch::load(VmcsField::VmExitMsrLoadAddress),
    DirectVmcsPatch::load(VmcsField::VmExitMsrLoadCount),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vmx_results_replace_only_the_six_status_flags() {
        let original = u64::MAX;
        let preserved = original & !VMX_STATUS_RFLAGS;

        assert_eq!(
            VmInstructionResult::Vmsucceed.apply_to_rflags(original),
            preserved
        );
        assert_eq!(
            VmInstructionResult::VmfailInvalid.apply_to_rflags(original),
            preserved | RFLAGS_CF
        );
        assert_eq!(
            VmInstructionResult::VmfailValid(7).apply_to_rflags(original),
            preserved | RFLAGS_ZF
        );
        assert_eq!(
            VmInstructionResult::VmfailValid(7).instruction_error(),
            Some(7)
        );
        assert_eq!(VmInstructionResult::VmfailInvalid.instruction_error(), None);
    }

    #[test]
    fn vcpu_tracks_current_vmcs_launch_state() {
        let vmxon = VmxonPhys::new(0x1000).unwrap();
        let vmcs = VmcsPhys::new(0x2000).unwrap();
        let mut state = VcpuState::new();

        state.record_vmxon_success(vmxon);
        state.record_vmptrld_success(vmcs, VmcsLaunchState::Clear);
        assert_eq!(
            state.entry_result(VmEntryInstruction::Vmresume),
            VmInstructionResult::VmfailValid(VMXERR_VMRESUME_NONLAUNCHED_VMCS)
        );
        assert!(state.record_entry_success(VmEntryInstruction::Vmlaunch));
        assert_eq!(
            state.entry_result(VmEntryInstruction::Vmlaunch),
            VmInstructionResult::VmfailValid(VMXERR_VMLAUNCH_NONCLEAR_VMCS)
        );
        assert_eq!(
            state.entry_result(VmEntryInstruction::Vmresume),
            VmInstructionResult::Vmsucceed
        );

        state.record_vmclear_success(vmcs);
        assert_eq!(state.current_vmcs(), None);
        assert_eq!(
            state.entry_result(VmEntryInstruction::Vmlaunch),
            VmInstructionResult::VmfailInvalid
        );
        state.record_vmxoff_success();
        assert!(!state.in_vmx_operation());
    }

    #[test]
    fn capability_policy_is_kvm_sufficient_and_hides_deferred_features() {
        assert_eq!(
            TRUSTED_PIN_CONTROLS & PIN_POSTED_INTERRUPTS,
            0,
            "posted interrupts must remain hidden"
        );
        assert_eq!(TRUSTED_PRIMARY_CONTROLS & PRIMARY_TPR_SHADOW, 0);
        let hidden_secondary = SECONDARY_VIRTUALIZE_APIC_ACCESSES
            | SECONDARY_VIRTUALIZE_X2APIC
            | SECONDARY_ENABLE_VPID
            | SECONDARY_APIC_REGISTER_VIRTUALIZATION
            | SECONDARY_VIRTUAL_INTERRUPT_DELIVERY
            | SECONDARY_ENABLE_VMFUNC
            | SECONDARY_VMCS_SHADOWING
            | SECONDARY_ENABLE_PML
            | SECONDARY_TSC_SCALING;
        assert_eq!(TRUSTED_SECONDARY_CONTROLS & hidden_secondary, 0);
        assert_eq!(TRUSTED_EPT_VPID_CAPABILITIES >> 32, 0);
        assert_eq!(TRUSTED_VMFUNC_CAPABILITIES, 0);
        assert_eq!(TRUSTED_PIN_CONTROLS, 0x0000_0009);
        assert_eq!(TRUSTED_PRIMARY_CONTROLS, 0xb399_8e8c);
        assert_eq!(TRUSTED_SECONDARY_CONTROLS, 0x0000_0082);
        assert_eq!(TRUSTED_EXIT_CONTROLS, 0x0000_8204);
        assert_eq!(TRUSTED_ENTRY_CONTROLS, 0x0000_0204);

        assert_eq!(
            TRUSTED_PIN_CONTROLS & KVM_REQUIRED_PIN_CONTROLS,
            KVM_REQUIRED_PIN_CONTROLS
        );
        assert_eq!(
            TRUSTED_PRIMARY_CONTROLS & KVM_REQUIRED_PRIMARY_CONTROLS,
            KVM_REQUIRED_PRIMARY_CONTROLS
        );
        assert_eq!(
            TRUSTED_EXIT_CONTROLS & KVM_REQUIRED_EXIT_CONTROLS,
            KVM_REQUIRED_EXIT_CONTROLS
        );
        assert_eq!(
            TRUSTED_ENTRY_CONTROLS & KVM_REQUIRED_ENTRY_CONTROLS,
            KVM_REQUIRED_ENTRY_CONTROLS
        );
        assert_eq!(
            TRUSTED_EPT_VPID_CAPABILITIES & KVM_REQUIRED_EPT_CAPABILITIES,
            KVM_REQUIRED_EPT_CAPABILITIES
        );

        let all_controls = u64::from(u32::MAX) << 32;
        assert_eq!(
            restrict_vmx_capability(vmx::IA32_VMX_PROCBASED_CTLS, all_controls).unwrap() >> 32,
            u64::from(TRUSTED_PRIMARY_CONTROLS)
        );
        assert_eq!(
            restrict_vmx_capability(vmx::IA32_VMX_VMFUNC, u64::MAX),
            Some(0)
        );
        assert_eq!(
            restrict_vmx_capability(vmx::IA32_VMX_PROCBASED_CTLS3, u64::MAX),
            Some(0)
        );
        assert_eq!(
            restrict_vmx_capability(vmx::IA32_VMX_BASIC, 0x01d8_1000_11e5_7ed0),
            Some(0x0098_1000_11e5_7ed0)
        );
    }

    #[test]
    fn capability_restriction_rejects_missing_kvm_requirements() {
        let hardware = u64::from(u32::MAX) << 32;
        let advertised = restrict_control_capability(
            hardware,
            TRUSTED_PRIMARY_CONTROLS,
            KVM_REQUIRED_PRIMARY_CONTROLS,
        )
        .unwrap();
        assert_eq!((advertised >> 32) as u32, TRUSTED_PRIMARY_CONTROLS);

        let missing_hlt = hardware & !(u64::from(PRIMARY_HLT_EXITING) << 32);
        assert_eq!(
            restrict_control_capability(
                missing_hlt,
                TRUSTED_PRIMARY_CONTROLS,
                KVM_REQUIRED_PRIMARY_CONTROLS,
            ),
            None
        );
        assert_eq!(
            restrict_ept_vpid_capability(TRUSTED_EPT_VPID_CAPABILITIES),
            Some(TRUSTED_EPT_VPID_CAPABILITIES)
        );
        assert_eq!(
            restrict_ept_vpid_capability(
                TRUSTED_EPT_VPID_CAPABILITIES & !EPT_INVEPT_GLOBAL_CONTEXT
            ),
            None
        );
    }

    #[test]
    fn l0_only_exits_do_not_commit_l1_msr_stores() {
        let controls = ControlProvenance::new(PRIMARY_HLT_EXITING, PRIMARY_USE_MSR_BITMAPS);
        assert_eq!(
            controls.exit_disposition(PRIMARY_HLT_EXITING),
            ExitDisposition::ReflectToL1
        );
        assert_eq!(
            controls
                .exit_disposition(PRIMARY_USE_MSR_BITMAPS)
                .msr_store_commit(),
            MsrStoreCommit::Discard
        );
        assert_eq!(
            ExitDisposition::ReflectToL1.msr_store_commit(),
            MsrStoreCommit::CommitToL1
        );
    }

    #[test]
    fn patch_manifest_covers_every_host_and_exit_msr_field_once() {
        assert_eq!(
            DIRECT_VMCS_PATCH_MANIFEST
                .iter()
                .filter(|patch| patch.kind == PatchKind::HostState)
                .count(),
            26
        );
        assert_eq!(
            DIRECT_VMCS_PATCH_MANIFEST
                .iter()
                .filter(|patch| patch.kind == PatchKind::ExitMsrStore)
                .count(),
            2
        );
        assert_eq!(
            DIRECT_VMCS_PATCH_MANIFEST
                .iter()
                .filter(|patch| patch.kind == PatchKind::ExitMsrLoad)
                .count(),
            2
        );
        for (index, patch) in DIRECT_VMCS_PATCH_MANIFEST.iter().enumerate() {
            assert!(
                DIRECT_VMCS_PATCH_MANIFEST[..index]
                    .iter()
                    .all(|earlier| earlier.field != patch.field),
                "duplicate patch field: {:?}",
                patch.field
            );
        }
    }

    #[test]
    fn msr_mirror_metadata_enforces_the_advertised_limit() {
        let metadata = MsrMirrorMetadata::new(0x1000, 512, 0x2000, 512).unwrap();
        assert_eq!(metadata.exit_store(), (0x1000, 512));
        assert_eq!(metadata.exit_load(), (0x2000, 512));
        assert_eq!(MsrMirrorMetadata::new(0, 513, 0, 0), None);
        assert_eq!(MsrMirrorMetadata::new(0, 0, 0, 513), None);
    }
}
