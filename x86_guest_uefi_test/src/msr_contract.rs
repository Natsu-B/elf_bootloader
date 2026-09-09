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
use x86_64_hal::addr::EptPhys;
use x86_64_hal::addr::VmcsPhys;
use x86_64_hal::addr::VmxonPhys;
use x86_64_hal::cpu;
use x86_64_hal::ept;
use x86_64_hal::host_state::HOST_ENVIRONMENT_PAGES;
use x86_64_hal::host_state::HostEnvironment;
use x86_64_hal::host_state::HostStack;
use x86_64_hal::vmcs;
use x86_64_hal::vmx;

// VMXON, VMCS, zero MSR bitmap, guest stack, host/IST environment and host stack.
const ENV_PAGE: usize = 4;
const STACK_PAGE: usize = ENV_PAGE + HOST_ENVIRONMENT_PAGES;
const LIST_PAGE: usize = STACK_PAGE + 2;
const VPID_PAGE: usize = LIST_PAGE + 6;
const EPT_PAGE: usize = VPID_PAGE + super::l1_memory::PAGES;
const DATA_PAGE: usize = EPT_PAGE + 5;
// Two aligned 2 MiB payloads, plus at most one 2 MiB alignment gap.
const SNAPSHOT_VMCS_PAGE: usize = DATA_PAGE + 3 * 512;
// Do not overwrite the stopped L2's CR3 tables with L1 operand-fault scratch.
const REPEAT_PAGE: usize = SNAPSHOT_VMCS_PAGE + 1;
const PAGES: usize = REPEAT_PAGE + super::l1_memory::PAGES;
const PAT: u32 = 0x277;
const EFER: u32 = 0xc000_0080;

#[cfg(all(feature = "msr-abort-store", feature = "msr-abort-load"))]
compile_error!("select exactly one terminal MSR-list fixture");

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
    l2_debugctl: u64,
}
const _: () = assert!(core::mem::offset_of!(Frame, exit_efer) == 584);
const _: () = assert!(core::mem::offset_of!(Frame, l2_debugctl) == 592);

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
            l2_debugctl: 0,
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
            l2_debugctl: 0,
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

/// Ordered exit stores and deferred host loads, including overlap, maximum
/// counts, changed VMRESUME sources and no-store late failed entries.
unsafe fn exit_lists(
    base: u64,
    region: VmcsPhys,
    env: &HostEnvironment<'_>,
    serial: &mut Serial,
) -> Result<()> {
    // SAFETY: the checked VMX long-mode CPU supports PAT/EFER at CPL0.
    let (original_pat, original_efer) = unsafe { (cpu::rdmsr(PAT), cpu::rdmsr(EFER)) };
    let pat = |kind: u64| (original_pat & !(0xff << 32)) | (kind << 32);
    let store = base + (LIST_PAGE * PAGE) as u64;
    let load = store + (2 * PAGE) as u64;
    let entry_list = load + (2 * PAGE) as u64;
    for case in 0..12 {
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
            l2_debugctl: 0,
        };
        let mut expected = [
            (PAT, pat(5)),
            (EFER, original_efer),
            (0, 0),
            (0, 0),
            (0, 0),
            (0, 0),
            (0, 0),
        ];
        let count = match case {
            0 | 6 | 9 | 10 => 1,
            1 => 7,
            2 => 512,
            11 => 0,
            _ => 2,
        };
        let loads = match case {
            0 | 6 | 9 | 10 => 1,
            1 => 3,
            2 => 512,
            11 => 0,
            _ => 2,
        };
        let late = case == 4 || case == 5;
        // SAFETY: all six list pages are exclusive, aligned fixture RAM, and
        // hardware/L2 is stopped. List indices are bounded by their 512 slots.
        // The VMCS and Frame remain live on this CPU through each entry/exit.
        unsafe {
            core::ptr::write_bytes(store as *mut u8, 0, 6 * PAGE);
            let item = |address: u64, index: usize, msr: u32, value: u64| {
                let slot = (address as *mut u64).add(index * 2);
                slot.write(u64::from(msr));
                slot.add(1).write(value);
            };
            if case == 1 {
                expected = [
                    (PAT, pat(5)),
                    (EFER, original_efer),
                    (0xc000_0100, 0),
                    (0xc000_0101, 0),
                    (0x174, 0x10),
                    (0x175, 0x123400),
                    (0x176, 0x567800),
                ];
            } else if case == 6 {
                expected[0] = (0x1d9, 2);
            } else if case == 7 {
                expected[0] = (vmx::IA32_VMX_BASIC, cpu::rdmsr(vmx::IA32_VMX_BASIC));
                expected[1] = (
                    vmx::IA32_VMX_EPT_VPID_CAP,
                    cpu::rdmsr(vmx::IA32_VMX_EPT_VPID_CAP),
                );
            } else if case == 8 {
                expected[0] = (0xc000_0100, 0x123400);
                expected[1] = (0xc000_0101, 0x567800);
            }
            for index in 0..count {
                item(
                    store,
                    index,
                    expected[if case == 2 { 0 } else { index }].0,
                    u64::MAX,
                );
            }
            for index in 0..loads {
                item(load, index, PAT, pat(if case == 2 { 4 } else { 1 }));
            }
            if loads >= 2 && case != 2 {
                item(load, 1, EFER, original_efer ^ 1);
            }
            if case == 1 {
                item(load, 2, PAT, pat(4));
            }
            item(
                entry_list,
                0,
                if case == 4 { u32::MAX } else { PAT },
                pat(4),
            );
            success("exit-list-clear", vmx::vmclear(region))?;
            success("exit-list-current", vmx::vmptrld(region))?;
            let entry = controls(
                vmx::IA32_VMX_ENTRY_CTLS,
                vmx::IA32_VMX_TRUE_ENTRY_CTLS,
                vmcs::VM_ENTRY_IA32E_MODE
                    | vmcs::VM_ENTRY_LOAD_IA32_PAT
                    | vmcs::VM_ENTRY_LOAD_IA32_EFER
                    | if case == 6 { 1 << 2 } else { 0 },
            )?;
            let exit = controls(
                vmx::IA32_VMX_EXIT_CTLS,
                vmx::IA32_VMX_TRUE_EXIT_CTLS,
                vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE
                    | vmcs::VM_EXIT_LOAD_IA32_PAT
                    | vmcs::VM_EXIT_LOAD_IA32_EFER,
            )?;
            configure(base, env, entry, exit)?;
            write(
                vmcs::GUEST_IA32_PAT,
                if case == 5 { u64::MAX } else { pat(0) },
            )?;
            write(vmcs::GUEST_IA32_EFER, original_efer)?;
            write(vmcs::HOST_IA32_PAT, original_pat)?;
            write(vmcs::HOST_IA32_EFER, original_efer)?;
            write(
                vmcs::VM_EXIT_MSR_STORE_ADDR,
                if count == 0 { u64::MAX } else { store },
            )?;
            write(vmcs::VM_EXIT_MSR_STORE_COUNT, count as u64)?;
            let host_address = if case == 3 {
                store
            } else if loads == 0 {
                u64::MAX
            } else {
                load
            };
            write(vmcs::VM_EXIT_MSR_LOAD_ADDR, host_address)?;
            write(vmcs::VM_EXIT_MSR_LOAD_COUNT, loads as u64)?;
            if case == 4 || case == 10 {
                write(vmcs::VM_ENTRY_MSR_LOAD_ADDR, entry_list)?;
                write(vmcs::VM_ENTRY_MSR_LOAD_COUNT, 1)?;
            }
            if case == 1 {
                write(vmcs::GUEST_SYSENTER_CS, 0x10)?;
                write(vmcs::GUEST_SYSENTER_ESP, 0x123400)?;
                write(vmcs::GUEST_SYSENTER_EIP, 0x567800)?;
            }
            if case == 6 {
                write(vmcs::GUEST_IA32_DEBUGCTL, 2)?;
            }
            if case == 8 {
                write(vmcs::GUEST_FS_BASE, 0x123400)?;
                write(vmcs::GUEST_GS_BASE, 0x567800)?;
            }
            let _ = writeln!(serial, "thin-hv: MSR exit case={case}");
            equal("exit-list-flags", enter(&mut frame, 0), 0)?;
            if case == 6 {
                // KVM may mask BTF when its virtual CPU has no LBR support.
                // Compare the actual L2 register, not an assumed enabled bit;
                // report this limitation so zero cannot imply nonzero coverage.
                equal("exit-list-debugctl-bits", frame.l2_debugctl & !2, 0)?;
                let _ = writeln!(
                    serial,
                    "thin-hv: MSR debugctl requested=2 observed={}",
                    frame.l2_debugctl
                );
                expected[0].1 = frame.l2_debugctl;
            }
            equal(
                "exit-list-reason",
                read_field("exit-list-reason-read", vmcs::VM_EXIT_REASON)?,
                if late {
                    (1 << 31) | if case == 4 { 34 } else { 33 }
                } else {
                    18
                },
            )?;
            if late {
                equal("exit-list-no-guest", frame.entry_pat | frame.entry_efer, 0)?;
            } else {
                equal(
                    "exit-list-entry-pat",
                    frame.entry_pat,
                    if case == 10 { pat(4) } else { pat(0) },
                )?;
            }
            for index in 0..count {
                let wanted = expected[if case == 2 { 0 } else { index }];
                let slot = (store as *const u64).add(index * 2);
                equal("exit-list-store-index", slot.read(), u64::from(wanted.0))?;
                equal(
                    "exit-list-store-value",
                    slot.add(1).read(),
                    if late { u64::MAX } else { wanted.1 },
                )?;
            }
            equal(
                "exit-list-host-pat",
                frame.exit_pat,
                match case {
                    1 | 2 => pat(4),
                    3 => pat(5),
                    11 => original_pat,
                    _ => pat(1),
                },
            )?;
            equal(
                "exit-list-host-efer",
                frame.exit_efer,
                if loads >= 2 && case != 2 && case != 3 {
                    original_efer ^ 1
                } else {
                    original_efer
                },
            )?;
            equal(
                "exit-list-original-store-address",
                read_field("exit-list-store-address", vmcs::VM_EXIT_MSR_STORE_ADDR)?,
                if count == 0 { u64::MAX } else { store },
            )?;
            equal(
                "exit-list-original-load-address",
                read_field("exit-list-load-address", vmcs::VM_EXIT_MSR_LOAD_ADDR)?,
                host_address,
            )?;
            // The return stub already restored original PAT. A subsequent L0
            // CPUID exit must not reload this reflection's old host PAT list.
            let _ = cpu::cpuid(0, 0);
            equal("exit-list-no-replay", cpu::rdmsr(PAT), original_pat)?;
            if case == 9 {
                item(load, 0, PAT, pat(4));
                item(store, 0, PAT, u64::MAX);
                write(vmcs::GUEST_RIP, guest as *const () as usize as u64)?;
                equal("exit-list-resume", enter(&mut frame, 1), 0)?;
                equal(
                    "exit-list-resume-reason",
                    read_field("exit-list-resume-read", vmcs::VM_EXIT_REASON)?,
                    18,
                )?;
                equal("exit-list-resume-fresh-host", frame.exit_pat, pat(4))?;
                equal(
                    "exit-list-resume-fresh-store",
                    (store as *const u64).add(1).read(),
                    pat(5),
                )?;
            }
        }
    }
    Ok(())
}

/// Each successful control write must invalidate a previously warm policy
/// cache. A null host CS independently prevents any invalid entry from running.
unsafe fn control_cache(
    base: u64,
    region: VmcsPhys,
    env: &HostEnvironment<'_>,
    serial: &mut Serial,
) -> Result<()> {
    // SAFETY: this CPU owns the aligned VMCS and retained fixture pages. All
    // successful entries run only guest(), with valid PAT/EFER and private
    // descriptors; invalid attempts retain HOST_CS=0 until fully restored.
    unsafe {
        let original_pat = cpu::rdmsr(PAT);
        let original_efer = cpu::rdmsr(EFER);
        let mut frame = Frame {
            fx: [0; 512],
            original_pat,
            original_efer,
            inherited_pat: original_pat,
            inherited_efer: original_efer,
            l2_pat: original_pat,
            l2_efer: original_efer,
            entry_pat: 0,
            entry_efer: 0,
            exit_pat: 0,
            exit_efer: 0,
            l2_debugctl: 0,
        };
        success("control-cache-clear", vmx::vmclear(region))?;
        success("control-cache-current", vmx::vmptrld(region))?;
        let entry = controls(
            vmx::IA32_VMX_ENTRY_CTLS,
            vmx::IA32_VMX_TRUE_ENTRY_CTLS,
            vmcs::VM_ENTRY_IA32E_MODE
                | vmcs::VM_ENTRY_LOAD_IA32_PAT
                | vmcs::VM_ENTRY_LOAD_IA32_EFER,
        )?;
        let exit = controls(
            vmx::IA32_VMX_EXIT_CTLS,
            vmx::IA32_VMX_TRUE_EXIT_CTLS,
            vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE
                | vmcs::VM_EXIT_LOAD_IA32_PAT
                | vmcs::VM_EXIT_LOAD_IA32_EFER,
        )?;
        configure(base, env, entry, exit)?;
        // Warm the cache with secondary controls already active, so the later
        // secondary-only write cannot rely on a primary write to invalidate it.
        write(
            vmcs::CPU_BASED_VM_EXEC_CONTROL,
            read_field("control-cache-primary", vmcs::CPU_BASED_VM_EXEC_CONTROL)? | (1 << 31),
        )?;
        for (field, value) in [
            (vmcs::GUEST_IA32_PAT, original_pat),
            (vmcs::HOST_IA32_PAT, original_pat),
            (vmcs::GUEST_IA32_EFER, original_efer),
            (vmcs::HOST_IA32_EFER, original_efer),
        ] {
            write(field, value)?;
        }
        equal("control-cache-warm-entry", enter(&mut frame, 0), 0)?;
        equal(
            "control-cache-warm-exit",
            read_field("control-cache-warm-reason", vmcs::VM_EXIT_REASON)?,
            18,
        )?;
        let basic = vmx::VmxBasic::from_msr(cpu::rdmsr(vmx::IA32_VMX_BASIC));
        let fields = [
            vmcs::PIN_BASED_VM_EXEC_CONTROL,
            vmcs::CPU_BASED_VM_EXEC_CONTROL,
            vmcs::SECONDARY_VM_EXEC_CONTROL,
            vmcs::VM_ENTRY_CONTROLS,
            vmcs::VM_EXIT_CONTROLS,
        ];
        let capabilities = if basic.true_controls {
            [
                vmx::IA32_VMX_TRUE_PINBASED_CTLS,
                vmx::IA32_VMX_TRUE_PROCBASED_CTLS,
                vmx::IA32_VMX_PROCBASED_CTLS2,
                vmx::IA32_VMX_TRUE_ENTRY_CTLS,
                vmx::IA32_VMX_TRUE_EXIT_CTLS,
            ]
        } else {
            [
                vmx::IA32_VMX_PINBASED_CTLS,
                vmx::IA32_VMX_PROCBASED_CTLS,
                vmx::IA32_VMX_PROCBASED_CTLS2,
                vmx::IA32_VMX_ENTRY_CTLS,
                vmx::IA32_VMX_EXIT_CTLS,
            ]
        };
        let preferred = [1 << 7, 1 << 17, 1 << 6, 1 << 16, 1 << 23];
        let mut saved = [0; 5];
        for (slot, field) in saved.iter_mut().zip(fields) {
            *slot = read_field("control-cache-save", field)?;
        }
        let cs = read_field("control-cache-save-cs", vmcs::HOST_CS_SELECTOR)?;
        for index in 0..fields.len() {
            let unavailable = !(cpu::rdmsr(capabilities[index]) >> 32) as u32;
            equal("control-cache-unavailable", u64::from(unavailable != 0), 1)?;
            let bad = if unavailable & preferred[index] != 0 {
                preferred[index]
            } else {
                1 << unavailable.trailing_zeros()
            };
            write(vmcs::HOST_CS_SELECTOR, 0)?;
            write(fields[index], saved[index] | u64::from(bad))?;
            let result = (|| {
                equal("control-cache-invalid-flags", enter(&mut frame, 1), 0x40)?;
                equal(
                    "control-cache-invalid-error",
                    read_field("control-cache-error", vmcs::VM_INSTRUCTION_ERROR)?,
                    7,
                )
            })();
            for (field, value) in fields.into_iter().zip(saved) {
                write(field, value)?;
            }
            write(vmcs::HOST_CS_SELECTOR, cs)?;
            result?;
            write(vmcs::GUEST_RIP, guest as *const () as usize as u64)?;
            equal("control-cache-recovered-entry", enter(&mut frame, 1), 0)?;
            equal(
                "control-cache-recovered-exit",
                read_field("control-cache-reason", vmcs::VM_EXIT_REASON)?,
                18,
            )?;
        }
        let _ = writeln!(serial, "thin-hv: MSR control cache PASS invalid=5 resume=5");
        Ok(())
    }
}

/// Real tagged translations, all advertised invalidation types, and repeated
/// VMX periods reusing the same VMXON/VMCS/CR3/VPID. Intel does not require
/// VMXOFF/VMXON to flush; only the Direct runner requires lease_fresh=64.
unsafe fn vpid_lifetime(
    base: u64,
    region: VmcsPhys,
    env: &HostEnvironment<'_>,
    serial: &mut Serial,
    vmx_active: &mut bool,
) -> Result<()> {
    // SAFETY: the checked retained fixture owns the WB VMX regions and seven
    // disjoint paging/payload pages. Only this CPU accesses them, with IF=0.
    // Every entry uses copied live firmware mappings plus two owned leaves;
    // no firmware call or free occurs after the private host tables are loaded.
    unsafe {
        let original_pat = cpu::rdmsr(PAT);
        let original_efer = cpu::rdmsr(EFER);
        let cr3 = cpu::read_cr3();
        let root = base + (VPID_PAGE * PAGE) as u64;
        let (linear, pte, data) =
            super::l1_memory::prepare_pages(root, cr3, cpu::read_cr4() & (1 << 12) != 0)?;
        let a = 0x1234_5678_9abc_def0;
        let b = 0xfedc_ba98_7654_3210;
        (data as *mut u64).write_volatile(a);
        ((data - PAGE as u64) as *mut u64).write_volatile(b);
        let mut frame = Frame {
            fx: [0; 512],
            original_pat,
            original_efer,
            inherited_pat: original_pat,
            inherited_efer: original_efer,
            l2_pat: original_pat,
            l2_efer: original_efer,
            entry_pat: 0,
            entry_efer: 0,
            exit_pat: 0,
            exit_efer: 0,
            // The dedicated assembly uses this slot as its input address and
            // entry_pat as the observed load, without executing the MSR guest.
            l2_debugctl: linear,
        };
        let entry = controls(
            vmx::IA32_VMX_ENTRY_CTLS,
            vmx::IA32_VMX_TRUE_ENTRY_CTLS,
            vmcs::VM_ENTRY_IA32E_MODE
                | vmcs::VM_ENTRY_LOAD_IA32_PAT
                | vmcs::VM_ENTRY_LOAD_IA32_EFER,
        )?;
        let exit = controls(
            vmx::IA32_VMX_EXIT_CTLS,
            vmx::IA32_VMX_TRUE_EXIT_CTLS,
            vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE
                | vmcs::VM_EXIT_LOAD_IA32_PAT
                | vmcs::VM_EXIT_LOAD_IA32_EFER,
        )?;
        let secondary = controls(
            vmx::IA32_VMX_PROCBASED_CTLS2,
            vmx::IA32_VMX_PROCBASED_CTLS2,
            1 << 5,
        )?;
        equal(
            "vpid-all-types",
            cpu::rdmsr(vmx::IA32_VMX_EPT_VPID_CAP) >> 40 & 15,
            15,
        )?;
        let prepare = |tag| -> Result<()> {
            success("vpid-clear", vmx::vmclear(region))?;
            success("vpid-current", vmx::vmptrld(region))?;
            configure(base, env, entry, exit)?;
            write(
                vmcs::CPU_BASED_VM_EXEC_CONTROL,
                read_field("vpid-primary", vmcs::CPU_BASED_VM_EXEC_CONTROL)? | (1 << 31),
            )?;
            for (field, value) in [
                (vmcs::SECONDARY_VM_EXEC_CONTROL, u64::from(secondary)),
                (vmcs::VIRTUAL_PROCESSOR_ID, tag),
                (vmcs::GUEST_CR3, root | (cr3 & (0xfff | (3 << 61)))),
                (vmcs::GUEST_RIP, vpid_guest as *const () as usize as u64),
                (vmcs::GUEST_IA32_PAT, original_pat),
                (vmcs::HOST_IA32_PAT, original_pat),
                (vmcs::GUEST_IA32_EFER, original_efer),
                (vmcs::HOST_IA32_EFER, original_efer),
            ] {
                write(field, value)?;
            }
            Ok(())
        };
        let invalidate = |kind, tag| {
            success(
                "vpid-invalidate",
                vmx::invvpid(
                    kind,
                    &vmx::InvvpidDescriptor {
                        vpid: tag,
                        linear_address: linear,
                        ..vmx::InvvpidDescriptor::default()
                    },
                ),
            )
        };
        let run = |frame: &mut Frame, resume| -> Result<u64> {
            write(vmcs::GUEST_RIP, vpid_guest as *const () as usize as u64)?;
            equal("vpid-entry", enter(frame, resume), 0)?;
            equal(
                "vpid-exit",
                read_field("vpid-reason", vmcs::VM_EXIT_REASON)?,
                18,
            )?;
            Ok(frame.entry_pat)
        };
        prepare(0)?;
        equal("vpid-zero-flags", enter(&mut frame, 0), 0x40)?;
        equal(
            "vpid-zero-error",
            read_field("vpid-error", vmcs::VM_INSTRUCTION_ERROR)?,
            7,
        )?;
        for tag in [1, u16::MAX] {
            for kind in 0..4 {
                prepare(u64::from(tag))?;
                pte.write_volatile(data | 3);
                invalidate(1, tag)?;
                equal("vpid-initial-load", run(&mut frame, 0)?, a)?;
                pte.write_volatile((data - PAGE as u64) | 3);
                invalidate(kind, if kind == 2 { 0 } else { tag })?;
                equal("vpid-invalidated-load", run(&mut frame, 1)?, b)?;
            }
        }
        let owner = VmxonPhys::new(base).ok_or(failure("vpid-vmxon-address"))?;
        let mut fresh = 0;
        for cycle in 0..64 {
            let tag = if cycle & 1 == 0 { 1 } else { u16::MAX };
            prepare(u64::from(tag))?;
            pte.write_volatile(data | 3);
            invalidate(1, tag)?;
            equal("vpid-lease-initial-load", run(&mut frame, 0)?, a)?;
            pte.write_volatile((data - PAGE as u64) | 3);
            success("vpid-lease-clear", vmx::vmclear(region))?;
            success("vpid-lease-off", vmx::vmxoff())?;
            *vmx_active = false;
            success("vpid-lease-on", vmx::vmxon(owner))?;
            *vmx_active = true;
            prepare(u64::from(tag))?;
            let observed = run(&mut frame, 0)?;
            // A reference may retain or invalidate: both are architectural.
            // The backend-specific log gate enforces Direct's stronger lease.
            equal(
                "vpid-lease-value",
                u64::from(observed == a || observed == b),
                1,
            )?;
            fresh += u32::from(observed == b);
        }
        let _ = writeln!(
            serial,
            "thin-hv: MSR VPID PASS tags=2 invalid=1 types=4 invalidations=8"
        );
        let _ = writeln!(serial, "thin-hv: MSR VPID lease cycles=64 fresh={fresh}");
        Ok(())
    }
}

/// Controlled q35 hardware proof before exposing 2 MiB EPT to ordinary L1s.
/// Direct already requires physical 2 MiB support for its smoke carrier. This
/// fixture intentionally exercises it even while L1's capability bit is hidden;
/// that is a test-only probe, never a guest software capability-selection rule.
unsafe fn ept_large_pages(
    base: u64,
    region: VmcsPhys,
    env: &HostEnvironment<'_>,
    serial: &mut Serial,
) -> Result<()> {
    const HUGE: u64 = 2 * 1024 * 1024;
    const GPA: u64 = 1 << 32;
    const WB: u64 = 6 << 3;
    const LARGE: u64 = 1 << 7;
    // SAFETY: the q35-only fixture retains its checked contiguous low WB
    // allocation. The five EPT tables and two aligned 2 MiB payloads are
    // disjoint from paging, VMX, descriptors and stacks. Only this CPU uses
    // them, with IF=0; every mutation occurs with L2 stopped and is followed
    // by INVEPT before re-entry. No physical device or firmware table is touched.
    unsafe {
        equal(
            "ept-low-fixture",
            u64::from(base + (PAGES * PAGE) as u64 <= 1 << 30),
            1,
        )?;
        let capability = cpu::rdmsr(vmx::IA32_VMX_EPT_VPID_CAP);
        equal("ept-invept-types", capability & (3 << 25), 3 << 25)?;
        success("ept-clear", vmx::vmclear(region))?;
        success("ept-current", vmx::vmptrld(region))?;
        let original_pat = cpu::rdmsr(PAT);
        let original_efer = cpu::rdmsr(EFER);
        let cr3 = cpu::read_cr3();
        let root = base + (VPID_PAGE * PAGE) as u64;
        let (linear, pte, _) =
            super::l1_memory::prepare_pages(root, cr3, cpu::read_cr4() & (1 << 12) != 0)?;
        pte.write_volatile(GPA | 3);
        let tables = base + (EPT_PAGE * PAGE) as u64;
        let physical = |offset| EptPhys::new(tables + offset).ok_or(failure("ept-table-address"));
        let eptp = ept::build_identity_1g(
            &mut *(tables as *mut ept::EptPage),
            physical(0)?,
            &mut *((tables + PAGE as u64) as *mut ept::EptPage),
            physical(PAGE as u64)?,
            &mut *((tables + 2 * PAGE as u64) as *mut ept::EptPage),
            physical(2 * PAGE as u64)?,
        );
        let pd = (tables + 3 * PAGE as u64) as *mut u64;
        let pt = (tables + 4 * PAGE as u64) as *mut u64;
        ((tables + PAGE as u64) as *mut u64)
            .add(4)
            .write_volatile(pd as u64 | 7);
        let a = (base + (DATA_PAGE * PAGE) as u64 + HUGE - 1) & !(HUGE - 1);
        let b = a + HUGE;
        equal(
            "ept-payload-bounds",
            u64::from(b + HUGE <= base + (SNAPSHOT_VMCS_PAGE * PAGE) as u64),
            1,
        )?;
        (a as *mut u64).write_volatile(0x1122_3344);
        (b as *mut u64).write_volatile(0x5566_7788);
        pd.write_volatile(a | WB | LARGE | 3);
        let mut frame = Frame {
            fx: [0; 512],
            original_pat,
            original_efer,
            inherited_pat: original_pat,
            inherited_efer: original_efer,
            l2_pat: 0xaabb_ccdd,
            l2_efer: original_efer,
            entry_pat: 0,
            entry_efer: 0,
            exit_pat: 0,
            exit_efer: 0,
            l2_debugctl: linear,
        };
        let entry = controls(
            vmx::IA32_VMX_ENTRY_CTLS,
            vmx::IA32_VMX_TRUE_ENTRY_CTLS,
            vmcs::VM_ENTRY_IA32E_MODE
                | vmcs::VM_ENTRY_LOAD_IA32_PAT
                | vmcs::VM_ENTRY_LOAD_IA32_EFER,
        )?;
        let exit = controls(
            vmx::IA32_VMX_EXIT_CTLS,
            vmx::IA32_VMX_TRUE_EXIT_CTLS,
            vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE
                | vmcs::VM_EXIT_LOAD_IA32_PAT
                | vmcs::VM_EXIT_LOAD_IA32_EFER,
        )?;
        configure(base, env, entry, exit)?;
        let secondary = controls(
            vmx::IA32_VMX_PROCBASED_CTLS2,
            vmx::IA32_VMX_PROCBASED_CTLS2,
            1 << 1,
        )?;
        write(
            vmcs::CPU_BASED_VM_EXEC_CONTROL,
            read_field("ept-primary", vmcs::CPU_BASED_VM_EXEC_CONTROL)? | (1 << 31),
        )?;
        for (field, value) in [
            (vmcs::SECONDARY_VM_EXEC_CONTROL, u64::from(secondary)),
            (vmcs::EPT_POINTER, eptp),
            (vmcs::GUEST_CR3, root | (cr3 & (0xfff | (3 << 61)))),
            (vmcs::GUEST_IA32_PAT, original_pat),
            (vmcs::HOST_IA32_PAT, original_pat),
            (vmcs::GUEST_IA32_EFER, original_efer),
            (vmcs::HOST_IA32_EFER, original_efer),
        ] {
            write(field, value)?;
        }
        let invalidate = |kind| {
            success(
                "ept-invept",
                vmx::invept(
                    kind,
                    &vmx::InveptDescriptor {
                        ept_pointer: eptp,
                        reserved: 0,
                    },
                ),
            )
        };
        let run = |frame: &mut Frame, resume, store, expected| -> Result<()> {
            write(
                vmcs::GUEST_RIP,
                if store {
                    ept_store_guest as *const ()
                } else {
                    vpid_guest as *const ()
                } as usize as u64,
            )?;
            equal("ept-entry", enter(frame, resume), 0)?;
            equal(
                "ept-exit",
                read_field("ept-reason", vmcs::VM_EXIT_REASON)?,
                expected,
            )
        };
        let fault = |access: u64, permissions: u64| -> Result<()> {
            // Repeated L1 VMREADs hit the project's retained exit snapshot.
            // This field is defined for these EPT violations; its high alias
            // must not be confused with an unsupported natural-width alias.
            for _ in 0..8 {
                equal(
                    "ept-gpa-high",
                    read_field("ept-high", vmcs::GUEST_PHYSICAL_ADDRESS + 1)?,
                    GPA >> 32,
                )?;
                equal(
                    "ept-gpa-repeat",
                    read_field("ept-full", vmcs::GUEST_PHYSICAL_ADDRESS)?,
                    GPA,
                )?;
            }
            equal(
                "ept-fault-address",
                read_field("ept-gpa", vmcs::GUEST_PHYSICAL_ADDRESS)?,
                GPA,
            )?;
            let qualification = read_field("ept-qualification", vmcs::EXIT_QUALIFICATION)?;
            equal(
                "ept-fault-access",
                qualification & 0x3f,
                access | (permissions << 3),
            )?;
            equal("ept-fault-linear-valid", qualification & (3 << 7), 3 << 7)?;
            equal(
                "ept-fault-linear",
                read_field("ept-gla", vmcs::GUEST_LINEAR_ADDRESS)?,
                linear,
            )
        };
        invalidate(2)?;
        run(&mut frame, 0, false, 18)?;
        equal("ept-large-load", frame.entry_pat, 0x1122_3344)?;
        pd.write_volatile(a | WB | LARGE | 1);
        invalidate(1)?;
        run(&mut frame, 1, true, 48)?;
        fault(2, 1)?;
        equal(
            "ept-large-no-store",
            (a as *const u64).read_volatile(),
            0x1122_3344,
        )?;
        pd.write_volatile(a | WB | LARGE | 3);
        invalidate(1)?;
        // Resume the exact faulting store without rewriting GUEST_RIP. L1's
        // handler explicitly restores its two live guest operands (mode 2).
        equal("ept-large-recover-entry", enter(&mut frame, 2), 0)?;
        equal(
            "ept-large-recover-exit",
            read_field("ept-reason", vmcs::VM_EXIT_REASON)?,
            18,
        )?;
        equal(
            "ept-large-store",
            (a as *const u64).read_volatile(),
            frame.l2_pat,
        )?;
        for index in 0..512 {
            pt.add(index)
                .write_volatile(a + (index * PAGE) as u64 | WB | 3);
        }
        pd.write_volatile(pt as u64 | 7);
        invalidate(1)?;
        run(&mut frame, 1, false, 18)?;
        equal("ept-split-load", frame.entry_pat, frame.l2_pat)?;
        pt.write_volatile(a | WB | 1);
        invalidate(1)?;
        frame.l2_pat ^= u64::MAX;
        run(&mut frame, 1, true, 48)?;
        fault(2, 1)?;
        equal(
            "ept-split-no-store",
            (a as *const u64).read_volatile(),
            frame.l2_pat ^ u64::MAX,
        )?;
        pt.write_volatile(a | WB | 3);
        invalidate(1)?;
        equal("ept-split-recover-entry", enter(&mut frame, 2), 0)?;
        equal(
            "ept-split-recover-exit",
            read_field("ept-reason", vmcs::VM_EXIT_REASON)?,
            18,
        )?;
        equal(
            "ept-split-store",
            (a as *const u64).read_volatile(),
            frame.l2_pat,
        )?;
        pd.write_volatile(b | WB | LARGE | 3);
        invalidate(2)?;
        run(&mut frame, 1, false, 18)?;
        equal("ept-replacement-load", frame.entry_pat, 0x5566_7788)?;
        // Bits 20:12 are reserved in a present 2 MiB leaf: a nested EPT
        // misconfiguration must return to L1, not stop the project monitor.
        pd.write_volatile(b | PAGE as u64 | WB | LARGE | 3);
        invalidate(2)?;
        run(&mut frame, 1, false, 49)?;
        equal(
            "ept-misconfig-address",
            read_field("ept-gpa", vmcs::GUEST_PHYSICAL_ADDRESS)?,
            GPA,
        )?;
        pd.write_volatile(0);
        invalidate(2)?;
        run(&mut frame, 1, false, 48)?;
        fault(1, 0)?;
        pd.write_volatile(b | WB | LARGE | 3);
        invalidate(2)?;
        equal("ept-absent-recover-entry", enter(&mut frame, 2), 0)?;
        equal(
            "ept-absent-recover-exit",
            read_field("ept-reason", vmcs::VM_EXIT_REASON)?,
            18,
        )?;
        equal("ept-recovered-load", frame.entry_pat, 0x5566_7788)?;
        let _ = writeln!(
            serial,
            "thin-hv: MSR EPT2M proof PASS advertised={} large=1 split=1 replacement=1 violations=3 misconfig=1 recovery=3 invept=10",
            (capability >> 16) & 1
        );
        for _ in 0..8 {
            equal(
                "snapshot-warm",
                read_field("snapshot-reason", vmcs::VM_EXIT_REASON)?,
                18,
            )?;
        }
        // L2 is stopped at VMCALL. These temporary values are never entered;
        // verify exact hardware write-through (including 32-bit truncation),
        // rejected aliases, and restoration before any later entry attempt.
        let mut guest_fields = [
            (vmcs::GUEST_RIP, 0),
            (vmcs::GUEST_RFLAGS, 0),
            (vmcs::GUEST_CS_AR_BYTES, 0),
            (vmcs::GUEST_INTERRUPTIBILITY_INFO, 0),
        ];
        for (field, original) in &mut guest_fields {
            *original = read_field("snapshot-guest-original", *field)?;
            let changed = *original ^ 1;
            let narrow = matches!(
                *field,
                vmcs::GUEST_CS_AR_BYTES | vmcs::GUEST_INTERRUPTIBILITY_INFO
            );
            write(*field, changed | if narrow { 0xa5a5_5a5a << 32 } else { 0 })?;
            equal(
                "snapshot-guest-written",
                read_field("snapshot-guest", *field)?,
                changed,
            )?;
            equal(
                "snapshot-guest-alias-rejected",
                u64::from(vmx::vmwrite(*field + 1, 0) == vmx::VmxStatus::FailValid),
                1,
            )?;
            equal(
                "snapshot-guest-alias-error",
                read_field("snapshot-error", vmcs::VM_INSTRUCTION_ERROR)?,
                12,
            )?;
            equal(
                "snapshot-guest-failure-unchanged",
                read_field("snapshot-guest", *field)?,
                changed,
            )?;
            // A successful same-value write must leave the previous VMfail
            // error intact. Exercise both register and faulting/mapped memory
            // sources while L0's stopped-VMCS snapshot is populated.
            let repeated = changed | if narrow { 0x5a5a_a5a5 << 32 } else { 0 };
            write(*field, repeated)?;
            equal(
                "snapshot-repeat-error",
                read_field("snapshot-error", vmcs::VM_INSTRUCTION_ERROR)?,
                12,
            )?;
            super::l1_memory::repeat_vmwrite(base + (REPEAT_PAGE * PAGE) as u64, *field, repeated)?;
            equal(
                "snapshot-repeat-memory-error",
                read_field("snapshot-error", vmcs::VM_INSTRUCTION_ERROR)?,
                12,
            )?;
            equal(
                "snapshot-repeat-value",
                read_field("snapshot-guest", *field)?,
                changed,
            )?;
            write(*field, *original)?;
            equal(
                "snapshot-guest-restored",
                read_field("snapshot-guest", *field)?,
                *original,
            )?;
        }
        equal(
            "snapshot-reserved-field",
            u64::from(matches!(
                vmx::vmread(0x8000),
                Err(vmx::VmxStatus::FailValid)
            )),
            1,
        )?;
        equal(
            "snapshot-error-12",
            read_field("snapshot-error", vmcs::VM_INSTRUCTION_ERROR)?,
            12,
        )?;
        let readonly_reject = cpu::rdmsr(vmx::IA32_VMX_MISC) & (1 << 29) == 0;
        let written = vmx::vmwrite(vmcs::VM_EXIT_REASON, 10);
        if readonly_reject {
            equal(
                "snapshot-readonly",
                u64::from(written == vmx::VmxStatus::FailValid),
                1,
            )?;
            equal(
                "snapshot-error-13",
                read_field("snapshot-error", vmcs::VM_INSTRUCTION_ERROR)?,
                13,
            )?;
        } else {
            // Reference hardware may advertise writable exit fields. Direct
            // must not: its transcript gate requires the rejected-write branch.
            success("snapshot-writable", written)?;
            equal(
                "snapshot-write-visible",
                read_field("snapshot-reason", vmcs::VM_EXIT_REASON)?,
                10,
            )?;
            equal(
                "snapshot-error-retained",
                read_field("snapshot-error", vmcs::VM_INSTRUCTION_ERROR)?,
                12,
            )?;
            write(vmcs::VM_EXIT_REASON, 18)?;
        }
        equal(
            "snapshot-readonly-unchanged",
            read_field("snapshot-reason", vmcs::VM_EXIT_REASON)?,
            18,
        )?;
        let cs = read_field("snapshot-host-cs", vmcs::HOST_CS_SELECTOR)?;
        write(vmcs::HOST_CS_SELECTOR, 0)?;
        equal("snapshot-failed-entry", enter(&mut frame, 1), 0x40)?;
        equal(
            "snapshot-error-8",
            read_field("snapshot-error", vmcs::VM_INSTRUCTION_ERROR)?,
            8,
        )?;
        equal(
            "snapshot-failed-entry-reason",
            read_field("snapshot-reason", vmcs::VM_EXIT_REASON)?,
            18,
        )?;
        for (field, original) in guest_fields {
            equal(
                "snapshot-guest-failed-entry",
                read_field("snapshot-guest", field)?,
                original,
            )?;
        }
        write(vmcs::HOST_CS_SELECTOR, cs)?;

        // A second owned page proves the snapshot cannot follow another VMCS.
        // It is disjoint from all live EPT, payload, stack and descriptor pages.
        let other_address = base + (SNAPSHOT_VMCS_PAGE * PAGE) as u64;
        let other = VmcsPhys::new(other_address).ok_or(failure("snapshot-vmcs-address"))?;
        (other_address as *mut u32)
            .write_volatile(cpu::rdmsr(vmx::IA32_VMX_BASIC) as u32 & 0x7fff_ffff);
        success("snapshot-other-clear", vmx::vmclear(other))?;
        success("snapshot-other-current", vmx::vmptrld(other))?;
        configure(base, env, entry, exit)?;
        for (field, value) in [
            (vmcs::GUEST_IA32_PAT, original_pat),
            (vmcs::HOST_IA32_PAT, original_pat),
            (vmcs::GUEST_IA32_EFER, original_efer),
            (vmcs::HOST_IA32_EFER, original_efer),
            (vmcs::GUEST_RIP, snapshot_guest as *const () as usize as u64),
        ] {
            write(field, value)?;
        }
        equal("snapshot-other-entry", enter(&mut frame, 0), 0)?;
        equal(
            "snapshot-other-exit",
            read_field("snapshot-reason", vmcs::VM_EXIT_REASON)?,
            10,
        )?;
        success("snapshot-original-current", vmx::vmptrld(region))?;
        equal(
            "snapshot-original-exit",
            read_field("snapshot-reason", vmcs::VM_EXIT_REASON)?,
            18,
        )?;
        success("snapshot-other-reload", vmx::vmptrld(other))?;
        equal(
            "snapshot-other-retained",
            read_field("snapshot-reason", vmcs::VM_EXIT_REASON)?,
            10,
        )?;
        let other_rip = snapshot_guest as *const () as usize as u64;
        equal(
            "snapshot-other-rip",
            read_field("snapshot-rip", vmcs::GUEST_RIP)?,
            other_rip,
        )?;
        write(vmcs::GUEST_RIP, other_rip + 2)?; // Skip CPUID; execute NOP then VMCALL.
        equal(
            "snapshot-other-write",
            read_field("snapshot-rip", vmcs::GUEST_RIP)?,
            other_rip + 2,
        )?;
        equal("snapshot-other-resume", enter(&mut frame, 1), 0)?;
        equal(
            "snapshot-other-new-exit",
            read_field("snapshot-reason", vmcs::VM_EXIT_REASON)?,
            18,
        )?;
        equal(
            "snapshot-other-new-rip",
            read_field("snapshot-rip", vmcs::GUEST_RIP)?,
            other_rip + 3,
        )?;
        success("snapshot-other-final-clear", vmx::vmclear(other))?;
        success("snapshot-final-current", vmx::vmptrld(region))?;
        equal(
            "snapshot-after-clear",
            read_field("snapshot-reason", vmcs::VM_EXIT_REASON)?,
            18,
        )?;
        let _ = writeln!(
            serial,
            "thin-hv: MSR exit snapshot PASS warm=8 gpa_high=24 access_errors=2 readonly_reject={} switches=4 clear=1 guest_fields=4 guest_writes=17 guest_reject=4 guest_resume=1 guest_repeat=8 guest_operand_faults=8",
            u8::from(readonly_reject)
        );
        Ok(())
    }
}

/// A malformed exit item must abort this virtual CPU, after earlier stores.
/// The runner reads only the abort indicator and one PAT value through QEMU's
/// physical-memory monitor; any return to this fixture is an explicit failure.
unsafe fn abort_list(
    base: u64,
    region: VmcsPhys,
    env: &HostEnvironment<'_>,
    serial: &mut Serial,
) -> Result<()> {
    let code = if cfg!(feature = "msr-abort-store") {
        1
    } else {
        4
    };
    // SAFETY: owned low WB pages and the VMX CPU prerequisites were checked;
    // no hardware references the lists during preparation. On an unexpected
    // return, enter restores the original MSRs/FP before Rust diagnoses it.
    unsafe {
        let original_pat = cpu::rdmsr(PAT);
        let original_efer = cpu::rdmsr(EFER);
        let l2_pat = (original_pat & !(0xff << 32)) | (5 << 32);
        let store = base + (LIST_PAGE * PAGE) as u64;
        let load = store + 64;
        let mut frame = Frame {
            fx: [0; 512],
            original_pat,
            original_efer,
            inherited_pat: original_pat,
            inherited_efer: original_efer,
            l2_pat,
            l2_efer: original_efer,
            entry_pat: 0,
            entry_efer: 0,
            exit_pat: 0,
            exit_efer: 0,
            l2_debugctl: 0,
        };
        for address in [store, load] {
            let slots = address as *mut u64;
            slots.write(u64::from(PAT));
            slots.add(1).write(original_pat);
            slots.add(2).write(u64::from(u32::MAX));
            slots.add(3).write(0);
        }
        success("abort-list-clear", vmx::vmclear(region))?;
        success("abort-list-current", vmx::vmptrld(region))?;
        let entry = controls(
            vmx::IA32_VMX_ENTRY_CTLS,
            vmx::IA32_VMX_TRUE_ENTRY_CTLS,
            vmcs::VM_ENTRY_IA32E_MODE
                | vmcs::VM_ENTRY_LOAD_IA32_PAT
                | vmcs::VM_ENTRY_LOAD_IA32_EFER,
        )?;
        let exit = controls(
            vmx::IA32_VMX_EXIT_CTLS,
            vmx::IA32_VMX_TRUE_EXIT_CTLS,
            vmcs::VM_EXIT_HOST_ADDRESS_SPACE_SIZE
                | vmcs::VM_EXIT_LOAD_IA32_PAT
                | vmcs::VM_EXIT_LOAD_IA32_EFER,
        )?;
        configure(base, env, entry, exit)?;
        for (field, value) in [
            (vmcs::GUEST_IA32_PAT, original_pat),
            (vmcs::GUEST_IA32_EFER, original_efer),
            (vmcs::HOST_IA32_PAT, original_pat),
            (vmcs::HOST_IA32_EFER, original_efer),
            (vmcs::VM_EXIT_MSR_STORE_ADDR, store),
            (vmcs::VM_EXIT_MSR_STORE_COUNT, if code == 1 { 2 } else { 1 }),
            (vmcs::VM_EXIT_MSR_LOAD_ADDR, load),
            (vmcs::VM_EXIT_MSR_LOAD_COUNT, if code == 4 { 2 } else { 0 }),
        ] {
            write(field, value)?;
        }
        let _ = writeln!(
            serial,
            "thin-hv: MSR abort armed code={code} vmcs={:#018x} store={:#018x} value={:#018x}",
            region.get(),
            store + 8,
            l2_pat
        );
        let flags = enter(&mut frame, 0);
        Err(Failure {
            stage: "abort-list-returned",
            actual: flags,
            expected: u64::MAX,
        })
    }
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
        let mut vmx_active = true;
        // SAFETY: this CPU now owns VMX operation and its private VMCS/pages.
        let result = unsafe {
            if cfg!(any(feature = "msr-abort-store", feature = "msr-abort-load")) {
                abort_list(base, region, &env, serial)
            } else {
                matrix(base, region, &env, serial)
                    .and_then(|()| exit_lists(base, region, &env, serial))
                    .and_then(|()| control_cache(base, region, &env, serial))
                    .and_then(|()| vpid_lifetime(base, region, &env, serial, &mut vmx_active))
                    .and_then(|()| ept_large_pages(base, region, &env, serial))
                    .and_then(|()| entry_lists(base, region, &env, serial))
            }
        };
        // SAFETY: no L2 remains running; even failed attempts return here in L1
        // root. Clear the owned VMCS before VMXOFF; retain pages on any failure.
        unsafe {
            if vmx_active {
                success("msr-final-clear", vmx::vmclear(region))?;
                success("msr-vmxoff", vmx::vmxoff())?;
            }
            cpu::write_cr4(caps.cr4);
        }
        result
    })();
    match result {
        Ok(()) => {
            let _ = writeln!(
                serial,
                "thin-hv: MSR contract PASS matrix=128 exit_cases=12 exit_resume=1 entry_cases=20 entry_load=7 entry_resume=1 entry_fail=10 early_fail=2 guest_fail=2 final_vmxoff=1"
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

// SAFETY: the retained fixture owns this mapped executable L2 entry. Its
// current VMCS has valid long-mode state and CPUID exits unconditionally.
// No stack/memory operand or ABI call executes; RDI (the Frame) stays intact.
#[unsafe(naked)]
unsafe extern "sysv64" fn snapshot_guest() -> ! {
    core::arch::naked_asm!("cpuid", "nop", "vmcall", "ud2");
}

// SAFETY: matrix owns the clear current VMCS and aligned Frame, with CPL0,
// CR0.EM/TS=0 and OSFXSR=1. Only guest below runs, preserving RDI. The saved
// SysV callee registers and FP image are restored on every immediate failure
// or nested exit; no Rust executes before restoring original PAT/EFER.
// Resume mode 2 restores the controlled EPT guest's RAX/RDX operands from its
// live Frame after the entry wrapper's MSR writes, preserving the faulting RIP.
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
        "cmp rsi, 2",
        "jne 5f",
        "mov rax, [rdi + 592]",
        "mov rdx, [rdi + 544]",
        "5:",
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
        "mov ecx, 0x1d9",
        "rdmsr",
        "shl rdx, 32",
        "or rax, rdx",
        "mov [rdi + 592], rax",
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

// SAFETY: only vpid_lifetime enters with CPL0, IF=0 and RDI pointing at its
// retained Frame. Slot 592 names a present owned leaf in the private L2 root;
// slot 560 is the writable result. No stack or MSR state is changed.
#[unsafe(naked)]
unsafe extern "sysv64" fn vpid_guest() -> ! {
    core::arch::naked_asm!(
        "mov rax, [rdi + 592]",
        "mov rax, [rax]",
        "mov [rdi + 560], rax",
        "vmcall",
        "ud2",
    );
}

// SAFETY: ept_large_pages supplies a retained Frame at RDI, CPL0 and IF=0.
// Slot 592 names its controlled L2 leaf; the value at 544 is test data, not
// an MSR value here. A denied write exits before changing memory; recovery
// resumes that exact instruction. VMCALL is the only successful continuation.
#[unsafe(naked)]
unsafe extern "sysv64" fn ept_store_guest() -> ! {
    core::arch::naked_asm!(
        "mov rax, [rdi + 592]",
        "mov rdx, [rdi + 544]",
        "mov [rax], rdx",
        "vmcall",
        "ud2",
    );
}
