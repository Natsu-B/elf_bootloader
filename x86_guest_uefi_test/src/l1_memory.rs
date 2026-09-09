//! Native VMX operand faults on private L1 paging structures, before/after VMXON.

use super::FAIL_INVALID;
use super::FAIL_VALID;
use super::Failure;
use super::PAGE;
use super::Result;
use super::Serial;
use super::UNSUPPORTED_FIELD;
use super::equal;
use super::field_equal;
use super::l1_fault;
use core::arch::asm;
use core::fmt::Write;
use core::ptr;
use x86_64_hal::cpu;
use x86_64_hal::vmcs;

/// Five paging levels plus two discontiguous payload pages; four-level mode
/// leaves page four unused. All pages share the fixture's checked WB allocation.
pub(super) const PAGES: usize = 7;

#[derive(Clone, Copy)]
enum Instruction {
    Vmxon,
    Vmclear,
    Vmptrld,
    Vmptrst,
    Vmread,
    Vmwrite,
    Invept,
    Invvpid,
    StackStore,
}

fn probe(instruction: Instruction, address: u64, operand: u64) -> (l1_fault::FaultRecord, u64) {
    let mut flags: u64 = 0;
    let record = l1_fault::probe(|record| {
        macro_rules! execute {
            ($instruction:literal) => {
                // SAFETY: the scoped fixture installed exact-RIP #PF/#GP/#SS
                // recovery with IF clear. RBP is saved on the valid fixture
                // stack, even when it supplies a deliberately invalid SS
                // operand. Every continuation captures flags then restores RBP.
                unsafe {
                    asm!(
                        "push rbp", "mov rbp, r11",
                        "lea r8, [rip + 2f]", "mov [r10], r8",
                        "lea r8, [rip + 3f]", "mov [r10 + 8], r8",
                        "2:", $instruction, "3:",
                        "pushfq", "pop rax", "pop rbp",
                        in("r10") record, in("r11") address, in("r9") operand,
                        out("r8") _, lateout("rax") flags,
                    );
                }
            };
        }
        match instruction {
            Instruction::Vmxon => execute!("vmxon [r11]"),
            Instruction::Vmclear => execute!("vmclear [r11]"),
            Instruction::Vmptrld => execute!("vmptrld [r11]"),
            Instruction::Vmptrst => execute!("vmptrst [r11]"),
            Instruction::Vmread => execute!("vmread [r11], r9"),
            Instruction::Vmwrite => execute!("vmwrite r9, [r11]"),
            Instruction::Invept => execute!("invept r9, [r11]"),
            Instruction::Invvpid => execute!("invvpid r9, [r11]"),
            Instruction::StackStore => execute!("vmptrst [rbp]"),
        }
    });
    (record, (flags & 1) | ((flags >> 5) & 2))
}

fn fault(
    instruction: Instruction,
    address: u64,
    operand: u64,
    vector: u64,
    error: u64,
    cr2: u64,
) -> Result<()> {
    let (record, _) = probe(instruction, address, operand);
    equal(
        "l1-operand-fault-instruction",
        u64::from(record.instruction != 0),
        1,
    )?;
    equal("l1-operand-fault-vector", record.vector, vector)?;
    equal("l1-operand-fault-error", record.error, error)?;
    if vector == 14 {
        equal("l1-operand-fault-cr2", record.cr2, cr2)?;
    }
    Ok(())
}

fn succeeds(instruction: Instruction, address: u64, operand: u64) -> Result<()> {
    let (record, flags) = probe(instruction, address, operand);
    equal("l1-operand-success-vector", record.vector, u64::MAX)?;
    equal("l1-operand-success-flags", flags, 0)
}

/// A copied top-level table preserves every original firmware mapping. Only a
/// previously absent lower-half slot gets new subordinate tables and test pages.
/// CR3 and CR0 are restored before any error returns to VMX/UEFI cleanup.
fn with_pages<T>(
    base: u64,
    operation: impl FnOnce(u64, *mut u64, u64, u64) -> Result<T>,
) -> Result<T> {
    l1_fault::with_handler(|| {
        let original_cr3 = cpu::read_cr3();
        let cr0 = cpu::read_cr0();
        let five = cpu::read_cr4() & (1 << 12) != 0;
        let levels = if five { 5 } else { 4 };
        let top = base as *mut u64;
        // SAFETY: caller supplies seven exclusive low WB pages, checked by the
        // enclosing UEFI allocation_map. Firmware's current top-level CR3 is
        // identity mapped and remains live; no Boot Services exit occurs here.
        unsafe {
            ptr::write_bytes(base as *mut u8, 0, PAGES * PAGE);
            ptr::copy_nonoverlapping(
                (original_cr3 & 0x000f_ffff_ffff_f000) as *const u64,
                top,
                512,
            );
        }
        let slot = (1..256)
            .find(|&index| {
                // SAFETY: index is within the exclusive copied 512-entry root.
                unsafe { ptr::read_volatile(top.add(index)) & 1 == 0 }
            })
            .ok_or(Failure {
                stage: "l1-operand-free-root-slot",
                actual: 0,
                expected: 1,
            })?;
        let shift = if five { 48 } else { 39 };
        let virtual_base = (slot as u64) << shift;
        let pt = (base + ((levels - 1) * PAGE) as u64) as *mut u64;
        // Reverse the physical payload order so crossing requires two walks.
        let data = base + (6 * PAGE) as u64;
        // SAFETY: all links name distinct aligned pages within this allocation;
        // entries are supervisor RW WB RAM. No existing root slot is replaced.
        unsafe {
            ptr::write_volatile(top.add(slot), (base + PAGE as u64) | 3);
            for level in 1..levels - 1 {
                ptr::write_volatile(
                    (base + (level * PAGE) as u64) as *mut u64,
                    (base + ((level + 1) * PAGE) as u64) | 3,
                );
            }
            ptr::write_volatile(pt, data | 3);
            ptr::write_volatile(pt.add(1), (data - PAGE as u64) | 3);
            cpu::write_cr0(cr0 | (1 << 16));
            asm!("mov cr3, {}", in(reg) base | (original_cr3 & (0xfff | (3 << 61))), options(nostack, preserves_flags));
        }
        let result = operation(virtual_base, pt, data, if five { 1 << 57 } else { 1 << 48 });
        // SAFETY: the original root remained unchanged and live. No-flush bit
        // 63 is clear on both CR3 loads; restore before releasing private pages.
        unsafe {
            asm!("mov cr3, {}", in(reg) original_cr3, options(nostack, preserves_flags));
            cpu::write_cr0(cr0);
        }
        result
    })
}

/// One #PF and one #GP before VMXON; neither may create a VMX session.
pub(super) fn before_vmxon(base: u64) -> Result<()> {
    with_pages(base, |linear, _, _, noncanonical| {
        let absent = linear + (2 * PAGE) as u64;
        fault(Instruction::Vmxon, absent, 0, 14, 0, absent)?;
        fault(Instruction::Vmxon, noncanonical, 0, 13, 0, 0)
    })
}

/// VMCS validity is checked before VMREAD/VMWRITE access their memory operand;
/// an unsupported invalidation type also fails without reading its descriptor.
pub(super) fn without_current(base: u64) -> Result<()> {
    with_pages(base, |linear, _, _, _| {
        let absent = linear + (2 * PAGE) as u64;
        for (instruction, operand) in [
            (Instruction::Vmread, UNSUPPORTED_FIELD),
            (Instruction::Vmwrite, UNSUPPORTED_FIELD),
            (Instruction::Invept, 0),
            (Instruction::Invvpid, 4),
        ] {
            let (record, flags) = probe(instruction, absent, operand);
            equal("l1-operand-no-current-vector", record.vector, u64::MAX)?;
            equal("l1-operand-no-current-flags", flags, FAIL_INVALID)?;
        }
        let cross = linear + PAGE as u64 - 4;
        succeeds(Instruction::Vmptrst, cross, 0)?;
        // SAFETY: both discontiguous payload leaves are present and owned;
        // the unaligned read checks the complete VMPTRST destination via L1 MMU.
        equal(
            "l1-operand-no-current-pointer",
            unsafe { ptr::read_unaligned(cross as *const u64) },
            u64::MAX,
        )
    })
}

/// All checks run with a valid current VMCS. The original RIP value and current
/// pointer are retained; failures still return through paging and VMX cleanup.
pub(super) fn in_vmx(base: u64, serial: &mut Serial) -> Result<u64> {
    with_pages(base, |linear, pt, data, noncanonical| {
        let absent = linear + (2 * PAGE) as u64;
        let field = u64::from(vmcs::GUEST_RIP);
        for (instruction, operand, write) in [
            (Instruction::Vmclear, 0, false),
            (Instruction::Vmptrld, 0, false),
            (Instruction::Vmptrst, 0, true),
            (Instruction::Vmread, field, true),
            (Instruction::Vmwrite, field, false),
            (Instruction::Invept, 2, false),
            (Instruction::Invvpid, 2, false),
        ] {
            fault(
                instruction,
                absent,
                operand,
                14,
                if write { 2 } else { 0 },
                absent,
            )?;
            fault(instruction, noncanonical, operand, 13, 0, 0)?;
        }
        fault(Instruction::StackStore, noncanonical, 0, 12, 0, 0)?;
        for (instruction, operand, error) in [
            (Instruction::Vmread, UNSUPPORTED_FIELD, 12),
            (Instruction::Invept, 0, 28),
            (Instruction::Invvpid, 4, 28),
            (Instruction::Vmxon, 0, 15),
        ] {
            let (record, flags) = probe(instruction, absent, operand);
            equal("l1-operand-vmfail-priority-vector", record.vector, u64::MAX)?;
            equal("l1-operand-vmfail-priority-flags", flags, FAIL_VALID)?;
            // SAFETY: a valid current test VMCS remains selected; VMfail did not
            // clear it, and reading its instruction error has no side effects.
            unsafe {
                field_equal(
                    "l1-operand-vmfail-priority-error",
                    vmcs::VM_INSTRUCTION_ERROR,
                    error,
                )?;
            }
        }
        fault(
            Instruction::Vmwrite,
            absent,
            UNSUPPORTED_FIELD,
            14,
            0,
            absent,
        )?;
        // SAFETY: owned PT and payload are identity mapped; invalidate only the
        // new alias before testing supervisor write protection (CR0.WP is set).
        unsafe {
            ptr::write_volatile(pt, data | 1);
            asm!("invlpg [{}]", in(reg) linear, options(nostack, preserves_flags));
        }
        fault(Instruction::Vmptrst, linear, 0, 14, 3, linear)?;
        fault(Instruction::Vmread, linear, field, 14, 3, linear)?;
        let cross = linear + PAGE as u64 - 4;
        // SAFETY: the test owns both entries. Make page one absent; page zero's
        // tail is a sentinel, and no fixture code/stack uses either test alias.
        unsafe {
            ptr::write_volatile(pt, data | 3);
            ptr::write_volatile(pt.add(1), 0);
            ptr::write_volatile((data + PAGE as u64 - 4) as *mut u32, 0xa5a5_a5a5);
            asm!("invlpg [{a}]", "invlpg [{b}]", a = in(reg) linear, b = in(reg) linear + PAGE as u64, options(nostack, preserves_flags));
        }
        let mut partial_stores = 0_u64;
        for (instruction, operand, write) in [
            (Instruction::Vmptrst, 0, true),
            (Instruction::Vmread, field, true),
            (Instruction::Vmwrite, field, false),
            (Instruction::Invept, 2, false),
            (Instruction::Invvpid, 2, false),
        ] {
            // SAFETY: reset the owned first-page tail before each independent
            // fault probe, so one observed partial store cannot taint the next.
            unsafe {
                ptr::write_volatile((data + PAGE as u64 - 4) as *mut u32, 0xa5a5_a5a5);
            }
            fault(
                instruction,
                cross,
                operand,
                14,
                if write { 2 } else { 0 },
                linear + PAGE as u64,
            )?;
            // SAFETY: the tail is owned WB RAM; a faulting store must not have
            // partially changed it before detecting the second page's absence.
            let tail = unsafe { ptr::read_volatile((data + PAGE as u64 - 4) as *const u32) };
            if write {
                partial_stores += u64::from(tail != 0xa5a5_a5a5);
            } else {
                equal(
                    "l1-operand-read-preserves-tail",
                    u64::from(tail),
                    0xa5a5_a5a5,
                )?;
            }
        }
        // SAFETY: restore the owned second leaf and flush its cached absence.
        unsafe {
            ptr::write_volatile(pt.add(1), (data - PAGE as u64) | 3);
            asm!("invlpg [{}]", in(reg) linear + PAGE as u64, options(nostack, preserves_flags));
        }
        succeeds(Instruction::Vmptrst, cross, 0)?;
        succeeds(Instruction::Vmptrld, cross, 0)?;
        succeeds(Instruction::Vmread, cross, field)?;
        succeeds(Instruction::Vmwrite, cross, field)?;
        // SAFETY: both payload pages are now present and owned; write a zero
        // m128 for global INVEPT (the EPTP is ignored for this advertised type).
        unsafe {
            ptr::write_bytes(cross as *mut u8, 0, 16);
        }
        succeeds(Instruction::Invept, cross, 2)?;
        // Cold fixture evidence only. Keep the non-partial-store assertion, but
        // run every remaining case before reporting a reference-backend failure.
        let _ = writeln!(
            serial,
            "thin-hv: native L1 operand coverage pf=16 gp=8 ss=1 cross=6 priority=8 partial_stores={partial_stores}"
        );
        Ok(partial_stores)
    })
}
