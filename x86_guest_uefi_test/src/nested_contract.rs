//! Bounded, disposable QEMU/KVM nested-VMX instruction contract.
//!
//! Run the identical image below the project Direct-VMCS monitor and the
//! outer-KVM reference. This image does not launch an L2; the Linux KVM probe
//! tests real entry/exit separately. No variable, disk, or firmware identity
//! service is written. All VMX regions belong to this application until cleared
//! and VMXOFF succeeds; no test state survives a successful return.
//!
//! Intel SDM Volume 3C, sections 32.2--32.4 specify the flag/error contracts:
//! <https://cdrdv2-public.intel.com/868136/252046-081-sdm-change-document.pdf>.

#![cfg_attr(not(test), no_main)]
#![cfg_attr(not(test), no_std)]

mod l1_extended;
mod l1_fault;
mod l1_memory;
mod l1_xstate;

use core::arch::asm;
use core::fmt;
use core::fmt::Write;
use core::ptr;
use r_efi::efi;
use x86_64_hal::addr::VmcsPhys;
use x86_64_hal::addr::VmxonPhys;
use x86_64_hal::cpu;
use x86_64_hal::platform_memory;
use x86_64_hal::platform_memory::FirmwareDescriptor;
use x86_64_hal::vmcs;
use x86_64_hal::vmx;
use x86_64_hal::vmx::VmxStatus;

const PAGE: usize = 4096;
/// VMXON, two VMCS regions, an empty EPT root, and private operand-fault pages.
const PAGES: usize = 4 + l1_memory::PAGES;
const CYCLES: u64 = 8;
/// Reserved encoding bit 12 guarantees this is not a supported VMCS field.
const UNSUPPORTED_FIELD: u64 = 0x1000;
const SENTINEL: u64 = 0x1234_5678_9abc_def0;
/// CF in bit zero, ZF in bit one; the forbidden 1/1 result stays distinguishable.
const FAIL_INVALID: u64 = 1;
const FAIL_VALID: u64 = 2;

/// A bounded diagnostic with no firmware payload or identity contents.
#[derive(Debug)]
struct Failure {
    stage: &'static str,
    actual: u64,
    expected: u64,
}

type Result<T> = core::result::Result<T, Failure>;

fn equal(stage: &'static str, actual: u64, expected: u64) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Failure {
            stage,
            actual,
            expected,
        })
    }
}

fn success(stage: &'static str, status: VmxStatus) -> Result<()> {
    equal(
        stage,
        match status {
            VmxStatus::Success => 0,
            VmxStatus::FailInvalid => FAIL_INVALID,
            VmxStatus::FailValid => FAIL_VALID,
        },
        0,
    )
}

/// COM1 is owned by the disposable VM; absent/broken UART waits are finite.
struct Serial;

impl Serial {
    fn initialize(&mut self) {
        // SAFETY: the x86 UEFI fixture runs at CPL0 and owns QEMU's COM1 ports.
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

    fn byte(&mut self, byte: u8) -> fmt::Result {
        for _ in 0..65_536 {
            // SAFETY: the fixture owns COM1 and runs at CPL0.
            let ready = unsafe { cpu::inb(0x3fd) };
            if ready == 0xff {
                return Err(fmt::Error);
            }
            if ready & 0x20 != 0 {
                // SAFETY: the owned UART reported an available transmitter.
                unsafe { cpu::outb(0x3f8, byte) };
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(fmt::Error)
    }

    fn failure(&mut self, error: &Failure) {
        let _ = writeln!(
            self,
            "thin-hv: nested contract FAIL stage={} actual={:#x} expected={:#x}",
            error.stage, error.actual, error.expected
        );
    }
}

impl Write for Serial {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                self.byte(b'\r')?;
            }
            self.byte(byte)?;
        }
        Ok(())
    }
}

/// Captured prerequisites; original execution mode must remain unchanged.
struct Prerequisites {
    revision: u32,
    cr0: u64,
    cr4: u64,
    invept: bool,
    invvpid: bool,
    readonly: bool,
    shadow: bool,
    invept_types: u8,
    invvpid_types: u8,
    pku: bool,
}

impl Prerequisites {
    /// One successful instruction for each advertised invalidation type.
    fn invalidation_successes(&self) -> u32 {
        self.invept_types.count_ones() + self.invvpid_types.count_ones()
    }

    /// Single EPTP, all VPID reserved fields, non-global zero VPIDs, and a
    /// noncanonical individual address are separate negative descriptor cases.
    fn descriptor_failures(&self) -> u32 {
        u32::from(self.invept_types & 1)
            + self.invvpid_types.count_ones()
            + (self.invvpid_types & 0b1011).count_ones()
            + u32::from(self.invvpid_types & 1)
    }

    fn valid_failures(&self) -> u32 {
        14 - u32::from(self.shadow)
            + u32::from(self.invept)
            + u32::from(self.invvpid)
            + u32::from(self.readonly)
            + self.descriptor_failures()
    }
}

fn prerequisites() -> Result<Prerequisites> {
    equal("cpuid-vmx", u64::from(cpu::has_vmx()), 1)?;
    equal(
        "cpuid-msr",
        u64::from(cpu::cpuid(1, 0).edx & (1 << 5) != 0),
        1,
    )?;
    let extended = cpu::cpuid(0x8000_0000, 0).eax;
    equal("cpuid-address-leaf", u64::from(extended >= 0x8000_0008), 1)?;
    let width = cpu::cpuid(0x8000_0008, 0).eax & 0xff;
    equal("physical-width", u64::from((32..=52).contains(&width)), 1)?;
    // SAFETY: CPUID.VMX and MSR were checked; these are mandatory VMX capability
    // MSRs, read at CPL0. No FEATURE_CONTROL or other firmware MSR is written.
    let (feature, basic, cr0_fixed0, cr0_fixed1, cr4_fixed0, cr4_fixed1) = unsafe {
        (
            cpu::rdmsr(cpu::IA32_FEATURE_CONTROL),
            vmx::VmxBasic::from_msr(cpu::rdmsr(vmx::IA32_VMX_BASIC)),
            cpu::rdmsr(vmx::IA32_VMX_CR0_FIXED0),
            cpu::rdmsr(vmx::IA32_VMX_CR0_FIXED1),
            cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED0),
            cpu::rdmsr(vmx::IA32_VMX_CR4_FIXED1),
        )
    };
    equal("feature-control", feature & 5, 5)?;
    equal(
        "vmcs-size",
        u64::from((1..=4096).contains(&basic.region_size)),
        1,
    )?;
    equal("vmcs-memory-type", u64::from(basic.memory_type), 6)?;
    let cr0 = cpu::read_cr0();
    let cr4 = cpu::read_cr4();
    equal("firmware-vmxe-clear", cr4 & (1 << 13), 0)?;
    equal("firmware-cache-enabled", cr0 & ((1 << 29) | (1 << 30)), 0)?;
    // Refuse mode-changing normalization. Only CR4.VMXE is added; arbitrary
    // firmware state is never repaired by this diagnostic fixture.
    equal("cr0-fixed", (cr0 | cr0_fixed0) & cr0_fixed1, cr0)?;
    let enabled_cr4 = cr4 | (1 << 13);
    equal(
        "cr4-fixed",
        (enabled_cr4 | cr4_fixed0) & cr4_fixed1,
        enabled_cr4,
    )?;
    // SAFETY: CPUID.VMX establishes legacy controls; BASIC gates TRUE controls.
    let primary = unsafe {
        cpu::rdmsr(if basic.true_controls {
            vmx::IA32_VMX_TRUE_PROCBASED_CTLS
        } else {
            vmx::IA32_VMX_PROCBASED_CTLS
        })
    };
    // The entry-failure fixture deliberately sets this reserved primary bit.
    equal("entry-invalid-control-bit", primary & (1 << 32), 0)?;
    let mut secondary = 0;
    if primary & (1 << 63) != 0 {
        // SAFETY: allowed-one activate-secondary-controls establishes this MSR.
        secondary = unsafe { cpu::rdmsr(vmx::IA32_VMX_PROCBASED_CTLS2) };
    }
    let mut translation_caps = 0;
    if secondary & ((1 << 33) | (1 << 37)) != 0 {
        // SAFETY: secondary EPT or VPID support establishes EPT_VPID_CAP.
        translation_caps = unsafe { cpu::rdmsr(vmx::IA32_VMX_EPT_VPID_CAP) };
    }
    // SAFETY: CPUID.VMX establishes the mandatory VMX_MISC capability MSR.
    let misc = unsafe { cpu::rdmsr(vmx::IA32_VMX_MISC) };
    let invept = secondary & (1 << 33) != 0 && translation_caps & (1 << 20) != 0;
    let invvpid = secondary & (1 << 37) != 0 && translation_caps & (1 << 32) != 0;
    let invept_types = if invept {
        ((translation_caps >> 25) & 3) as u8
    } else {
        0
    };
    let invvpid_types = if invvpid {
        ((translation_caps >> 40) & 15) as u8
    } else {
        0
    };
    equal(
        "invept-types",
        u64::from(invept_types != 0),
        u64::from(invept),
    )?;
    equal(
        "invvpid-types",
        u64::from(invvpid_types != 0),
        u64::from(invvpid),
    )?;
    if invept_types & 1 != 0 {
        equal(
            "invept-single-format",
            translation_caps & ((1 << 6) | (1 << 14)),
            (1 << 6) | (1 << 14),
        )?;
    }
    Ok(Prerequisites {
        revision: basic.revision_id,
        cr0,
        cr4,
        invept,
        invvpid,
        readonly: misc & (1 << 29) == 0,
        shadow: secondary & (1 << 46) != 0,
        invept_types,
        invvpid_types,
        pku: cpu::cpuid(7, 0).ecx & x86_64_hal::xstate::CPUID_PKU != 0,
    })
}

/// Confirms the bounded low-memory allocation belongs to WB-capable firmware RAM.
fn allocation_map(boot: *mut efi::BootServices, base: u64) -> Result<()> {
    let mut storage = [0_u64; 1024];
    let mut length = core::mem::size_of_val(&storage);
    let mut key = 0;
    let mut stride = 0;
    let mut version = 0;
    // SAFETY: boot is live, the eight-byte-aligned stack buffer covers `length`
    // bytes, and all output slots remain valid for this synchronous call.
    let status = unsafe {
        ((*boot).get_memory_map)(
            &mut length,
            storage.as_mut_ptr().cast(),
            &mut key,
            &mut stride,
            &mut version,
        )
    };
    equal("memory-map-status", status.as_usize() as u64, 0)?;
    equal(
        "memory-map-size",
        u64::from(length <= core::mem::size_of_val(&storage)),
        1,
    )?;
    // SAFETY: the initialized stack array is byte-readable and the returned size
    // was checked against its complete allocation; the slice stays in this call.
    let bytes = unsafe { core::slice::from_raw_parts(storage.as_ptr().cast(), length) };
    let mut regions = [FirmwareDescriptor::default(); 205];
    let count =
        platform_memory::decode_uefi_map(bytes, stride, version, &mut regions).map_err(|_| {
            Failure {
                stage: "memory-map-layout",
                actual: 1,
                expected: 0,
            }
        })?;
    allocation_covered(&regions[..count], base)
}

fn allocation_covered(regions: &[FirmwareDescriptor], base: u64) -> Result<()> {
    let end = base
        .checked_add((PAGES * PAGE) as u64)
        .filter(|&end| base != 0 && base % PAGE as u64 == 0 && end <= 1 << 32)
        .ok_or(Failure {
            stage: "allocation-range",
            actual: base,
            expected: 0,
        })?;
    let mut covered = [0_u8; PAGES];
    for region in regions {
        let region_end = region
            .number_of_pages
            .checked_mul(PAGE as u64)
            .and_then(|bytes| region.physical_start.checked_add(bytes))
            .filter(|_| region.physical_start % PAGE as u64 == 0 && region.number_of_pages != 0)
            .ok_or(Failure {
                stage: "descriptor-range",
                actual: region.physical_start,
                expected: 0,
            })?;
        if region.physical_start >= end || region_end <= base {
            continue;
        }
        equal(
            "allocation-memory-type",
            u64::from(region.memory_type),
            u64::from(efi::BOOT_SERVICES_DATA),
        )?;
        equal(
            "allocation-wb",
            region.attributes & efi::MEMORY_WB,
            efi::MEMORY_WB,
        )?;
        equal(
            "allocation-protected",
            region.attributes & (efi::MEMORY_RP | efi::MEMORY_WP | efi::MEMORY_RO),
            0,
        )?;
        for (index, coverage) in covered.iter_mut().enumerate() {
            let page = base + (index * PAGE) as u64;
            if region.physical_start <= page && page < region_end {
                *coverage = coverage.saturating_add(1);
            }
        }
    }
    for count in covered {
        equal("allocation-page-coverage", u64::from(count), 1)?;
    }
    Ok(())
}

/// Executes only architecturally failing VMREAD forms, preserving raw CF/ZF.
///
/// # Safety
/// The caller must be in VMX root operation. `field` is either reserved, or
/// there is no current VMCS. Neither case can read or modify VMCS payload data.
unsafe fn rejected_read(field: u64) -> (u64, u64) {
    let mut value = SENTINEL;
    let carry: u8;
    let zero: u8;
    // SAFETY: the caller established VMX root operation and the intentional
    // invalid case. Register operands cannot fault on guest memory. The HAL's
    // valid-VMCS/field contract deliberately is not invoked for this probe.
    unsafe {
        asm!("vmread {value}, {field}", "setc {carry}", "setz {zero}",
        field = in(reg) field, value = inout(reg) value,
        carry = lateout(reg_byte) carry, zero = lateout(reg_byte) zero,
        options(nostack));
    }
    (u64::from(carry) | (u64::from(zero) << 1), value)
}

/// Executes an unsupported/read-only VMWRITE without claiming the HAL contract.
///
/// # Safety
/// The caller must be in VMX root operation, either without a current VMCS or
/// with a current, exclusively owned VMCS and a reserved field or read-only
/// field with VMX_MISC[29] clear.
unsafe fn rejected_write(field: u64) -> u64 {
    let carry: u8;
    let zero: u8;
    // SAFETY: VMX root and the absent VMCS or architecturally failing field are
    // caller invariants. No memory operand exists; only a current error may change.
    unsafe {
        asm!("vmwrite {field}, {value}", "setc {carry}", "setz {zero}",
        field = in(reg) field, value = in(reg) SENTINEL,
        carry = lateout(reg_byte) carry, zero = lateout(reg_byte) zero,
        options(nostack));
    }
    u64::from(carry) | (u64::from(zero) << 1)
}

/// Tests rejected physical pointers without dereferencing malformed addresses.
///
/// # Safety
/// For operation 0, VMX must already be active, or `physical` must identify an
/// owned, inactive VMXON page with a deliberately rejected revision/header and
/// VMX prerequisites set. For operations 1/2, VMX root must be active and
/// `physical` must be this CPU's VMXON pointer, 1, or an owned inactive VMCS
/// with a test header. Any current VMCS must be test-owned. A shadow VMPTRLD
/// may succeed only when advertised; VMCLEAR ignores revision/header bits.
/// Callers must clear any accepted region before rewriting its header.
unsafe fn rejected_pointer(physical: u64, operation: u8) -> u64 {
    let carry: u8;
    let zero: u8;
    // SAFETY: the memory operand is the readable live stack slot, not `physical`.
    // Address 1 is rejected before dereference; all other operands name exclusively
    // owned regions. The caller enforces VMX state/header rules and accounts for
    // whether hardware accepted the page before modifying any region header.
    unsafe {
        match operation {
            0 => asm!("vmxon [{pointer}]", "setc {carry}", "setz {zero}",
                pointer = in(reg) &physical, carry = lateout(reg_byte) carry,
                zero = lateout(reg_byte) zero, options(nostack)),
            1 => asm!("vmclear [{pointer}]", "setc {carry}", "setz {zero}",
                pointer = in(reg) &physical, carry = lateout(reg_byte) carry,
                zero = lateout(reg_byte) zero, options(nostack)),
            _ => asm!("vmptrld [{pointer}]", "setc {carry}", "setz {zero}",
                pointer = in(reg) &physical, carry = lateout(reg_byte) carry,
                zero = lateout(reg_byte) zero, options(nostack)),
        }
    }
    u64::from(carry) | (u64::from(zero) << 1)
}

/// Uses a reserved invalidation type with a valid, zero-filled m128 descriptor.
///
/// # Safety
/// VMX root and a current VMCS are required; CPUID and VMX capability MSRs must
/// establish availability of the selected instruction, avoiding #UD.
unsafe fn rejected_invalidation(vpid: bool) -> u64 {
    let descriptor = vmx::InveptDescriptor::default();
    let carry: u8;
    let zero: u8;
    // SAFETY: the advertised instruction sees a readable aligned m128 operand;
    // type u64::MAX is unsupported and must report error 28 without invalidation.
    // Deliberately do not call the HAL wrapper requiring a supported type.
    unsafe {
        if vpid {
            asm!("invvpid {kind}, [{descriptor}]", "setc {carry}", "setz {zero}",
                kind = in(reg) u64::MAX, descriptor = in(reg) &descriptor,
                carry = lateout(reg_byte) carry, zero = lateout(reg_byte) zero,
                options(nostack));
        } else {
            asm!("invept {kind}, [{descriptor}]", "setc {carry}", "setz {zero}",
                kind = in(reg) u64::MAX, descriptor = in(reg) &descriptor,
                carry = lateout(reg_byte) carry, zero = lateout(reg_byte) zero,
                options(nostack));
        }
    }
    u64::from(carry) | (u64::from(zero) << 1)
}

/// Captures immediate entry failure without invoking a successful-entry wrapper.
///
/// # Safety
/// VMX root operation is active and the current VMCS is absent or test-owned.
/// VMRESUME must target clear launch state; VMLAUNCH must have no current VMCS
/// or the explicitly reserved primary control bit set. No entry can succeed.
unsafe fn rejected_entry(resume: bool) -> Result<u64> {
    // SAFETY: the caller establishes an architectural pre-entry failure. Neither
    // instruction can install guest/host state, and there are no memory operands.
    unsafe {
        l1_extended::check(
            if resume {
                l1_extended::Operation::Resume
            } else {
                l1_extended::Operation::Launch
            },
            false,
        )
    }
}

/// Tests revision and shadow-header policy while preserving the active VMCS.
///
/// # Safety
/// `first` is current; `second` is initialized, cleared and inactive. Both pages
/// are owned and identity mapped. No VM entry occurs in this helper.
unsafe fn vmcs_headers(
    first: VmcsPhys,
    second: VmcsPhys,
    capabilities: &Prerequisites,
    serial: &mut Serial,
) -> Result<()> {
    for (stage, header, expected) in [
        ("vmcs-revision", capabilities.revision ^ 1, FAIL_VALID),
        (
            "vmcs-shadow",
            capabilities.revision | (1 << 31),
            if capabilities.shadow { 0 } else { FAIL_VALID },
        ),
    ] {
        // SAFETY: second was cleared before this iteration and is not active on
        // any CPU. Only its owned four-byte header changes, before VMPTRLD.
        unsafe { ptr::write_volatile(second.get() as *mut u32, header) };
        // SAFETY: the modified region is owned; VMPTRLD either rejects its header
        // or makes it current. Both outcomes are handled before any header rewrite.
        let flags = unsafe { rejected_pointer(second.get(), 2) };
        let result = (|| {
            equal(stage, flags, expected)?;
            // SAFETY: flags established the actual current VMCS. Both candidate
            // pages are exclusively owned and only mandatory fields are read.
            unsafe {
                pointer_equal(
                    "header-current-pointer",
                    if flags == 0 {
                        second.get()
                    } else {
                        first.get()
                    },
                )?;
                if flags == FAIL_VALID {
                    field_equal(stage, vmcs::VM_INSTRUCTION_ERROR, 11)?;
                }
            }
            Ok(())
        })();
        // SAFETY: second remains exclusively owned. VMCLEAR does not validate
        // revision/shadow bits; it makes any accepted page inactive before writes.
        if let Err(error) = equal(
            "header-clear-retaining-pages",
            unsafe { rejected_pointer(second.get(), 1) },
            0,
        ) {
            fatal(serial, error);
        }
        // SAFETY: VMCLEAR succeeded, so the second region is inactive and its
        // original header can be restored. The first remains a valid owned VMCS.
        unsafe {
            ptr::write_volatile(second.get() as *mut u32, capabilities.revision);
            if let Err(error) = success("header-restore-current", vmx::vmptrld(first)) {
                fatal(serial, error);
            }
        }
        result?;
    }
    Ok(())
}

/// Enters no guest: primary control bit zero is reserved and must fail first.
///
/// # Safety
/// A test-owned clear ordinary VMCS is current, and capability checks established
/// that primary control bit zero cannot be one. The caller is in VMX root.
unsafe fn entry_boundaries(capabilities: &Prerequisites) -> Result<()> {
    // Host values satisfy host-state field checks without installing descriptors.
    // They are never loaded because the deliberately invalid primary control
    // prevents guest-state loading. This is not a usable guest bootstrap.
    let host_fields = [
        (vmcs::HOST_CR0, capabilities.cr0),
        (vmcs::HOST_CR3, cpu::read_cr3()),
        (vmcs::HOST_CR4, capabilities.cr4 | (1 << 13)),
        (vmcs::HOST_CS_SELECTOR, 8),
        (vmcs::HOST_SS_SELECTOR, 16),
        (vmcs::HOST_DS_SELECTOR, 0),
        (vmcs::HOST_ES_SELECTOR, 0),
        (vmcs::HOST_FS_SELECTOR, 0),
        (vmcs::HOST_GS_SELECTOR, 0),
        (vmcs::HOST_TR_SELECTOR, 24),
        (vmcs::HOST_FS_BASE, 0),
        (vmcs::HOST_GS_BASE, 0),
        (vmcs::HOST_TR_BASE, 0),
        (vmcs::HOST_GDTR_BASE, 0),
        (vmcs::HOST_IDTR_BASE, 0),
        (vmcs::HOST_IA32_SYSENTER_CS, 0),
        (vmcs::HOST_IA32_SYSENTER_ESP, 0),
        (vmcs::HOST_IA32_SYSENTER_EIP, 0),
        (vmcs::HOST_RSP, cpu::read_rsp()),
        (vmcs::HOST_RIP, 0),
    ];
    // SAFETY: the current VMCS is owned. All encodings/widths are mandatory and
    // writable; the reserved control prevents any of these host fields loading.
    unsafe {
        for (field, value) in host_fields {
            success("entry-host-field", vmx::vmwrite(field, value))?;
        }
        for (field, value) in [
            (vmcs::CPU_BASED_VM_EXEC_CONTROL, 1),
            (vmcs::PIN_BASED_VM_EXEC_CONTROL, 0),
            (vmcs::VM_ENTRY_CONTROLS, 0),
            (vmcs::VM_EXIT_CONTROLS, 1 << 9),
            (vmcs::VM_ENTRY_MSR_LOAD_COUNT, 0),
            (vmcs::VM_EXIT_MSR_LOAD_COUNT, 0),
            (vmcs::VM_EXIT_MSR_STORE_COUNT, 0),
            (vmcs::VM_ENTRY_INTR_INFO_FIELD, 0),
        ] {
            success("entry-control-field", vmx::vmwrite(field, value))?;
        }
        equal("resume-clear-flags", rejected_entry(true)?, FAIL_VALID)?;
        field_equal("resume-clear-error", vmcs::VM_INSTRUCTION_ERROR, 5)?;
        equal("launch-controls-flags", rejected_entry(false)?, FAIL_VALID)?;
        field_equal("launch-controls-error", vmcs::VM_INSTRUCTION_ERROR, 7)?;
        equal(
            "resume-after-failed-launch-flags",
            rejected_entry(true)?,
            FAIL_VALID,
        )?;
        field_equal(
            "resume-after-failed-launch-error",
            vmcs::VM_INSTRUCTION_ERROR,
            5,
        )?;
        // L0 must not leak its required host-field patches through VMREAD after
        // an immediate entry failure; all twenty retained L1 values are checked.
        for (field, value) in host_fields {
            field_equal("entry-host-field-preserved", field, value)?;
        }
        field_equal(
            "entry-control-preserved",
            vmcs::CPU_BASED_VM_EXEC_CONTROL,
            1,
        )?;
        // SAFETY: the VMCS remains clear and owned; the helper substitutes an
        // invalid host field before every entry with otherwise valid controls.
        host_field_boundaries()?;
        for (field, value) in host_fields {
            field_equal("host-check-all-originals-restored", field, value)?;
        }
    }
    Ok(())
}

/// Rejects 34 invalid original host values and checks two higher-priority
/// failures. No entry may reach guest-state loading: one host field is always
/// invalid, including when the test selects otherwise valid control settings.
///
/// # Safety
/// The caller owns a clear current VMCS initialized by entry_boundaries, with
/// valid baseline host fields and primary bit zero proved reserved. No CPU may
/// concurrently modify its fields or launch it. Failure cleanup retains pages.
unsafe fn host_field_boundaries() -> Result<()> {
    // SAFETY: VMX is active at CPL0; BASIC determines whether TRUE controls
    // exist. All four capability MSRs are read-only and present in this mode.
    let capabilities = unsafe {
        let basic = vmx::VmxBasic::from_msr(cpu::rdmsr(vmx::IA32_VMX_BASIC));
        [
            (
                vmcs::PIN_BASED_VM_EXEC_CONTROL,
                0,
                cpu::rdmsr(if basic.true_controls {
                    vmx::IA32_VMX_TRUE_PINBASED_CTLS
                } else {
                    vmx::IA32_VMX_PINBASED_CTLS
                }),
            ),
            (
                vmcs::CPU_BASED_VM_EXEC_CONTROL,
                0,
                cpu::rdmsr(if basic.true_controls {
                    vmx::IA32_VMX_TRUE_PROCBASED_CTLS
                } else {
                    vmx::IA32_VMX_PROCBASED_CTLS
                }),
            ),
            (
                vmcs::VM_EXIT_CONTROLS,
                (1 << 9) | vmcs::VM_EXIT_LOAD_IA32_PAT | vmcs::VM_EXIT_LOAD_IA32_EFER,
                cpu::rdmsr(if basic.true_controls {
                    vmx::IA32_VMX_TRUE_EXIT_CTLS
                } else {
                    vmx::IA32_VMX_EXIT_CTLS
                }),
            ),
            (
                vmcs::VM_ENTRY_CONTROLS,
                vmcs::VM_ENTRY_LOAD_IA32_PAT | vmcs::VM_ENTRY_LOAD_IA32_EFER,
                cpu::rdmsr(if basic.true_controls {
                    vmx::IA32_VMX_TRUE_ENTRY_CTLS
                } else {
                    vmx::IA32_VMX_ENTRY_CTLS
                }),
            ),
        ]
    };
    for (_, requested, capability) in capabilities {
        equal(
            "host-check-control-supported",
            u64::from(requested) & !(capability >> 32),
            0,
        )?;
    }
    let mut saved_controls = [0_u64; 4];
    let saved_pat;
    let saved_efer;
    // SAFETY: these mandatory VMCS encodings are readable on the owned current
    // VMCS. Read all original values before the first mutation for cleanup.
    unsafe {
        for (slot, (field, _, _)) in saved_controls.iter_mut().zip(capabilities) {
            *slot = read_field("host-check-save-control", field)?;
        }
        saved_pat = read_field("host-check-save-pat", vmcs::HOST_IA32_PAT)?;
        saved_efer = read_field("host-check-save-efer", vmcs::HOST_IA32_EFER)?;
    }
    let result = (|| {
        // SAFETY: only owned VMCS fields change, with no entry until one host
        // field is deliberately invalidated. PAT/EFER copies are valid live L1
        // MSRs; no MSR is written and no guest/host state is actually loaded.
        unsafe {
            for (field, requested, capability) in capabilities {
                success(
                    "host-check-control",
                    vmx::vmwrite(
                        field,
                        u64::from(vmx::adjust_controls(requested, capability)),
                    ),
                )?;
            }
            success(
                "host-check-pat",
                vmx::vmwrite(vmcs::HOST_IA32_PAT, cpu::rdmsr(cpu::IA32_PAT)),
            )?;
            success(
                "host-check-efer",
                vmx::vmwrite(vmcs::HOST_IA32_EFER, cpu::rdmsr(cpu::IA32_EFER)),
            )?;
        }
        let address_bits = cpu::cpuid(0x8000_0008, 0).eax;
        let physical_bits = address_bits & 255;
        let linear_bits = (address_bits >> 8) & 255;
        equal(
            "host-check-linear-width",
            u64::from(matches!(linear_bits, 48 | 57)),
            1,
        )?;
        let mut failures = 0;
        // SAFETY: all probes below retain this clear VMCS and restore their
        // individual mutated field on success or assertion failure. A null or
        // RPL/TI selector, invalid CR/MSR or noncanonical checked base must fail
        // before hardware loads any guest state or entry MSR list.
        unsafe {
            for field in [
                vmcs::HOST_CS_SELECTOR,
                vmcs::HOST_SS_SELECTOR,
                vmcs::HOST_DS_SELECTOR,
                vmcs::HOST_ES_SELECTOR,
                vmcs::HOST_FS_SELECTOR,
                vmcs::HOST_GS_SELECTOR,
                vmcs::HOST_TR_SELECTOR,
            ] {
                for value in [1, 4] {
                    reject_host_field(field, value)?;
                    failures += 1;
                }
            }
            for field in [vmcs::HOST_CS_SELECTOR, vmcs::HOST_TR_SELECTOR] {
                reject_host_field(field, 0)?;
                failures += 1;
            }
            let cr0 = read_field("host-check-cr0", vmcs::HOST_CR0)?;
            let cr4 = read_field("host-check-cr4", vmcs::HOST_CR4)?;
            for (field, value) in [
                (vmcs::HOST_CR0, cr0 & !(1 << 31)),
                (vmcs::HOST_CR0, cr0 & !1),
                (vmcs::HOST_CR4, cr4 & !(1 << 13)),
                (vmcs::HOST_CR4, cr4 & !(1 << 5)),
                (vmcs::HOST_CR4, cr4 | (1 << 63)),
                (vmcs::HOST_CR3, 1 << 63),
                (vmcs::HOST_CR3, 1 << physical_bits),
            ] {
                reject_host_field(field, value)?;
                failures += 1;
            }
            for field in [
                vmcs::HOST_FS_BASE,
                vmcs::HOST_GS_BASE,
                vmcs::HOST_TR_BASE,
                vmcs::HOST_GDTR_BASE,
                vmcs::HOST_IDTR_BASE,
                vmcs::HOST_IA32_SYSENTER_ESP,
                vmcs::HOST_IA32_SYSENTER_EIP,
                vmcs::HOST_RIP,
            ] {
                reject_host_field(field, 1 << linear_bits)?;
                failures += 1;
            }
            for (field, value) in [
                (vmcs::HOST_IA32_PAT, 2),
                (vmcs::HOST_IA32_EFER, 0),
                (vmcs::HOST_IA32_EFER, 0xd03),
            ] {
                reject_host_field(field, value)?;
                failures += 1;
            }
            equal("host-check-failure-count", failures, 34)?;
            let cs = read_field("host-check-priority-cs", vmcs::HOST_CS_SELECTOR)?;
            let primary = read_field(
                "host-check-priority-controls",
                vmcs::CPU_BASED_VM_EXEC_CONTROL,
            )?;
            success(
                "host-check-invalid-cs",
                vmx::vmwrite(vmcs::HOST_CS_SELECTOR, 0),
            )?;
            let priority = (|| {
                msr_list_boundaries(physical_bits as u8)?;
                equal(
                    "host-check-resume-priority-flags",
                    rejected_entry(true)?,
                    FAIL_VALID,
                )?;
                field_equal(
                    "host-check-resume-priority-error",
                    vmcs::VM_INSTRUCTION_ERROR,
                    5,
                )?;
                success(
                    "host-check-invalid-control",
                    vmx::vmwrite(vmcs::CPU_BASED_VM_EXEC_CONTROL, primary | 1),
                )?;
                equal(
                    "host-check-control-priority-flags",
                    rejected_entry(false)?,
                    FAIL_VALID,
                )?;
                field_equal(
                    "host-check-control-priority-error",
                    vmcs::VM_INSTRUCTION_ERROR,
                    7,
                )
            })();
            success(
                "host-check-restore-cs",
                vmx::vmwrite(vmcs::HOST_CS_SELECTOR, cs),
            )?;
            success(
                "host-check-restore-primary",
                vmx::vmwrite(vmcs::CPU_BASED_VM_EXEC_CONTROL, primary),
            )?;
            priority?;
        }
        Ok(())
    })();
    // SAFETY: every entry was guarded by an invalid host field or an invalid
    // control; no guest ran and the VMCS remains clear/current. Restore all
    // changed control/MSR fields even on an ordinary assertion failure.
    unsafe {
        for ((field, _, _), value) in capabilities.into_iter().zip(saved_controls) {
            success("host-check-restore-control", vmx::vmwrite(field, value))?;
            field_equal("host-check-restored-control", field, value)?;
        }
        success(
            "host-check-restore-pat",
            vmx::vmwrite(vmcs::HOST_IA32_PAT, saved_pat),
        )?;
        success(
            "host-check-restore-efer",
            vmx::vmwrite(vmcs::HOST_IA32_EFER, saved_efer),
        )?;
        field_equal("host-check-restored-pat", vmcs::HOST_IA32_PAT, saved_pat)?;
        field_equal("host-check-restored-efer", vmcs::HOST_IA32_EFER, saved_efer)?;
    }
    result
}

/// Checks list controls before any list memory can be touched. The current
/// clear VMCS has HOST_CS=0 as an independent guard even if a negative control
/// is mistakenly accepted. All list fields are restored on assertion failure.
unsafe fn msr_list_boundaries(physical_bits: u8) -> Result<()> {
    for (address_field, count_field) in [
        (vmcs::VM_ENTRY_MSR_LOAD_ADDR, vmcs::VM_ENTRY_MSR_LOAD_COUNT),
        (vmcs::VM_EXIT_MSR_STORE_ADDR, vmcs::VM_EXIT_MSR_STORE_COUNT),
        (vmcs::VM_EXIT_MSR_LOAD_ADDR, vmcs::VM_EXIT_MSR_LOAD_COUNT),
    ] {
        // SAFETY: this CPU exclusively owns the clear/current VMCS. HOST_CS=0
        // prevents guest state or MSR loading for every case including count=0.
        // The addresses below are never dereferenced by either test or CPU.
        unsafe {
            let address = read_field("msr-list-save-address", address_field)?;
            let count = read_field("msr-list-save-count", count_field)?;
            equal("msr-list-initial-count", count, 0)?;
            let result = (|| {
                for (address, count) in [
                    (1, 1),
                    ((1_u64 << physical_bits) - 16, 2),
                    (u64::MAX - 15, 2),
                    (0, u64::from(u32::MAX)),
                ] {
                    success("msr-list-bad-address", vmx::vmwrite(address_field, address))?;
                    success("msr-list-bad-count", vmx::vmwrite(count_field, count))?;
                    for (resume, error) in [(false, 7), (true, 5)] {
                        equal("msr-list-flags", rejected_entry(resume)?, FAIL_VALID)?;
                        field_equal("msr-list-error", vmcs::VM_INSTRUCTION_ERROR, error)?;
                        field_equal("msr-list-address-retained", address_field, address)?;
                        field_equal("msr-list-count-retained", count_field, count)?;
                    }
                }
                success("msr-list-empty-count", vmx::vmwrite(count_field, 0))?;
                success(
                    "msr-list-ignored-address",
                    vmx::vmwrite(address_field, u64::MAX),
                )?;
                equal("msr-list-empty-flags", rejected_entry(false)?, FAIL_VALID)?;
                // The invalid original host selector, not an ignored list
                // address, must determine this VMfailValid error.
                field_equal("msr-list-empty-host-error", vmcs::VM_INSTRUCTION_ERROR, 8)
            })();
            success("msr-list-restore-count", vmx::vmwrite(count_field, count))?;
            success(
                "msr-list-restore-address",
                vmx::vmwrite(address_field, address),
            )?;
            field_equal("msr-list-restored-count", count_field, count)?;
            field_equal("msr-list-restored-address", address_field, address)?;
            result?;
        }
    }
    Ok(())
}

/// # Safety
/// The current test-owned VMCS has valid controls and baseline host state.
/// The supplied replacement is architecturally invalid for this host field,
/// guaranteeing VMfail before guest state or MSR-list loading.
unsafe fn reject_host_field(field: u32, value: u64) -> Result<()> {
    // SAFETY: the caller owns this writable field and supplies a guaranteed
    // invalid replacement. Restore it even when the returned status is wrong.
    unsafe {
        let original = read_field("host-check-save-field", field)?;
        success("host-check-invalid-field", vmx::vmwrite(field, value))?;
        let result = (|| {
            equal(
                "host-check-vmlaunch-flags",
                rejected_entry(false)?,
                FAIL_VALID,
            )?;
            field_equal("host-check-vmlaunch-error", vmcs::VM_INSTRUCTION_ERROR, 8)?;
            field_equal("host-check-original-visible", field, value)
        })();
        success("host-check-restore-field", vmx::vmwrite(field, original))?;
        field_equal("host-check-restored-field", field, original)?;
        result
    }
}

/// Descriptor-error probe; no invalidation or physical pointer access can occur.
///
/// # Safety
/// VMX root/current test-owned VMCS and instruction/type support are required.
/// The readable descriptor must encode one of the deliberately invalid cases.
unsafe fn invalid_descriptor(kind: u64, words: [u64; 2], vpid: bool) -> u64 {
    // A stack m128 operand need not be 16-byte aligned for INVEPT/INVVPID.
    let carry: u8;
    let zero: u8;
    // SAFETY: this live 16-byte operand is readable. All malformed cases fail
    // architectural operand validation before invalidation; no L2 entry exists.
    unsafe {
        if vpid {
            asm!("invvpid {kind}, [{words}]", "setc {carry}", "setz {zero}",
                kind = in(reg) kind, words = in(reg) &words,
                carry = lateout(reg_byte) carry, zero = lateout(reg_byte) zero,
                options(nostack));
        } else {
            asm!("invept {kind}, [{words}]", "setc {carry}", "setz {zero}",
                kind = in(reg) kind, words = in(reg) &words,
                carry = lateout(reg_byte) carry, zero = lateout(reg_byte) zero,
                options(nostack));
        }
    }
    u64::from(carry) | (u64::from(zero) << 1)
}

/// Covers every advertised invalidation type and its distinct operand checks.
///
/// # Safety
/// A test-owned VMCS is current in VMX root, with error 28 from the prior invalid
/// type probes if either instruction is advertised. `ept_root` is an owned,
/// aligned WB zero page. No EPT/VPID tested here has ever run an L2.
unsafe fn invalidation_descriptors(capabilities: &Prerequisites, ept_root: u64) -> Result<()> {
    for kind in 1..=2_u64 {
        if capabilities.invept_types & (1 << (kind - 1)) == 0 {
            continue;
        }
        let descriptor = vmx::InveptDescriptor {
            // Global INVEPT ignores EPTP completely; the reserved word stays zero.
            ept_pointer: if kind == 1 {
                ept_root | 6 | (3 << 3)
            } else {
                u64::MAX
            },
            reserved: 0,
        };
        // SAFETY: the captured capability advertised this type; single-context
        // uses the validated WB/four-level EPTP, global ignores its EPTP entirely.
        unsafe {
            success("invept-valid-descriptor", vmx::invept(kind, &descriptor))?;
            field_equal(
                "invept-success-error-preserved",
                vmcs::VM_INSTRUCTION_ERROR,
                28,
            )?;
            if kind == 1 {
                equal(
                    "invept-invalid-eptp-flags",
                    invalid_descriptor(kind, [0, 0], false),
                    FAIL_VALID,
                )?;
                field_equal("invept-invalid-eptp-error", vmcs::VM_INSTRUCTION_ERROR, 28)?;
            }
        }
    }
    for kind in 0..4_u64 {
        if capabilities.invvpid_types & (1 << kind) == 0 {
            continue;
        }
        let descriptor = vmx::InvvpidDescriptor {
            vpid: if kind == 2 { 0 } else { 1 },
            reserved: [0; 3],
            linear_address: if kind == 0 { 0 } else { 1 << 63 },
        };
        // SAFETY: this advertised type sees a supported descriptor: nonzero VPID
        // except for global, canonical address only where individual requires it.
        // Other types ignore the linear address. Invalidations are safe with no L2.
        unsafe {
            success("invvpid-valid-descriptor", vmx::invvpid(kind, &descriptor))?;
            field_equal(
                "invvpid-success-error-preserved",
                vmcs::VM_INSTRUCTION_ERROR,
                28,
            )?;
            // Intel checks bits63:16 before the type-specific switch, including
            // type2 whose VPID and linear address themselves are ignored.
            equal(
                "invvpid-reserved-flags",
                invalid_descriptor(kind, [(1 << 16) | 1, 0], true),
                FAIL_VALID,
            )?;
            field_equal("invvpid-reserved-error", vmcs::VM_INSTRUCTION_ERROR, 28)?;
            if kind != 2 {
                equal(
                    "invvpid-zero-flags",
                    invalid_descriptor(kind, [0, 0], true),
                    FAIL_VALID,
                )?;
                field_equal("invvpid-zero-error", vmcs::VM_INSTRUCTION_ERROR, 28)?;
            }
            if kind == 0 {
                // Bit63 alone is noncanonical under both 48- and 57-bit addressing.
                equal(
                    "invvpid-noncanonical-flags",
                    invalid_descriptor(kind, [1, 1 << 63], true),
                    FAIL_VALID,
                )?;
                field_equal("invvpid-noncanonical-error", vmcs::VM_INSTRUCTION_ERROR, 28)?;
            }
        }
    }
    Ok(())
}

/// The current VMCS is exclusively test-owned throughout each successful call.
unsafe fn field_equal(stage: &'static str, field: u32, expected: u64) -> Result<()> {
    // SAFETY: inherited owned-current-VMCS and supported-field invariant.
    equal(stage, unsafe { read_field(stage, field) }?, expected)
}

/// Reads a supported field of the caller's exclusively owned current VMCS.
unsafe fn read_field(stage: &'static str, field: u32) -> Result<u64> {
    // SAFETY: callers have successfully loaded their owned VMCS; every call
    // passes a mandatory supported guest, host, control, or error-field encoding.
    unsafe { vmx::vmread(field) }.map_err(|status| Failure {
        stage,
        actual: match status {
            VmxStatus::Success => 0,
            VmxStatus::FailInvalid => FAIL_INVALID,
            VmxStatus::FailValid => FAIL_VALID,
        },
        expected: 0,
    })
}

unsafe fn pointer_equal(stage: &'static str, expected: u64) -> Result<()> {
    let mut physical = 0;
    // SAFETY: callers are in VMX root and this output slot is writable for 8 bytes.
    success(stage, unsafe { vmx::vmptrst(&mut physical) })?;
    equal(stage, physical, expected)
}

/// No guest entries occur, so field contents are independent of launch validity.
unsafe fn instructions(
    vmxon: VmxonPhys,
    first: VmcsPhys,
    second: VmcsPhys,
    capabilities: &Prerequisites,
    serial: &mut Serial,
) -> Result<()> {
    l1_memory::without_current(vmxon.get() + (4 * PAGE) as u64)?;
    // SAFETY: VMXON succeeded and no VMPTRLD has executed in this session.
    unsafe {
        pointer_equal("initial-pointer", u64::MAX)?;
        let (flags, value) = rejected_read(u64::from(vmcs::GUEST_RIP));
        equal("read-no-current-flags", flags, FAIL_INVALID)?;
        equal("read-no-current-destination", value, SENTINEL)?;
        equal(
            "write-no-current-flags",
            rejected_write(u64::from(vmcs::GUEST_RIP)),
            FAIL_INVALID,
        )?;
        equal(
            "vmxon-no-current-flags",
            rejected_pointer(vmxon.get(), 0),
            FAIL_INVALID,
        )?;
        equal(
            "clear-no-current-flags",
            rejected_pointer(1, 1),
            FAIL_INVALID,
        )?;
        equal(
            "load-no-current-flags",
            rejected_pointer(1, 2),
            FAIL_INVALID,
        )?;
        equal(
            "launch-no-current-flags",
            rejected_entry(false)?,
            FAIL_INVALID,
        )?;
        equal(
            "resume-no-current-flags",
            rejected_entry(true)?,
            FAIL_INVALID,
        )?;
    }
    // SAFETY: both aligned, initialized WB VMCS pages are exclusively test-owned.
    unsafe {
        success("clear-first", vmx::vmclear(first))?;
        success("clear-second", vmx::vmclear(second))?;
        success("load-first", vmx::vmptrld(first))?;
        success("write-first-rip", vmx::vmwrite(vmcs::GUEST_RIP, 0x1000))?;
        success("write-first-rsp", vmx::vmwrite(vmcs::GUEST_RSP, 0x8000))?;
    }
    let partial_stores = l1_memory::in_vmx(vmxon.get() + (4 * PAGE) as u64, serial)?;
    // SAFETY: first is current, second was cleared and has not been loaded.
    // The header helper restores ordinary second/first ownership before return.
    unsafe {
        vmcs_headers(first, second, capabilities, serial)?;
    }
    // SAFETY: first is ordinary and clear. Only explicitly failing VM-entry
    // instructions run; the capability check proved primary control bit0 invalid.
    unsafe {
        entry_boundaries(capabilities)?;
    }
    let _ = writeln!(
        serial,
        "thin-hv: native L1 original host validation PASS invalid=34 priority=2 restored=1"
    );
    // SAFETY: first is current and owned; the reserved/read-only fields and
    // VMXON pointer exercise defined VMfailValid paths. Physical value 1 is held
    // in a readable operand and rejected for alignment before VMCS memory access.
    unsafe {
        let (flags, value) = rejected_read(UNSUPPORTED_FIELD);
        equal("unsupported-read-flags", flags, FAIL_VALID)?;
        equal("unsupported-read-destination", value, SENTINEL)?;
        field_equal("unsupported-read-error", vmcs::VM_INSTRUCTION_ERROR, 12)?;
        let wide_field = (1 << 32) | u64::from(vmcs::GUEST_RIP);
        let (flags, value) = rejected_read(wide_field);
        equal("wide-read-flags", flags, FAIL_VALID)?;
        equal("wide-read-destination", value, SENTINEL)?;
        field_equal("wide-read-error", vmcs::VM_INSTRUCTION_ERROR, 12)?;
        equal("wide-write-flags", rejected_write(wide_field), FAIL_VALID)?;
        field_equal("wide-write-error", vmcs::VM_INSTRUCTION_ERROR, 12)?;
        if capabilities.readonly {
            equal(
                "readonly-write-flags",
                rejected_write(u64::from(vmcs::VM_INSTRUCTION_ERROR)),
                FAIL_VALID,
            )?;
            field_equal("readonly-write-error", vmcs::VM_INSTRUCTION_ERROR, 13)?;
        }
        equal(
            "clear-vmxon-flags",
            rejected_pointer(vmxon.get(), 1),
            FAIL_VALID,
        )?;
        field_equal("clear-vmxon-error", vmcs::VM_INSTRUCTION_ERROR, 3)?;
        equal("clear-misaligned-flags", rejected_pointer(1, 1), FAIL_VALID)?;
        field_equal("clear-misaligned-error", vmcs::VM_INSTRUCTION_ERROR, 2)?;
        equal(
            "repeat-vmxon-flags",
            rejected_pointer(vmxon.get(), 0),
            FAIL_VALID,
        )?;
        field_equal("repeat-vmxon-error", vmcs::VM_INSTRUCTION_ERROR, 15)?;
    }
    // SAFETY: second is an initialized, exclusively owned VMCS; after VMPTRLD
    // succeeds only mandatory natural-width guest fields are written. The invalid
    // field, physical value 1, and VMXON pointer are defined rejection probes;
    // the operand slot is valid and address 1 is never dereferenced by VMPTRLD.
    unsafe {
        success("load-second", vmx::vmptrld(second))?;
        success("write-second-rip", vmx::vmwrite(vmcs::GUEST_RIP, 0x2000))?;
        success("write-second-rsp", vmx::vmwrite(vmcs::GUEST_RSP, 0x9000))?;
        equal(
            "unsupported-write-flags",
            rejected_write(UNSUPPORTED_FIELD),
            FAIL_VALID,
        )?;
        field_equal("unsupported-write-error", vmcs::VM_INSTRUCTION_ERROR, 12)?;
        equal("load-misaligned-flags", rejected_pointer(1, 2), FAIL_VALID)?;
        field_equal("load-misaligned-error", vmcs::VM_INSTRUCTION_ERROR, 9)?;
        equal(
            "load-vmxon-flags",
            rejected_pointer(vmxon.get(), 2),
            FAIL_VALID,
        )?;
        field_equal("load-vmxon-error", vmcs::VM_INSTRUCTION_ERROR, 10)?;
    }
    for cycle in 0..CYCLES {
        // SAFETY: the two owned VMCSs remain active only on this CPU. Successful
        // VMX operations must preserve each VMCS's distinct error field; these
        // guest RIP/RSP values are never consumed by a hardware VM entry.
        unsafe {
            success("switch-first", vmx::vmptrld(first))?;
            pointer_equal("first-pointer", first.get())?;
            field_equal("first-rip", vmcs::GUEST_RIP, 0x1000 + cycle)?;
            field_equal("first-rsp", vmcs::GUEST_RSP, 0x8000 + cycle)?;
            field_equal("first-error", vmcs::VM_INSTRUCTION_ERROR, 15)?;
            success(
                "update-first-rip",
                vmx::vmwrite(vmcs::GUEST_RIP, 0x1001 + cycle),
            )?;
            success(
                "update-first-rsp",
                vmx::vmwrite(vmcs::GUEST_RSP, 0x8001 + cycle),
            )?;
            success("switch-second", vmx::vmptrld(second))?;
            pointer_equal("second-pointer", second.get())?;
            field_equal("second-rip", vmcs::GUEST_RIP, 0x2000 + cycle)?;
            field_equal("second-rsp", vmcs::GUEST_RSP, 0x9000 + cycle)?;
            field_equal("second-error", vmcs::VM_INSTRUCTION_ERROR, 10)?;
            success(
                "update-second-rip",
                vmx::vmwrite(vmcs::GUEST_RIP, 0x2001 + cycle),
            )?;
            success(
                "update-second-rsp",
                vmx::vmwrite(vmcs::GUEST_RSP, 0x9001 + cycle),
            )?;
        }
    }
    if capabilities.invept {
        // SAFETY: capability MSRs advertised INVEPT and second remains current.
        unsafe {
            equal("invept-flags", rejected_invalidation(false), FAIL_VALID)?;
            field_equal("invept-error", vmcs::VM_INSTRUCTION_ERROR, 28)?;
        }
    }
    if capabilities.invvpid {
        // SAFETY: capability MSRs advertised INVVPID and second remains current.
        unsafe {
            equal("invvpid-flags", rejected_invalidation(true), FAIL_VALID)?;
            field_equal("invvpid-error", vmcs::VM_INSTRUCTION_ERROR, 28)?;
        }
    }
    // SAFETY: second remains current with error28 if invalidations are supported.
    // The final allocation page is an owned, aligned, zero-filled WB EPT root.
    unsafe {
        invalidation_descriptors(capabilities, vmxon.get() + (3 * PAGE) as u64)?;
    }
    // Retain this strict assertion, but only after exercising the existing
    // contract as well; a reference partial write must not hide later evidence.
    equal("l1-operand-no-partial-stores", partial_stores, 0)
}

/// Halts only after one bounded diagnostic; runner owns the finite test deadline.
fn fatal(serial: &mut Serial, error: Failure) -> ! {
    serial.failure(&error);
    loop {
        // SAFETY: a cleanup/panic failure cannot safely return or release VMX
        // storage. This dedicated VM is stopped at CPL0 until the runner kills it.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

fn run(table: *mut efi::SystemTable, serial: &mut Serial) -> Result<Prerequisites> {
    l1_xstate::run()?;
    equal("system-table", u64::from(!table.is_null()), 1)?;
    // SAFETY: UEFI supplies this live SystemTable; this fixture never calls EBS.
    let boot = unsafe { (*table).boot_services };
    equal("boot-services", u64::from(!boot.is_null()), 1)?;
    let capabilities = prerequisites()?;
    equal(
        "operand-invept-global-required",
        u64::from(capabilities.invept_types & 2 != 0),
        1,
    )?;
    equal(
        "operand-invvpid-global-required",
        u64::from(capabilities.invvpid_types & 4 != 0),
        1,
    )?;
    let mut base = u64::from(u32::MAX);
    // SAFETY: boot is live and base is a writable output slot. A low allocation
    // satisfies BASIC's optional 32-bit operand limit and UEFI identity mapping.
    let allocation = unsafe {
        ((*boot).allocate_pages)(
            efi::ALLOCATE_MAX_ADDRESS,
            efi::BOOT_SERVICES_DATA,
            PAGES,
            &mut base,
        )
    };
    equal("allocate-pages", allocation.as_usize() as u64, 0)?;
    let result = (|| {
        allocation_map(boot, base)?;
        let vmxon = VmxonPhys::new(base).ok_or(Failure {
            stage: "vmxon-alignment",
            actual: base,
            expected: 0,
        })?;
        let first = VmcsPhys::new(base + PAGE as u64).ok_or(Failure {
            stage: "first-alignment",
            actual: base,
            expected: 0,
        })?;
        let second = VmcsPhys::new(base + (2 * PAGE) as u64).ok_or(Failure {
            stage: "second-alignment",
            actual: base,
            expected: 0,
        })?;
        // SAFETY: the entire exclusive low-memory allocation was
        // checked against the current UEFI map before any pointer dereference.
        // OVMF supplies WB RAM; CR0 caching is enabled and BASIC requires WB.
        unsafe {
            ptr::write_bytes(base as *mut u8, 0, PAGES * PAGE);
            for index in 0..3 {
                ptr::write_volatile(
                    (base + (index * PAGE) as u64) as *mut u32,
                    capabilities.revision,
                );
            }
            cpu::write_cr4(capabilities.cr4 | (1 << 13));
        }
        if let Err(error) = l1_memory::before_vmxon(base + (4 * PAGE) as u64) {
            // SAFETY: only architecturally faulting VMXON probes ran; restore
            // the controls before ordinary allocation cleanup on assertion error.
            unsafe {
                cpu::write_cr4(capabilities.cr4);
                cpu::write_cr0(capabilities.cr0);
            }
            return Err(error);
        }
        for (stage, header) in [
            ("vmxon-revision", capabilities.revision ^ 1),
            ("vmxon-shadow", capabilities.revision | (1 << 31)),
        ] {
            // SAFETY: VMX has not entered and the VMXON page is exclusively owned.
            // VMX prerequisites are active; Intel rejects each header without entry.
            let flags = unsafe {
                ptr::write_volatile(base as *mut u32, header);
                rejected_pointer(base, 0)
            };
            if let Err(error) = equal(stage, flags, FAIL_INVALID) {
                // Unexpected success means the page could now be active. Never
                // overwrite or free it on any contradictory result; halt visibly.
                fatal(serial, error);
            }
        }
        // SAFETY: both VMXON probes returned VMfailInvalid and left the owned
        // region inactive. Restore the ordinary header before the valid VMXON.
        unsafe { ptr::write_volatile(base as *mut u32, capabilities.revision) };
        // SAFETY: FEATURE_CONTROL, fixed CR bits, cacheability, revision and
        // exclusive VMXON storage were checked. No VMX session was active at entry.
        let entered = unsafe { vmx::vmxon(vmxon) };
        let result = if entered == VmxStatus::Success {
            // SAFETY: this CPU owns the newly entered session and both initialized
            // VMCS pages. The finite contract never enters a guest or migrates.
            let result = unsafe { instructions(vmxon, first, second, &capabilities, serial) };
            // SAFETY: these are our initialized VMCS pages even after a failed
            // assertion. Clear both to evict cached VMCS state before VMXOFF.
            let (clear_first, clear_second, left) =
                unsafe { (vmx::vmclear(first), vmx::vmclear(second), vmx::vmxoff()) };
            if let Err(error) = success("cleanup-vmxoff-retaining-pages", left) {
                fatal(serial, error);
            }
            if let Err(error) = success("cleanup-clear-first-retaining-pages", clear_first)
                .and_then(|()| success("cleanup-clear-second-retaining-pages", clear_second))
            {
                fatal(serial, error);
            }
            result
        } else {
            success("vmxon", entered)
        };
        // SAFETY: VMXON failed without entering, or cleanup VMXOFF succeeded.
        // Restore the exact captured controls before calling firmware or freeing.
        unsafe {
            cpu::write_cr4(capabilities.cr4);
            cpu::write_cr0(capabilities.cr0);
        }
        if let Err(error) = equal("restored-cr0", cpu::read_cr0(), capabilities.cr0)
            .and_then(|()| equal("restored-cr4", cpu::read_cr4(), capabilities.cr4))
        {
            fatal(serial, error);
        }
        result?;
        Ok(capabilities)
    })();
    // SAFETY: the closure returns only before VMXON, after its failure, or after
    // both VMCLEARs and VMXOFF succeeded. No active VMCS or references survive.
    // Fatal cleanup failures retain this allocation and never reach FreePages.
    let release = unsafe { ((*boot).free_pages)(base, PAGES) };
    equal("free-pages", release.as_usize() as u64, 0)?;
    result
}

/// UEFI entry point; PASS is emitted only after all architectural cleanup.
#[unsafe(no_mangle)]
pub extern "efiapi" fn efi_main(_image: efi::Handle, table: *mut efi::SystemTable) -> efi::Status {
    let mut serial = Serial;
    serial.initialize();
    if writeln!(serial, "thin-hv: nested contract START").is_err() {
        return efi::Status::DEVICE_ERROR;
    }
    match run(table, &mut serial) {
        Ok(capabilities) => {
            if writeln!(serial,
                "thin-hv: nested contract PASS vmcs=2 cycles={CYCLES} vmfail_invalid=9 vmfail_valid={} invept={} invvpid={} readonly={} wide_fields=2 misaligned=2 revision=3 entry_failures=3 no_current=7 shadow={} invept_types={} invvpid_types={} invalidation_success={} descriptor_failures={} osxsave_toggles=4 xsetbv_valid=4 xsetbv_gp=4 xsetbv_ud=1 pku={} ospke_toggles={} operand_pf=16 operand_gp=8 operand_ss=1 operand_cross=6 operand_priority=8 host_invalid=34 host_priority=2 host_restore=1 msr_invalid=12 msr_priority=12 msr_ignored=3 fx_cpuid=6 fx_xsetbv=12 fx_entry=68 fx_irq=3 ymm_rounds={}",
                capabilities.valid_failures(), u8::from(capabilities.invept),
                u8::from(capabilities.invvpid), u8::from(capabilities.readonly),
                u8::from(capabilities.shadow), capabilities.invept_types,
                capabilities.invvpid_types, capabilities.invalidation_successes(),
                capabilities.descriptor_failures(), u8::from(capabilities.pku),
                u8::from(capabilities.pku) * 4,
                u8::from(cpu::cpuid(1, 0).ecx & (1 << 28) != 0 && cpu::cpuid(0xd, 0).eax & 7 == 7) * 4).is_ok() {
                efi::Status::SUCCESS
            } else { efi::Status::DEVICE_ERROR }
        }
        Err(error) => { serial.failure(&error); efi::Status::DEVICE_ERROR }
    }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    fatal(
        &mut Serial,
        Failure {
            stage: "panic-retaining-pages",
            actual: 1,
            expected: 0,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_totals_follow_advertised_invalidation_types() {
        for ept in 0..4_u8 {
            for vpid in 0..16_u8 {
                let capabilities = Prerequisites {
                    revision: 1,
                    cr0: 0,
                    cr4: 0,
                    invept: ept != 0,
                    invvpid: vpid != 0,
                    readonly: true,
                    shadow: false,
                    invept_types: ept,
                    invvpid_types: vpid,
                    pku: false,
                };
                let mut good = 0;
                let mut bad = 0;
                for kind in 1..=2 {
                    if ept & (1 << (kind - 1)) != 0 {
                        good += 1;
                        bad += u32::from(kind == 1);
                    }
                }
                for kind in 0..4 {
                    if vpid & (1 << kind) != 0 {
                        good += 1;
                        bad += 1 + u32::from(kind != 2) + u32::from(kind == 0);
                    }
                }
                assert_eq!(capabilities.invalidation_successes(), good);
                assert_eq!(capabilities.descriptor_failures(), bad);
                assert_eq!(
                    capabilities.valid_failures(),
                    15 + u32::from(ept != 0) + u32::from(vpid != 0) + bad
                );
            }
        }
    }

    #[test]
    fn allocation_requires_complete_unique_writable_wb_pages() {
        let mut region = FirmwareDescriptor {
            memory_type: efi::BOOT_SERVICES_DATA,
            physical_start: 0x1000,
            number_of_pages: PAGES as u64,
            attributes: efi::MEMORY_WB,
        };
        assert!(allocation_covered(&[region], 0x1000).is_ok());
        assert!(allocation_covered(&[region, region], 0x1000).is_err());
        assert!(allocation_covered(&[region], 0).is_err());
        assert!(allocation_covered(&[region], u64::MAX).is_err());
        assert!(allocation_covered(&[region], (1 << 32) - PAGE as u64).is_err());
        region.attributes |= efi::MEMORY_RO;
        assert!(allocation_covered(&[region], 0x1000).is_err());
        region.attributes = 0;
        assert!(allocation_covered(&[region], 0x1000).is_err());
        region.attributes = efi::MEMORY_WB;
        region.number_of_pages -= 1;
        assert!(allocation_covered(&[region], 0x1000).is_err());
    }
}
