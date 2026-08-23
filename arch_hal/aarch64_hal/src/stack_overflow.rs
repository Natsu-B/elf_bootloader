use core::arch::naked_asm;
use core::cell::SyncUnsafeCell;

const EMERGENCY_STACK_SIZE: usize = 64 * 1024;

#[repr(C, align(16))]
struct EmergencyStack([u8; EMERGENCY_STACK_SIZE]);

// SAFETY: This emergency stack is only used when the main SP_EL2 stack overflows.
// It is mapped as Normal memory in Stage-1 and remains valid for the lifetime of the hypervisor.
static EMERGENCY_STACK: SyncUnsafeCell<EmergencyStack> =
    SyncUnsafeCell::new(EmergencyStack([0u8; EMERGENCY_STACK_SIZE]));

#[repr(C)]
struct SavedFrame {
    x0: u64,
    x1: u64,
    x2: u64,
    x3: u64,
    x4: u64,
    x5: u64,
    x6: u64,
    x7: u64,
    x8: u64,
    x9: u64,
    x10: u64,
    x11: u64,
    x12: u64,
    x13: u64,
    x14: u64,
    x15: u64,
    original_sp: u64,
    esr_el2: u64,
    far_el2: u64,
    elr_el2: u64,
    spsr_el2: u64,
}

/// Touches both ends of the emergency stack so it is ready for exception handling.
///
/// # Safety
///
/// The caller must ensure Stage-1 maps the static buffer as identity-mapped Normal memory.
pub unsafe fn init_emergency_stack() {
    let stack_base = EMERGENCY_STACK.get() as *const EmergencyStack as usize;
    let stack_top = stack_base + EMERGENCY_STACK_SIZE;
    unsafe {
        // SAFETY: The emergency stack is static, mapped, and sized for these writes.
        core::ptr::write_volatile(stack_base as *mut u8, 0);
        core::ptr::write_volatile((stack_top - 1) as *mut u8, 0);
    }
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
extern "C" fn el2_sync_current_x_handler() -> ! {
    naked_asm!(
        // Capture original SP_EL2
        "mov x16, sp",
        // Switch to emergency stack (SP_EL2)
        "adrp x17, {emergency_stack}",
        "add x17, x17, :lo12:{emergency_stack}",
        "mov sp, x17",
        "movz x17, #1, lsl #16",
        "add sp, sp, x17",
        // Save context on emergency stack
        "sub sp, sp, #(22 * 8)",
        // Save general purpose registers
        "stp x0, x1, [sp, #(0 * 16)]",
        "stp x2, x3, [sp, #(1 * 16)]",
        "stp x4, x5, [sp, #(2 * 16)]",
        "stp x6, x7, [sp, #(3 * 16)]",
        "stp x8, x9, [sp, #(4 * 16)]",
        "stp x10, x11, [sp, #(5 * 16)]",
        "stp x12, x13, [sp, #(6 * 16)]",
        "stp x14, x15, [sp, #(7 * 16)]",
        "str x16, [sp, #(16 * 8)]",
        // Read system registers
        "mrs x0, esr_el2",
        "mrs x1, far_el2",
        "mrs x2, elr_el2",
        "mrs x3, spsr_el2",
        "str x0, [sp, #(17 * 8)]",  // esr_el2
        "str x1, [sp, #(18 * 8)]",  // far_el2
        "str x2, [sp, #(19 * 8)]",  // elr_el2
        "str x3, [sp, #(20 * 8)]",  // spsr_el2
        // Call Rust handler with frame pointer
        "mov x0, sp",
        "bl {handle_el2_sync_current_x}",
        "b .",
        emergency_stack = sym EMERGENCY_STACK,
        handle_el2_sync_current_x = sym handle_el2_sync_current_x,
    );
}

extern "C" fn handle_el2_sync_current_x(frame: &SavedFrame) -> ! {
    unsafe extern "C" {
        static _STACK_BOTTOM: usize;
        static _STACK_TOP: usize;
    }

    let stack_bottom = &raw const _STACK_BOTTOM as usize;
    let stack_top = &raw const _STACK_TOP as usize;
    let guard_page_start = stack_bottom;
    let guard_page_end = stack_bottom + 0x1000;

    let esr_el2 = frame.esr_el2;
    let far_el2 = frame.far_el2;
    let ec = (esr_el2 >> 26) & 0x3F;

    const EC_DATA_ABORT_CURRENT_EL: u64 = 0x25;

    let is_stack_overflow = ec == EC_DATA_ABORT_CURRENT_EL
        && far_el2 >= guard_page_start as u64
        && far_el2 < guard_page_end as u64;

    if is_stack_overflow {
        let original_sp = frame.original_sp;
        let overflowed_guard = original_sp < guard_page_end as u64;

        panic!(
            "EL2 STACK OVERFLOW!\n\
             FAR_EL2: 0x{:016X} (guard page: 0x{:016X}-0x{:016X})\n\
             ESR_EL2: 0x{:016X}\n\
             ELR_EL2: 0x{:016X}\n\
             SPSR_EL2: 0x{:016X}\n\
             Original SP_EL2: 0x{:016X}\n\
             Stack bounds: 0x{:016X}-0x{:016X}\n\
             Original SP below guard end: {}",
            far_el2,
            guard_page_start,
            guard_page_end,
            esr_el2,
            frame.elr_el2,
            frame.spsr_el2,
            original_sp,
            stack_bottom,
            stack_top,
            overflowed_guard
        );
    } else {
        panic!(
            "Unexpected EL2 synchronous exception (sync_current_x)\n\
             ESR_EL2: 0x{:016X} (EC: 0x{:02X})\n\
             FAR_EL2: 0x{:016X}\n\
             ELR_EL2: 0x{:016X}\n\
             SPSR_EL2: 0x{:016X}\n\
             Original SP_EL2: 0x{:016X}\n\
             Stack bounds: 0x{:016X}-0x{:016X}",
            esr_el2,
            ec,
            far_el2,
            frame.elr_el2,
            frame.spsr_el2,
            frame.original_sp,
            stack_bottom,
            stack_top
        );
    }
}
