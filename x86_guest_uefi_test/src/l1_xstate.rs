//! Direct L1 XSAVE/CPUID contract, also run unchanged on the reference backend.
//! The temporary IDT catches only the test's exact XSETBV RIP. Firmware tables,
//! CR4, XCR0 and interrupt enablement are restored before returning any result.

use super::Result;
use super::equal;
use core::arch::asm;
use core::ptr;
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::Ordering;
use x86_64_hal::cpu;
use x86_64_hal::xstate;

#[repr(C)]
struct FaultRecord {
    instruction: u64,
    continuation: u64,
    vector: u64,
    error: u64,
}

/// This disposable fixture has one L1 CPU. Only the synchronous exception entry
/// reads this pointer, between publication and completion of one assembly probe.
static ACTIVE_FAULT: AtomicPtr<FaultRecord> = AtomicPtr::new(ptr::null_mut());

#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

/// Hardware #UD has no error code; normalize it to the #GP stack layout.
#[unsafe(naked)]
extern "C" fn invalid_opcode() -> ! {
    core::arch::naked_asm!("push 0", "push 6", "jmp {}", sym fault_entry);
}

/// Hardware #GP already pushed its error code.
#[unsafe(naked)]
extern "C" fn general_protection() -> ! {
    core::arch::naked_asm!("push 13", "jmp {}", sym fault_entry);
}

/// Runs with IF clear, preserving all interrupted GPRs and XSTATE. The published
/// stack record is live and unique until IRET returns to the probe. Unexpected
/// RIPs never receive a synthetic continuation; the runner reports a timeout.
#[unsafe(naked)]
extern "C" fn fault_entry() -> ! {
    core::arch::naked_asm!(
        "push rax", "push rcx",
        "mov rax, [rip + {active}]",
        "test rax, rax", "jz 3f",
        "mov rcx, [rsp + 32]", "cmp rcx, [rax]", "jne 3f",
        "mov rcx, [rax + 8]", "mov [rsp + 32], rcx",
        "mov rcx, [rsp + 16]", "mov [rax + 16], rcx",
        "mov rcx, [rsp + 24]", "mov [rax + 24], rcx",
        "pop rcx", "pop rax", "add rsp, 16", "iretq",
        "3:", "cli", "hlt", "jmp 3b",
        active = sym ACTIVE_FAULT,
    );
}

fn gate(entry: u64, selector: u16) -> u128 {
    u128::from(entry & 0xffff)
        | (u128::from(selector) << 16)
        | (0x8e_u128 << 40)
        | (u128::from((entry >> 16) & 0xffff) << 48)
        | (u128::from(entry >> 32) << 64)
}

/// Does not call Rust while the faulting RIP is live. ECX and EDX:EAX deliberately
/// retain high input bits in separate tests of architectural operand truncation.
fn probe(index: u64, value: u64, high_bits: u64) -> (u64, u64) {
    let mut record = FaultRecord {
        instruction: 0,
        continuation: 0,
        vector: u64::MAX,
        error: u64::MAX,
    };
    ACTIVE_FAULT.store(&raw mut record, Ordering::Release);
    // SAFETY: the single-CPU fixture installed live #GP/#UD gates, disabled IF,
    // and published this stack record. The ISR accepts only label 2's RIP and
    // returns to label 3 without altering input GPRs, XSTATE or any other RIP.
    unsafe {
        asm!(
            "lea r8, [rip + 2f]", "mov [r10], r8",
            "lea r8, [rip + 3f]", "mov [r10 + 8], r8",
            "2:", "xsetbv", "3:",
            in("r10") &raw mut record,
            in("rcx") index,
            in("rax") u64::from(value as u32) | high_bits,
            in("rdx") (value >> 32) | high_bits,
            out("r8") _,
            options(nostack),
        );
    }
    ACTIVE_FAULT.store(ptr::null_mut(), Ordering::Release);
    (record.vector, record.error)
}

fn enabled_area_size(xcr0: u64) -> Result<()> {
    let mut size = 576_u32;
    for bit in 2..64 {
        if xcr0 & (1 << bit) != 0 {
            let component = cpu::cpuid(0xd, bit);
            equal("l1-xstate-user-component", u64::from(component.ecx & 1), 0)?;
            size = size.max(
                component
                    .ebx
                    .checked_add(component.eax)
                    .ok_or(super::Failure {
                        stage: "l1-xstate-area-overflow",
                        actual: u64::from(component.ebx),
                        expected: 0,
                    })?,
            );
        }
    }
    equal(
        "l1-xstate-leaf-d-size",
        u64::from(cpu::cpuid(0xd, 0).ebx),
        u64::from(size),
    )
}

/// Runs before VMXON in L1, so CPUID/XSETBV exercise the project L0 interception,
/// not KVM's L2 emulation. No asynchronous handler calls firmware or uses SIMD.
pub(super) fn run() -> Result<()> {
    let leaf1 = cpu::cpuid(1, 0);
    equal(
        "l1-xstate-available",
        u64::from(leaf1.ecx & xstate::CPUID_XSAVE != 0),
        1,
    )?;
    let leafd = cpu::cpuid(0xd, 0);
    let supported = (u64::from(leafd.edx) << 32) | u64::from(leafd.eax);
    equal("l1-xstate-legacy", supported & 3, 3)?;
    let reserved = (!supported).trailing_zeros();
    equal("l1-xstate-reserved-bit", u64::from(reserved < 64), 1)?;
    let original_idt = cpu::sidt();
    equal(
        "l1-xstate-idt-range",
        u64::from((223..=4095).contains(&original_idt.limit)),
        1,
    )?;
    equal("l1-xstate-idt-base", u64::from(original_idt.base != 0), 1)?;
    let mut idt = [0_u128; 256];
    // SAFETY: UEFI's current IDTR names a live table; the checked limit fits
    // this aligned local copy, which remains live until the original is restored.
    unsafe {
        ptr::copy_nonoverlapping(
            original_idt.base as *const u8,
            idt.as_mut_ptr().cast(),
            usize::from(original_idt.limit) + 1,
        )
    };
    idt[6] = gate(invalid_opcode as usize as u64, cpu::read_cs());
    idt[13] = gate(general_protection as usize as u64, cpu::read_cs());
    let replacement = Idtr {
        limit: original_idt.limit,
        base: idt.as_ptr() as u64,
    };
    let original = Idtr {
        limit: original_idt.limit,
        base: original_idt.base,
    };
    let cr4 = cpu::read_cr4();
    let flags: u64;
    // SAFETY: CPL0 fixture owns this CPU; keep maskable interrupts disabled while
    // changing CR4/IDTR. All original IDT entries except #GP/#UD are preserved.
    unsafe {
        asm!("pushfq", "pop {}", "cli", out(reg) flags);
    }
    // SAFETY: CPUID.XSAVE was checked. Only OSXSAVE is enabled, permitting XGETBV;
    // no state component is changed before recording the original XCR0.
    unsafe { cpu::write_cr4(cr4 | xstate::CR4_OSXSAVE) };
    // SAFETY: XSAVE exists and this CPU's CR4.OSXSAVE is now set.
    let xcr0 = unsafe { cpu::xgetbv(0) };
    // SAFETY: replacement is a packed 10-byte IDTR, naming the live local table
    // with the original CS selector and valid 64-bit interrupt gate encodings.
    unsafe { asm!("lidt [{}]", in(reg) &replacement, options(readonly, nostack, preserves_flags)) };
    let result = (|| {
        for enabled in [false, true, false, true] {
            let visible =
                (cr4 & !xstate::CR4_OSXSAVE) | if enabled { xstate::CR4_OSXSAVE } else { 0 };
            // SAFETY: both values differ from valid original CR4 only by OSXSAVE.
            unsafe { cpu::write_cr4(visible) };
            let actual = cpu::cpuid(1, 0);
            equal(
                "l1-cpuid-osxsave",
                u64::from(actual.ecx & xstate::CPUID_OSXSAVE != 0),
                u64::from(enabled),
            )?;
            equal(
                "l1-cpuid-static",
                u64::from(actual.ecx & !xstate::CPUID_OSXSAVE),
                u64::from(leaf1.ecx & !xstate::CPUID_OSXSAVE),
            )?;
            equal(
                "l1-cpuid-leaf-d-cr4-independent",
                u64::from(cpu::cpuid(0xd, 0).ebx),
                u64::from(leafd.ebx),
            )?;
        }
        for value in [1, 3, xcr0] {
            equal("l1-xsetbv-valid", probe(0, value, 0).0, u64::MAX)?;
            // SAFETY: OSXSAVE remains enabled after the toggle loop.
            equal("l1-xsetbv-value", unsafe { cpu::xgetbv(0) }, value)?;
            enabled_area_size(value)?;
        }
        equal(
            "l1-xsetbv-high-operands",
            probe(1 << 32, xcr0, 0xabcd_1234_0000_0000).0,
            u64::MAX,
        )?;
        for (index, value) in [(1, xcr0), (0, 0), (0, 5), (0, xcr0 | (1 << reserved))] {
            let (vector, error) = probe(index, value, 0);
            equal("l1-xsetbv-gp-vector", vector, 13)?;
            equal("l1-xsetbv-gp-error", error, 0)?;
            // SAFETY: OSXSAVE is enabled; the fault must not have modified XCR0.
            equal("l1-xsetbv-fault-preserves", unsafe { cpu::xgetbv(0) }, xcr0)?;
        }
        // SAFETY: clearing only OSXSAVE is valid; the private fixture #UD gate
        // must observe the unchanged XSETBV RIP, then return to its continuation.
        unsafe { cpu::write_cr4(cr4 & !xstate::CR4_OSXSAVE) };
        let fault = probe(1, 0, 0);
        // SAFETY: re-enable the already established XSAVE facility for XGETBV.
        unsafe { cpu::write_cr4(cr4 | xstate::CR4_OSXSAVE) };
        equal("l1-xsetbv-ud-priority", fault.0, 6)?;
        // SAFETY: OSXSAVE is enabled; even the #UD case must preserve XCR0.
        equal("l1-xsetbv-ud-preserves", unsafe { cpu::xgetbv(0) }, xcr0)?;
        Ok(())
    })();
    // SAFETY: every fallible test above returns through this cleanup. Restore
    // the captured valid XCR0 with OSXSAVE enabled, then the original CR4/IDTR;
    // neither the local IDT nor the exception record is referenced afterward.
    unsafe {
        cpu::write_cr4(cr4 | xstate::CR4_OSXSAVE);
        cpu::xsetbv(0, xcr0);
        cpu::write_cr4(cr4);
        asm!("lidt [{}]", in(reg) &original, options(readonly, nostack, preserves_flags));
        if flags & (1 << 9) != 0 {
            asm!("sti", options(nomem, nostack));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exception_assembly_layout_and_gate_are_exact() {
        assert_eq!(core::mem::size_of::<Idtr>(), 10);
        assert_eq!(core::mem::offset_of!(FaultRecord, instruction), 0);
        assert_eq!(core::mem::offset_of!(FaultRecord, continuation), 8);
        assert_eq!(core::mem::offset_of!(FaultRecord, vector), 16);
        assert_eq!(core::mem::offset_of!(FaultRecord, error), 24);
        let bytes = gate(0x1234_5678_9abc_def0, 8).to_le_bytes();
        assert_eq!(
            bytes,
            [
                0xf0, 0xde, 8, 0, 0, 0x8e, 0xbc, 0x9a, 0x78, 0x56, 0x34, 0x12, 0, 0, 0, 0
            ]
        );
    }
}
