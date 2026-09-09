//! Disposable q35 L1/L2 MSR-state contract. Never returns to firmware: the
//! first nested exit installs our retained descriptors, then cleanup powers
//! off the test VM. No firmware variable or physical disk is modified.

use super::Failure;
use super::PAGE;
use super::Result;
use super::Serial;
use super::equal;
use super::read_field;
use super::success;
use core::arch::asm;
use core::fmt::Write;
use r_efi::efi;
use x86_64_hal::addr::VmcsPhys;
use x86_64_hal::addr::VmxonPhys;
use x86_64_hal::cpu;
use x86_64_hal::host_state::HOST_ENVIRONMENT_PAGES;
use x86_64_hal::host_state::HostEnvironment;
use x86_64_hal::host_state::HostStack;
use x86_64_hal::vmcs;
use x86_64_hal::vmx;

// VMXON, VMCS, zero MSR bitmap, guest stack, host/IST environment and host stack.
const ENV_PAGE: usize = 4;
const STACK_PAGE: usize = ENV_PAGE + HOST_ENVIRONMENT_PAGES;
const LIST_PAGE: usize = STACK_PAGE + 2;
const PAGES: usize = LIST_PAGE + 2;
const PAT: u32 = 0x277;
const EFER: u32 = 0xc000_0080;

#[repr(C, align(16))]
struct Frame {
    fx: [u8; 512],
    original_pat: u64,
    original_efer: u64,
    inherited_pat: u64,
    inherited_efer: u64,
    l2_pat: u64,
    l2_efer: u64,
    entry_pat: u64,
    entry_efer: u64,
    exit_pat: u64,
    exit_efer: u64,
}
const _: () = assert!(core::mem::offset_of!(Frame, exit_efer) == 584);

fn failure(stage: &'static str) -> Failure {
    Failure {
        stage,
        actual: 1,
        expected: 0,
    }
}

/// Only owned-current-VMCS callers use this mandatory field writer.
unsafe fn write(field: u32, value: u64) -> Result<()> {
    // SAFETY: the caller owns the loaded VMCS on this CPU; every field is a
    // supported architectural encoding and no other CPU accesses the region.
    success("msr-vmwrite", unsafe { vmx::vmwrite(field, value) })
}

/// The checked VMX CPU advertises either legacy or TRUE control MSRs.
unsafe fn controls(legacy: u32, true_msr: u32, requested: u32) -> Result<u32> {
    // SAFETY: prerequisites established VMX capability MSR availability at CPL0.
    let basic = unsafe { cpu::rdmsr(vmx::IA32_VMX_BASIC) };
    // SAFETY: BASIC gates the TRUE MSR; the legacy alternative is mandatory.
    let caps = unsafe {
        cpu::rdmsr(if basic & (1 << 55) != 0 {
            true_msr
        } else {
            legacy
        })
    };
    let actual = (requested | caps as u32) & (caps >> 32) as u32;
    equal(
        "msr-required-control",
        u64::from(actual & requested),
        u64::from(requested),
    )?;
    Ok(actual)
}

/// Prepares a long-mode, no-EPT L2 using only the fixture's owned pages and
/// existing UEFI identity map. The test's private environment stays retained.
unsafe fn configure(base: u64, env: &HostEnvironment<'_>, entry: u32, exit: u32) -> Result<()> {
    // SAFETY: callers have VMPTRLD'd the owned clear VMCS. The low-memory pages
    // and immutable executable fixture image remain live through VMXOFF.
    unsafe {
        for (field, value) in env.vmcs_fields() {
            write(field, value)?;
        }
        let pin = controls(
            vmx::IA32_VMX_PINBASED_CTLS,
            vmx::IA32_VMX_TRUE_PINBASED_CTLS,
            0,
        )?;
        let primary = controls(
            vmx::IA32_VMX_PROCBASED_CTLS,
            vmx::IA32_VMX_TRUE_PROCBASED_CTLS,
            vmcs::PRIMARY_EXEC_USE_MSR_BITMAPS,
        )?;
        for (field, value) in [
            (vmcs::PIN_BASED_VM_EXEC_CONTROL, u64::from(pin)),
            (vmcs::CPU_BASED_VM_EXEC_CONTROL, u64::from(primary)),
            (vmcs::SECONDARY_VM_EXEC_CONTROL, 0),
            (vmcs::MSR_BITMAP, base + (2 * PAGE) as u64),
            (vmcs::VM_EXIT_CONTROLS, u64::from(exit)),
            (vmcs::VM_ENTRY_CONTROLS, u64::from(entry)),
            (vmcs::VMCS_LINK_POINTER, u64::MAX),
            (vmcs::EXCEPTION_BITMAP, u64::from(u32::MAX)),
            (vmcs::PAGE_FAULT_ERROR_CODE_MASK, 0),
            (vmcs::PAGE_FAULT_ERROR_CODE_MATCH, 0),
            (vmcs::CR3_TARGET_COUNT, 0),
            (vmcs::CR0_GUEST_HOST_MASK, 0),
            (vmcs::CR4_GUEST_HOST_MASK, 0),
            (vmcs::CR0_READ_SHADOW, 0),
            (vmcs::CR4_READ_SHADOW, 0),
            (vmcs::VM_ENTRY_INTR_INFO_FIELD, 0),
            (vmcs::VM_EXIT_MSR_STORE_COUNT, 0),
            (vmcs::VM_EXIT_MSR_LOAD_COUNT, 0),
            (vmcs::VM_ENTRY_MSR_LOAD_COUNT, 0),
            (vmcs::GUEST_CR0, cpu::read_cr0()),
            (vmcs::GUEST_CR3, cpu::read_cr3()),
            (vmcs::GUEST_CR4, cpu::read_cr4()),
            (vmcs::HOST_CR0, cpu::read_cr0()),
            (vmcs::HOST_CR3, cpu::read_cr3()),
            (vmcs::HOST_CR4, cpu::read_cr4()),
            (vmcs::GUEST_DR7, 0x400),
            (vmcs::GUEST_IA32_DEBUGCTL, 0),
            (vmcs::GUEST_INTERRUPTIBILITY_INFO, 0),
            (vmcs::GUEST_ACTIVITY_STATE, 0),
            (vmcs::GUEST_PENDING_DBG_EXCEPTIONS, 0),
            (vmcs::GUEST_SYSENTER_CS, 0),
            (vmcs::GUEST_SYSENTER_ESP, 0),
            (vmcs::GUEST_SYSENTER_EIP, 0),
            (vmcs::GUEST_RFLAGS, 2),
            (vmcs::GUEST_RIP, guest as *const () as usize as u64),
            (vmcs::GUEST_RSP, base + (4 * PAGE) as u64 - 16),
            (vmcs::GUEST_GDTR_BASE, base + (ENV_PAGE * PAGE) as u64),
            (vmcs::GUEST_GDTR_LIMIT, 39),
            (vmcs::GUEST_IDTR_BASE, base + ((ENV_PAGE + 1) * PAGE) as u64),
            (vmcs::GUEST_IDTR_LIMIT, 4095),
        ] {
            write(field, value)?;
        }
        for index in 0..8_u32 {
            let (selector, access, limit, address) = match index {
                1 => (8, 0xa09b, u64::from(u32::MAX), 0),
                4..=6 => (0, 1 << 16, 0, 0),
                7 => (24, 0x8b, 103, base + (ENV_PAGE * PAGE) as u64 + 64),
                _ => (16, 0xc093, u64::from(u32::MAX), 0),
            };
            write(vmcs::GUEST_ES_SELECTOR + 2 * index, selector)?;
            write(vmcs::GUEST_ES_LIMIT + 2 * index, limit)?;
            write(vmcs::GUEST_ES_AR_BYTES + 2 * index, access)?;
            write(vmcs::GUEST_ES_BASE + 2 * index, address)?;
        }
    }
    Ok(())
}

/// Executes the complete six-control matrix, with distinct live/guest/host PAT
/// values. EFER.SCE is alternated twice, preserving NXE and long-mode state.
unsafe fn matrix(
    base: u64,
    region: VmcsPhys,
    env: &HostEnvironment<'_>,
    serial: &mut Serial,
) -> Result<()> {
    // SAFETY: the VMX CPU supports PAT and long mode; reads do not modify state.
    let (original_pat, original_efer) = unsafe { (cpu::rdmsr(PAT), cpu::rdmsr(EFER)) };
    // Keep all four low PAT entries exactly unchanged (UEFI uses these); only
    // PAT4 differs among valid architectural types. No PTE is changed.
    let pat = |kind: u64| (original_pat & !(0xff << 32)) | (kind << 32);
    for variant in 0..128_u32 {
        let requested_entry = vmcs::VM_ENTRY_IA32E_MODE
            | if variant & 1 != 0 {
                vmcs::VM_ENTRY_LOAD_IA32_PAT
            } else {
                0
            }
            | if variant & 8 != 0 {
                vmcs::VM_ENTRY_LOAD_IA32_EFER
            } else {
                0
            };
        let requested_exit = vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE
            | if variant & 2 != 0 {
                vmcs::VM_EXIT_SAVE_IA32_PAT
            } else {
                0
            }
            | if variant & 4 != 0 {
                vmcs::VM_EXIT_LOAD_IA32_PAT
            } else {
                0
            }
            | if variant & 16 != 0 {
                vmcs::VM_EXIT_SAVE_IA32_EFER
            } else {
                0
            }
            | if variant & 32 != 0 {
                vmcs::VM_EXIT_LOAD_IA32_EFER
            } else {
                0
            };
        let sce = u64::from(variant >> 6);
        let inherited_efer = (original_efer & !1) | sce;
        let guest_efer = inherited_efer ^ 1;
        let mut frame = Frame {
            fx: [0; 512],
            original_pat,
            original_efer,
            inherited_pat: pat(0),
            inherited_efer,
            l2_pat: pat(4),
            l2_efer: inherited_efer,
            entry_pat: 0,
            entry_efer: 0,
            exit_pat: 0,
            exit_efer: 0,
        };
        // SAFETY: this CPU owns both active VMX regions. VMCLEAR ends the prior
        // launch lifetime; every run installs complete fresh guest/host state.
        unsafe {
            success("msr-clear", vmx::vmclear(region))?;
            success("msr-load", vmx::vmptrld(region))?;
            let entry = controls(
                vmx::IA32_VMX_ENTRY_CTLS,
                vmx::IA32_VMX_TRUE_ENTRY_CTLS,
                requested_entry,
            )?;
            let exit = controls(
                vmx::IA32_VMX_EXIT_CTLS,
                vmx::IA32_VMX_TRUE_EXIT_CTLS,
                requested_exit,
            )?;
            equal(
                "msr-entry-matrix",
                u64::from(entry & ((1 << 14) | (1 << 15))),
                u64::from(requested_entry & ((1 << 14) | (1 << 15))),
            )?;
            equal(
                "msr-exit-matrix",
                u64::from(exit & (15 << 18)),
                u64::from(requested_exit & (15 << 18)),
            )?;
            configure(base, env, entry, exit)?;
            write(vmcs::GUEST_IA32_PAT, pat(1))?;
            write(vmcs::HOST_IA32_PAT, pat(5))?;
            write(vmcs::GUEST_IA32_EFER, guest_efer)?;
            write(vmcs::HOST_IA32_EFER, guest_efer)?;
            let _ = writeln!(serial, "thin-hv: MSR matrix case={variant}");
            let flags = enter(&mut frame, 0);
            if flags != 0 {
                return Err(Failure {
                    stage: "msr-entry-flags",
                    actual: read_field("msr-entry-error", vmcs::VM_INSTRUCTION_ERROR)?,
                    expected: 0,
                });
            }
            equal(
                "msr-exit-reason",
                read_field("msr-reason", vmcs::VM_EXIT_REASON)?,
                18,
            )?;
            equal(
                "msr-entry-pat",
                frame.entry_pat,
                if variant & 1 != 0 { pat(1) } else { pat(0) },
            )?;
            equal(
                "msr-entry-efer",
                frame.entry_efer,
                if variant & 8 != 0 {
                    guest_efer
                } else {
                    inherited_efer
                },
            )?;
            equal(
                "msr-exit-pat",
                frame.exit_pat,
                if variant & 4 != 0 { pat(5) } else { pat(4) },
            )?;
            equal(
                "msr-exit-efer",
                frame.exit_efer,
                if variant & 32 != 0 {
                    guest_efer
                } else {
                    inherited_efer
                },
            )?;
            equal(
                "msr-shadow-pat",
                read_field("msr-shadow-pat-read", vmcs::GUEST_IA32_PAT)?,
                if variant & 2 != 0 { pat(4) } else { pat(1) },
            )?;
            equal(
                "msr-shadow-efer",
                read_field("msr-shadow-efer-read", vmcs::GUEST_IA32_EFER)?,
                if variant & 16 != 0 {
                    inherited_efer
                } else {
                    guest_efer
                },
            )?;
        }
    }
    Ok(())
}

/// Hardware list loading, exact early/late error priority, ordered prefix
/// effects, ignored zero-count addresses and recovery after every failure.
unsafe fn entry_lists(
    base: u64,
    region: VmcsPhys,
    env: &HostEnvironment<'_>,
    serial: &mut Serial,
) -> Result<()> {
    // SAFETY: the VMX CPU supports these mandatory long-mode/PAT MSRs at CPL0.
    let (original_pat, original_efer) = unsafe { (cpu::rdmsr(PAT), cpu::rdmsr(EFER)) };
    let pat = |kind: u64| (original_pat & !(0xff << 32)) | (kind << 32);
    let address = base + (LIST_PAGE * PAGE) as u64;
    let mut late_field_changes = 0;
    for case in 0..20 {
        let mut count = match case {
            1 | 7 => 3,
            2 | 12 => 512,
            6 => 2,
            19 => 0,
            _ => 1,
        };
        let mut frame = Frame {
            fx: [0; 512],
            original_pat,
            original_efer,
            inherited_pat: pat(6),
            inherited_efer: original_efer,
            l2_pat: pat(5),
            l2_efer: original_efer,
            entry_pat: 0,
            entry_efer: 0,
            exit_pat: 0,
            exit_efer: 0,
        };
        let guest_pat = if case == 16 { u64::MAX } else { pat(0) };
        let mut expected_pat = pat(1);
        let mut expected_efer = original_efer;
        let mut failure_index = 0;
        // SAFETY: these two private fixture pages are exclusive aligned WB RAM;
        // only this L1 writes them. No L2 or hardware list processing runs while
        // they are prepared; counts never exceed the allocated 512 items.
        unsafe {
            core::ptr::write_bytes(address as *mut u8, 0, 2 * PAGE);
            for index in 0..count {
                let slot = (address as *mut u64).add(index * 2);
                slot.write(u64::from(PAT));
                slot.add(1).write(pat(if index & 1 == 0 { 1 } else { 4 }));
            }
            let item = |index: usize, msr: u64, value: u64| {
                let slot = (address as *mut u64).add(index * 2);
                slot.write(msr);
                slot.add(1).write(value);
            };
            match case {
                1 => {
                    item(1, u64::from(EFER), original_efer ^ 1);
                    item(2, u64::from(PAT), pat(4));
                    expected_pat = pat(4);
                    expected_efer ^= 1;
                }
                2 => expected_pat = pat(4),
                3 => {
                    item(0, (1 << 32) | u64::from(PAT), pat(1));
                    failure_index = 1;
                }
                4 => {
                    item(0, u64::from(u32::MAX), 0);
                    failure_index = 1;
                }
                5 => {
                    item(0, u64::from(PAT), u64::MAX);
                    failure_index = 1;
                }
                6 => {
                    item(1, u64::from(PAT), u64::MAX);
                    failure_index = 2;
                }
                7 => {
                    // WRMSR/list loading ignores the supplied read-only LMA bit.
                    item(1, u64::from(EFER), (original_efer ^ 1) & !(1 << 10));
                    item(2, u64::from(u32::MAX), 0);
                    expected_efer ^= 1;
                    failure_index = 3;
                }
                8..=11 => {
                    item(0, [0xc000_0101, 0xc000_0100, 0x802, 0x9b][case - 8], 0);
                    failure_index = 1;
                }
                12 => {
                    item(511, u64::from(u32::MAX), 0);
                    failure_index = 512;
                }
                19 => {
                    count = 0;
                    expected_pat = guest_pat;
                }
                _ => {}
            }
            if failure_index == 1 {
                expected_pat = guest_pat;
            }
            success("entry-list-clear", vmx::vmclear(region))?;
            success("entry-list-current", vmx::vmptrld(region))?;
            let entry = controls(
                vmx::IA32_VMX_ENTRY_CTLS,
                vmx::IA32_VMX_TRUE_ENTRY_CTLS,
                vmcs::VM_ENTRY_IA32E_MODE
                    | vmcs::VM_ENTRY_LOAD_IA32_PAT
                    | vmcs::VM_ENTRY_LOAD_IA32_EFER,
            )?;
            let host_load = case == 16 || case == 17;
            let exit = controls(
                vmx::IA32_VMX_EXIT_CTLS,
                vmx::IA32_VMX_TRUE_EXIT_CTLS,
                vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE
                    | vmcs::VM_EXIT_SAVE_IA32_PAT
                    | vmcs::VM_EXIT_SAVE_IA32_EFER
                    | if host_load {
                        vmcs::VM_EXIT_LOAD_IA32_PAT | vmcs::VM_EXIT_LOAD_IA32_EFER
                    } else {
                        0
                    },
            )?;
            configure(base, env, entry, exit)?;
            write(vmcs::GUEST_IA32_PAT, guest_pat)?;
            write(vmcs::GUEST_IA32_EFER, original_efer)?;
            write(vmcs::HOST_IA32_PAT, pat(5))?;
            write(vmcs::HOST_IA32_EFER, original_efer)?;
            let list_address = if count == 0 { u64::MAX } else { address };
            write(vmcs::VM_ENTRY_MSR_LOAD_ADDR, list_address)?;
            write(vmcs::VM_ENTRY_MSR_LOAD_COUNT, count as u64)?;
            if case == 15 {
                write(vmcs::HOST_CS_SELECTOR, 0)?;
            }
            if case == 17 {
                write(vmcs::GUEST_CR0, cpu::read_cr0() & !(1 << 31))?;
            }
            let _ = writeln!(serial, "thin-hv: MSR entry case={case}");
            let flags = enter(&mut frame, u64::from(case == 14));
            if case == 14 || case == 15 {
                equal("entry-list-early-flags", flags, 0x40)?;
                equal(
                    "entry-list-early-error",
                    read_field("entry-list-error", vmcs::VM_INSTRUCTION_ERROR)?,
                    if case == 14 { 5 } else { 8 },
                )?;
                equal(
                    "entry-list-no-early-load",
                    frame.exit_pat,
                    frame.inherited_pat,
                )?;
                equal(
                    "entry-list-no-early-efer",
                    frame.exit_efer,
                    frame.inherited_efer,
                )?;
            } else {
                equal("entry-list-flags", flags, 0)?;
                let reason = read_field("entry-list-reason", vmcs::VM_EXIT_REASON)?;
                if failure_index != 0 || host_load {
                    equal(
                        "entry-list-late-reason",
                        reason,
                        (1 << 31) | if host_load { 33 } else { 34 },
                    )?;
                    if !host_load {
                        equal(
                            "entry-list-failed-index",
                            read_field("entry-list-qualification", vmcs::EXIT_QUALIFICATION)?,
                            failure_index,
                        )?;
                    }
                    equal(
                        "entry-list-guest-not-run",
                        frame.entry_pat | frame.entry_efer,
                        0,
                    )?;
                    equal(
                        "entry-list-prefix-pat",
                        frame.exit_pat,
                        if host_load { pat(5) } else { expected_pat },
                    )?;
                    equal(
                        "entry-list-prefix-efer",
                        frame.exit_efer,
                        if host_load {
                            original_efer
                        } else {
                            expected_efer
                        },
                    )?;
                    // SDM 29.8 leaves the guest-state area unchanged on late
                    // entry failure. Continue only these field comparisons:
                    // the next case overwrites both fields before any entry.
                    // This records reference-KVM's eager PAT shadow update
                    // without dropping later tests or accepting the mismatch.
                    late_field_changes += u32::from(
                        read_field("entry-list-shadow-pat", vmcs::GUEST_IA32_PAT)? != guest_pat,
                    );
                    late_field_changes += u32::from(
                        read_field("entry-list-shadow-efer", vmcs::GUEST_IA32_EFER)?
                            != original_efer,
                    );
                } else {
                    equal("entry-list-normal-reason", reason, 18)?;
                    equal("entry-list-loaded-pat", frame.entry_pat, expected_pat)?;
                    equal("entry-list-loaded-efer", frame.entry_efer, expected_efer)?;
                    equal("entry-list-reflected-pat", frame.exit_pat, frame.l2_pat)?;
                    equal("entry-list-reflected-efer", frame.exit_efer, frame.l2_efer)?;
                }
            }
            equal(
                "entry-list-original-address",
                read_field("entry-list-address", vmcs::VM_ENTRY_MSR_LOAD_ADDR)?,
                list_address,
            )?;
            equal(
                "entry-list-original-count",
                read_field("entry-list-count", vmcs::VM_ENTRY_MSR_LOAD_COUNT)?,
                count as u64,
            )?;
            if case == 0 {
                // The source changes without VMCLEAR or any list-control write.
                // VMRESUME must load a fresh mirror, not the previous copy.
                (address as *mut u64).add(1).write(pat(4));
                write(vmcs::GUEST_RIP, guest as *const () as usize as u64)?;
                equal("entry-list-resume-flags", enter(&mut frame, 1), 0)?;
                equal(
                    "entry-list-resume-reason",
                    read_field("entry-list-resume-exit", vmcs::VM_EXIT_REASON)?,
                    18,
                )?;
                equal("entry-list-resume-fresh", frame.entry_pat, pat(4))?;
                equal(
                    "entry-list-resume-count",
                    read_field("entry-list-resume-list", vmcs::VM_ENTRY_MSR_LOAD_COUNT)?,
                    1,
                )?;
            }
        }
    }
    let _ = writeln!(
        serial,
        "thin-hv: MSR late-failure guest-field changes={late_field_changes}"
    );
    equal(
        "entry-list-no-failed-save-fields",
        u64::from(late_field_changes),
        0,
    )
}

/// No return after selecting private host tables; all pages stay allocated
/// until the disposable q35 VM powers off, including every error path.
pub(super) fn run(table: *mut efi::SystemTable, serial: &mut Serial) -> ! {
    let _ = writeln!(serial, "thin-hv: MSR contract START");
    let result = (|| {
        equal("msr-system-table", u64::from(!table.is_null()), 1)?;
        // SAFETY: firmware supplied the live system table to this UEFI entry.
        let boot = unsafe { (*table).boot_services };
        equal("msr-boot-services", u64::from(!boot.is_null()), 1)?;
        let caps = super::prerequisites()?;
        equal(
            "msr-fxsr",
            u64::from(cpu::cpuid(1, 0).edx & (1 << 24) != 0),
            1,
        )?;
        equal("msr-osfxsr", caps.cr4 & (1 << 9), 1 << 9)?;
        equal("msr-cet-disabled", caps.cr4 & (1 << 23), 0)?;
        equal("msr-fp-enabled", caps.cr0 & 12, 0)?;
        let mut base = u64::from(u32::MAX);
        // SAFETY: live firmware allocation interface and initialized output
        // slot; low WB pages are retained until the VM is powered off.
        let status = unsafe {
            ((*boot).allocate_pages)(
                efi::ALLOCATE_MAX_ADDRESS,
                efi::BOOT_SERVICES_DATA,
                PAGES,
                &mut base,
            )
        };
        equal("msr-allocate", status.as_usize() as u64, 0)?;
        super::allocation_map::<PAGES>(boot, base)?;
        let vmxon = VmxonPhys::new(base).ok_or(failure("msr-vmxon-address"))?;
        let region = VmcsPhys::new(base + PAGE as u64).ok_or(failure("msr-vmcs-address"))?;
        let stack = HostStack::new(base + (STACK_PAGE * PAGE) as u64, (2 * PAGE) as u64)
            .map_err(|_| failure("msr-stack"))?;
        // SAFETY: the checked exclusive WB allocation covers every zeroed byte
        // and both aligned revision headers. No CPU references these pages yet.
        unsafe {
            core::ptr::write_bytes(base as *mut u8, 0, PAGES * PAGE);
            (base as *mut u32).write(caps.revision);
            ((base + PAGE as u64) as *mut u32).write(caps.revision);
        }
        // SAFETY: this disjoint table/IST slice belongs solely to this CPU and
        // stays mapped by the unchanged identity CR3 until the VM powers off.
        let storage = unsafe {
            core::slice::from_raw_parts_mut(
                (base + (ENV_PAGE * PAGE) as u64) as *mut u8,
                HOST_ENVIRONMENT_PAGES * PAGE,
            )
        };
        // SAFETY: the owned table/IST and stack ranges are disjoint; no hardware
        // uses them yet. Fixture/HAL code remains executable; no firmware return.
        let env = unsafe {
            HostEnvironment::initialize(
                storage,
                stack,
                if caps.cr4 & (1 << 12) != 0 { 57 } else { 48 },
            )
        }
        .map_err(|_| failure("msr-environment"))?;
        // SAFETY: VMX prerequisites and fixed bits were checked. Disable only
        // this disposable CPU's maskable interrupts; private IDT handles faults.
        unsafe {
            asm!("cli", options(nomem, nostack));
            cpu::write_cr4(caps.cr4 | (1 << 13));
        }
        // SAFETY: the owned aligned WB region has the advertised revision.
        success("msr-vmxon", unsafe { vmx::vmxon(vmxon) })?;
        // SAFETY: this CPU now owns VMX operation and its private VMCS/pages.
        let result = unsafe {
            matrix(base, region, &env, serial)
                .and_then(|()| entry_lists(base, region, &env, serial))
        };
        // SAFETY: no L2 remains running; even failed attempts return here in L1
        // root. Clear the owned VMCS before VMXOFF; retain pages on any failure.
        unsafe {
            success("msr-final-clear", vmx::vmclear(region))?;
            success("msr-vmxoff", vmx::vmxoff())?;
            cpu::write_cr4(caps.cr4);
        }
        result
    })();
    match result {
        Ok(()) => {
            let _ = writeln!(
                serial,
                "thin-hv: MSR contract PASS matrix=128 entry_cases=20 entry_load=7 entry_resume=1 entry_fail=10 early_fail=2 guest_fail=2 vmxoff=1"
            );
        }
        Err(error) => {
            let _ = writeln!(
                serial,
                "thin-hv: MSR contract FAIL stage={} actual={:#x} expected={:#x}",
                error.stage, error.actual, error.expected
            );
        }
    }
    // SAFETY: this fixture is exclusively a disposable QEMU q35 test. Its
    // runner selects the q35 PM1 control port; no physical machine runs it.
    unsafe {
        asm!("out dx, ax", in("dx") 0x604_u16, in("ax") 0x2000_u16, options(nomem, nostack));
    }
    loop {
        // SAFETY: a failed shutdown cannot return to disposable firmware state.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

// SAFETY: matrix owns the clear current VMCS and aligned Frame, with CPL0,
// CR0.EM/TS=0 and OSFXSR=1. Only guest below runs, preserving RDI. The saved
// SysV callee registers and FP image are restored on every immediate failure
// or nested exit; no Rust executes before restoring original PAT/EFER.
#[unsafe(naked)]
unsafe extern "sysv64" fn enter(_frame: *mut Frame, _resume: u64) -> u64 {
    core::arch::naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "push rdi",
        "fxsave64 [rdi]",
        "mov eax, 0x6c14",
        "vmwrite rax, rsp",
        "jc 8f",
        "jz 8f",
        "lea rdx, [rip + 2f]",
        "mov eax, 0x6c16",
        "vmwrite rax, rdx",
        "jc 8f",
        "jz 8f",
        "mov rax, [rdi + 528]",
        "mov rdx, rax",
        "shr rdx, 32",
        "mov ecx, 0x277",
        "wrmsr",
        "mov rax, [rdi + 536]",
        "mov rdx, rax",
        "shr rdx, 32",
        "mov ecx, 0xc0000080",
        "wrmsr",
        "test rsi, rsi",
        "jnz 6f",
        "vmlaunch",
        "jmp 7f",
        "6:",
        "vmresume",
        "7:",
        "pushfq",
        "pop r8",
        "and r8d, 0x41",
        "jmp 3f",
        "2:",
        "xor r8d, r8d",
        "3:",
        "mov rdi, [rsp]",
        "mov ecx, 0x277",
        "rdmsr",
        "shl rdx, 32",
        "or rax, rdx",
        "mov [rdi + 576], rax",
        "mov ecx, 0xc0000080",
        "rdmsr",
        "shl rdx, 32",
        "or rax, rdx",
        "mov [rdi + 584], rax",
        "mov rax, [rdi + 512]",
        "mov rdx, rax",
        "shr rdx, 32",
        "mov ecx, 0x277",
        "wrmsr",
        "mov rax, [rdi + 520]",
        "mov rdx, rax",
        "shr rdx, 32",
        "mov ecx, 0xc0000080",
        "wrmsr",
        "jmp 9f",
        "8:",
        "mov r8d, 0x41",
        "9:",
        "fxrstor64 [rdi]",
        "mov rax, r8",
        "pop rdi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    );
}

// SAFETY: entered only with the validated owned L2 VMCS, identity mapping,
// CPL0, IF=0 and RDI pointing at the live aligned Frame. Only architecturally
// valid PAT/EFER values are written. VMCALL is the intended reflected exit;
// an erroneous resume stops at UD2 rather than falling into arbitrary bytes.
#[unsafe(naked)]
unsafe extern "sysv64" fn guest() -> ! {
    core::arch::naked_asm!(
        "mov ecx, 0x277",
        "rdmsr",
        "shl rdx, 32",
        "or rax, rdx",
        "mov [rdi + 560], rax",
        "mov ecx, 0xc0000080",
        "rdmsr",
        "shl rdx, 32",
        "or rax, rdx",
        "mov [rdi + 568], rax",
        "mov rax, [rdi + 544]",
        "mov rdx, rax",
        "shr rdx, 32",
        "mov ecx, 0x277",
        "wrmsr",
        "mov rax, [rdi + 552]",
        "mov rdx, rax",
        "shr rdx, 32",
        "mov ecx, 0xc0000080",
        "wrmsr",
        "vmcall",
        "ud2",
    );
}
