//! Direct L1 XSAVE/CPUID contract, also run unchanged on the reference backend.
//! The temporary IDT catches only the test's exact faulting instruction RIP.
//! Firmware tables, CR0/CR4, XCR0 and IF are restored before returning a result.

use super::Result;
use super::equal;
use super::l1_extended;
use super::l1_fault;
use core::arch::asm;
use x86_64_hal::control_state::CR0_NE;
use x86_64_hal::cpu;
use x86_64_hal::xstate;

/// Does not call Rust while the faulting RIP is live. ECX and EDX:EAX deliberately
/// retain high input bits in separate tests of architectural operand truncation.
fn probe(index: u64, value: u64, high_bits: u64) -> Result<(u64, u64)> {
    let before = l1_extended::seed();
    let mut after = l1_extended::Image::zeroed();
    let mut saved = l1_extended::Image::zeroed();
    let record = l1_fault::probe(|record| {
        // SAFETY: the single-CPU fixture installed live #GP/#UD gates, disabled IF,
        // and published this stack record. The ISR accepts only label 2's RIP and
        // returns to label 3 without altering input GPRs, XSTATE or any other RIP.
        // The three disjoint aligned images live through this bracket; firmware
        // has OSFXSR set and EM/TS clear even when this test clears OSXSAVE.
        unsafe {
            asm!(
                "lea r8, [rip + 2f]", "mov [r10], r8",
                "lea r8, [rip + 3f]", "mov [r10 + 8], r8",
                "fxsave64 [r9]", "fxrstor64 [rdi]",
                "2:", "xsetbv", "3:",
                "fxsave64 [rsi]", "fxrstor64 [r9]",
                in("rdi") &before, in("rsi") &mut after, in("r9") &mut saved,
                in("r10") record,
                in("rcx") index,
                in("rax") u64::from(value as u32) | high_bits,
                in("rdx") (value >> 32) | high_bits,
                out("r8") _,
                options(nostack),
            );
        }
    });
    l1_extended::verify(&before, &after, false)?;
    Ok((record.vector, record.error))
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

/// AP startup needs a visible NE=0 even though the hardware carrier needs NE=1.
/// Exercise the L1 carrier, before this fixture enters its own VMX operation.
fn cr0_ne() -> Result<()> {
    let original = cpu::read_cr0();
    let result = (|| {
        for ne in [0, CR0_NE, 0, CR0_NE] {
            let value = (original & !CR0_NE) | ne;
            // SAFETY: IF is clear, firmware's x87 exceptions are masked and no
            // x87 operation occurs here. Only numeric-error routing changes;
            // paging, protection, cache and extended-state enables stay intact.
            unsafe { cpu::write_cr0(value) };
            let _ = cpu::cpuid(0, 0);
            equal("l1-cr0-ne-visible", cpu::read_cr0(), value)?;
        }
        let before = cpu::read_cr0();
        let toggled = before ^ CR0_NE;
        for invalid in [
            toggled | (1 << 63),
            (toggled | (1 << 29)) & !(1 << 30),
            toggled & !1,
            toggled & !(1 << 31),
        ] {
            let record = probe_cr0(invalid);
            equal("l1-cr0-gp-vector", record.vector, 13)?;
            equal("l1-cr0-gp-error", record.error, 0)?;
            equal("l1-cr0-gp-preserves", cpu::read_cr0(), before)?;
        }
        Ok(())
    })();
    // SAFETY: all fallible checks pass through this restoration; original CR0
    // is the valid firmware value on this same CPU, with its original NE bit.
    unsafe { cpu::write_cr0(original) };
    result
}

/// Caller has installed the exact-RIP fault handler and supplies an invalid
/// long-mode CR0 value, or a value whose only possible successful change is NE.
pub(super) fn probe_cr0(value: u64) -> l1_fault::FaultRecord {
    l1_fault::probe(|record| {
        // SAFETY: the caller owns this CPU with IF clear and the private #GP
        // gate live. It accepts only label 2 and returns to label 3. Code/stack
        // mappings cannot change on a valid implementation, and no Rust call
        // occurs while the faulting RIP is live. The record remains on stack.
        unsafe {
            asm!(
                "lea r8, [rip + 2f]", "mov [r10], r8",
                "lea r8, [rip + 3f]", "mov [r10 + 8], r8",
                "2:", "mov cr0, r11", "3:",
                in("r10") record, in("r11") value, out("r8") _,
                options(nostack),
            );
        }
    })
}

/// Temporarily makes PKRU permissive before any data access with PKE enabled.
/// PKRU may outlive a cleared CR4.PKE, so enabling PKE alone is not safe.
fn protection_keys(cr4: u64) -> Result<()> {
    let features = cpu::cpuid(7, 0).ecx;
    if features & xstate::CPUID_PKU == 0 {
        return equal(
            "l1-cpuid-ospke-unavailable",
            u64::from(features & xstate::CPUID_OSPKE),
            0,
        );
    }
    let saved_pkru: u32;
    // SAFETY: PKU was advertised; IF is clear and this fixture owns its CPU.
    // After enabling PKE, no data access occurs before saving PKRU in a GPR
    // and making all keys permissive. ECX/EDX satisfy RDPKRU/WRPKRU constraints.
    unsafe {
        asm!(
            "mov cr4, {enabled}", "xor ecx, ecx", "rdpkru",
            "mov {saved:e}, eax", "xor eax, eax", "wrpkru",
            enabled = in(reg) cr4 | xstate::CR4_PKE,
            saved = out(reg) saved_pkru,
            out("eax") _, out("ecx") _, out("edx") _,
            options(nostack),
        );
    }
    let result = (|| {
        for enabled in [false, true, false, true] {
            let value = (cr4 & !xstate::CR4_PKE) | if enabled { xstate::CR4_PKE } else { 0 };
            // SAFETY: only supported PKE changes and live PKRU is zero, so this
            // cannot revoke any access to the fixture, stack or firmware tables.
            unsafe { cpu::write_cr4(value) };
            let actual = cpu::cpuid(7, 0).ecx;
            equal(
                "l1-cpuid-ospke",
                u64::from(actual & xstate::CPUID_OSPKE != 0),
                u64::from(enabled),
            )?;
            equal(
                "l1-cpuid-pku-static",
                u64::from(actual & !xstate::CPUID_OSPKE),
                u64::from(features & !xstate::CPUID_OSPKE),
            )?;
        }
        Ok(())
    })();
    // SAFETY: all fallible checks return here. There is no data access between
    // restoring the original PKRU and restoring CR4's original PKE enablement.
    // The exact original permissions are then in force before Rust resumes.
    unsafe {
        asm!(
            "mov cr4, {enabled}", "wrpkru", "mov cr4, {original}",
            enabled = in(reg) cr4 | xstate::CR4_PKE,
            original = in(reg) cr4,
            in("eax") saved_pkru, in("ecx") 0_u32, in("edx") 0_u32,
            options(nostack),
        );
    }
    result
}

/// Runs before VMXON in L1, so CPUID/XSETBV exercise the project L0 interception,
/// not KVM's L2 emulation. Fault probes keep IF clear; explicit STI/HLT checks
/// retain the copied firmware timer gates and restore IF=0 before capture.
pub(super) fn run() -> Result<()> {
    l1_fault::with_handler(run_with_handler)
}

fn run_with_handler() -> Result<()> {
    cr0_ne()?;
    let leaf1 = cpu::cpuid(1, 0);
    let legacy = (1 << 24) | (1 << 25) | (1 << 26);
    equal(
        "l1-extended-features",
        u64::from(leaf1.edx & legacy),
        u64::from(legacy),
    )?;
    equal("l1-extended-cr0", cpu::read_cr0() & 0xc, 0)?;
    equal("l1-extended-osfxsr", cpu::read_cr4() & (1 << 9), 1 << 9)?;
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
    let cr4 = cpu::read_cr4();
    // SAFETY: CPUID.XSAVE was checked. Only OSXSAVE is enabled, permitting XGETBV;
    // no state component is changed before recording the original XCR0.
    unsafe { cpu::write_cr4(cr4 | xstate::CR4_OSXSAVE) };
    // SAFETY: XSAVE exists and this CPU's CR4.OSXSAVE is now set.
    let xcr0 = unsafe { cpu::xgetbv(0) };
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
        protection_keys(cr4 | xstate::CR4_OSXSAVE)?;
        for value in [1, 3, xcr0] {
            equal("l1-xsetbv-valid", probe(0, value, 0)?.0, u64::MAX)?;
            // SAFETY: OSXSAVE remains enabled after the toggle loop.
            equal("l1-xsetbv-value", unsafe { cpu::xgetbv(0) }, value)?;
            enabled_area_size(value)?;
            // SAFETY: the fixture is CPL0 with IF clear and firmware FXSR/SSE2
            // enabled. Both CPUIDs preserve XCR0; the tested write repeats the
            // already accepted value. In particular value=1 disables XCR0.SSE,
            // but must not permit L0 to corrupt the still-usable XMM registers.
            unsafe {
                l1_extended::check(l1_extended::Operation::Cpuid, false)?;
                l1_extended::check(l1_extended::Operation::CpuidD, false)?;
                l1_extended::check(l1_extended::Operation::Xsetbv(value), false)?;
                // The copied firmware IDT/timer remain live. STI;HLT opens a
                // bounded interrupt window with sentinels live, then clears IF.
                l1_extended::check(l1_extended::Operation::Interrupt, false)?;
            }
        }
        if leaf1.ecx & (1 << 28) != 0 && supported & 7 == 7 {
            // SAFETY: AVX and its x87/SSE prerequisites were advertised. IF is
            // clear; all fallible checks pass through original-XCR0 cleanup.
            unsafe {
                cpu::xsetbv(0, 7);
                l1_extended::check(l1_extended::Operation::Cpuid, true)?;
                l1_extended::check(l1_extended::Operation::CpuidD, true)?;
                l1_extended::check(l1_extended::Operation::Xsetbv(7), true)?;
                l1_extended::check(l1_extended::Operation::Interrupt, true)?;
                cpu::xsetbv(0, xcr0);
            }
        }
        equal(
            "l1-xsetbv-high-operands",
            probe(1 << 32, xcr0, 0xabcd_1234_0000_0000)?.0,
            u64::MAX,
        )?;
        for (index, value) in [(1, xcr0), (0, 0), (0, 5), (0, xcr0 | (1 << reserved))] {
            let (vector, error) = probe(index, value, 0)?;
            equal("l1-xsetbv-gp-vector", vector, 13)?;
            equal("l1-xsetbv-gp-error", error, 0)?;
            // SAFETY: OSXSAVE is enabled; the fault must not have modified XCR0.
            equal("l1-xsetbv-fault-preserves", unsafe { cpu::xgetbv(0) }, xcr0)?;
        }
        // SAFETY: clearing only OSXSAVE is valid; the private fixture #UD gate
        // must observe the unchanged XSETBV RIP, then return to its continuation.
        unsafe { cpu::write_cr4(cr4 & !xstate::CR4_OSXSAVE) };
        let fault = probe(1, 0, 0)?;
        // SAFETY: re-enable the already established XSAVE facility for XGETBV.
        unsafe { cpu::write_cr4(cr4 | xstate::CR4_OSXSAVE) };
        equal("l1-xsetbv-ud-priority", fault.0, 6)?;
        // SAFETY: OSXSAVE is enabled; even the #UD case must preserve XCR0.
        equal("l1-xsetbv-ud-preserves", unsafe { cpu::xgetbv(0) }, xcr0)?;
        Ok(())
    })();
    // SAFETY: every fallible test above returns through this cleanup. Restore
    // the captured valid XCR0 with OSXSAVE enabled, then the original CR4. The
    // enclosing fault fixture restores IDTR and IF after this function returns.
    unsafe {
        cpu::write_cr4(cr4 | xstate::CR4_OSXSAVE);
        cpu::xsetbv(0, xcr0);
        cpu::write_cr4(cr4);
    }
    result
}
