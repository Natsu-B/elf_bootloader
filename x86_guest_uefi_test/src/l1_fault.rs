//! Scoped synchronous L1 fault fixture shared by XSTATE and VMX operand probes.

use super::Result;
use super::equal;
use core::arch::asm;
use core::ptr;
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::Ordering;
use x86_64_hal::cpu;

#[repr(C)]
pub(super) struct FaultRecord {
    pub instruction: u64,
    pub continuation: u64,
    pub vector: u64,
    pub error: u64,
    pub cr2: u64,
}

/// One disposable L1 CPU, with IF clear. Only the synchronous exception entry
/// observes this pointer during a single exact-RIP assembly probe.
static ACTIVE_FAULT: AtomicPtr<FaultRecord> = AtomicPtr::new(ptr::null_mut());

#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

#[unsafe(naked)]
extern "C" fn invalid_opcode() -> ! {
    core::arch::naked_asm!("push 0", "push 6", "jmp {}", sym fault_entry);
}

#[unsafe(naked)]
extern "C" fn stack_fault() -> ! {
    core::arch::naked_asm!("push 12", "jmp {}", sym fault_entry);
}

#[unsafe(naked)]
extern "C" fn general_protection() -> ! {
    core::arch::naked_asm!("push 13", "jmp {}", sym fault_entry);
}

#[unsafe(naked)]
extern "C" fn page_fault() -> ! {
    core::arch::naked_asm!("push 14", "jmp {}", sym fault_entry);
}

/// Preserves GPRs and XSTATE. Only the published instruction RIP can recover;
/// unexpected exceptions fail closed, never skip unrelated instructions.
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
        "mov rcx, cr2", "mov [rax + 32], rcx",
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

/// The operation must publish its exact instruction/continuation labels into
/// the record in assembly, then execute that instruction without calling Rust.
pub(super) fn probe(operation: impl FnOnce(*mut FaultRecord)) -> FaultRecord {
    let mut record = FaultRecord {
        instruction: 0,
        continuation: 0,
        vector: u64::MAX,
        error: u64::MAX,
        cr2: 0,
    };
    ACTIVE_FAULT.store(&raw mut record, Ordering::Release);
    operation(&raw mut record);
    ACTIVE_FAULT.store(ptr::null_mut(), Ordering::Release);
    record
}

/// Keeps a copied IDT live until all probes finish, restoring IDTR, CR2 and IF
/// even when the operation returns an assertion failure. No firmware writes.
pub(super) fn with_handler<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let original_idt = cpu::sidt();
    equal(
        "l1-fault-idt-range",
        u64::from(original_idt.limit >= 239),
        1,
    )?;
    equal("l1-fault-idt-base", u64::from(original_idt.base != 0), 1)?;
    let mut idt = [0_u128; 256];
    // VM exit sets IDTR.limit to 0xffff even for a 4 KiB IDT. Only vectors
    // 0..255 are addressable: preserve all possible gates without reading the
    // unrelated 60 KiB beyond them. Restore the exact original limit below.
    let copied_limit = original_idt.limit.min(4095);
    // SAFETY: the live IDT covers these at most 256 gates; the bounded copy
    // fits idt and stays live until IDTR is restored on every ordinary return.
    unsafe {
        ptr::copy_nonoverlapping(
            original_idt.base as *const u8,
            idt.as_mut_ptr().cast(),
            usize::from(copied_limit) + 1,
        )
    };
    for (vector, entry) in [
        (6, invalid_opcode as usize),
        (12, stack_fault as usize),
        (13, general_protection as usize),
        (14, page_fault as usize),
    ] {
        idt[vector] = gate(entry as u64, cpu::read_cs());
    }
    let replacement = Idtr {
        limit: copied_limit,
        base: idt.as_ptr() as u64,
    };
    let original = Idtr {
        limit: original_idt.limit,
        base: original_idt.base,
    };
    let flags: u64;
    let cr2: u64;
    // SAFETY: CPL0 single-CPU fixture owns its temporary IDT and disables IF
    // before publishing it. All unrelated firmware gates remain unchanged.
    unsafe {
        asm!("pushfq", "pop {flags}", "cli", "mov {cr2}, cr2", "lidt [{table}]",
            flags = out(reg) flags, cr2 = out(reg) cr2, table = in(reg) &replacement);
    }
    let result = operation();
    // SAFETY: all fallible probes return here. The original live firmware IDT
    // and CR2 are restored before freeing the copy or re-enabling interrupts.
    unsafe {
        asm!("lidt [{table}]", "mov cr2, {cr2}", table = in(reg) &original, cr2 = in(reg) cr2,
            options(nostack, preserves_flags));
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
        assert_eq!(core::mem::offset_of!(FaultRecord, cr2), 32);
        assert_eq!(
            gate(0x1234_5678_9abc_def0, 8).to_le_bytes(),
            [
                0xf0, 0xde, 8, 0, 0, 0x8e, 0xbc, 0x9a, 0x78, 0x56, 0x34, 0x12, 0, 0, 0, 0
            ]
        );
    }
}
