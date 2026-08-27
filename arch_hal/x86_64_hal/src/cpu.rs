//! Privileged x86-64 CPU instructions.

use core::arch::asm;
use core::arch::x86_64::__cpuid_count;
use core::ptr;

/// IA32_FEATURE_CONTROL.
pub const IA32_FEATURE_CONTROL: u32 = 0x003a;
/// IA32_SYSENTER_CS.
pub const IA32_SYSENTER_CS: u32 = 0x0174;
/// IA32_SYSENTER_ESP.
pub const IA32_SYSENTER_ESP: u32 = 0x0175;
/// IA32_SYSENTER_EIP.
pub const IA32_SYSENTER_EIP: u32 = 0x0176;
/// IA32_PAT.
pub const IA32_PAT: u32 = 0x0277;
/// IA32_EFER.
pub const IA32_EFER: u32 = 0xc000_0080;
/// IA32_FS_BASE.
pub const IA32_FS_BASE: u32 = 0xc000_0100;
/// IA32_GS_BASE.
pub const IA32_GS_BASE: u32 = 0xc000_0101;

/// The base address and inclusive limit held by GDTR or IDTR.
///
/// This is an aligned Rust value rather than the packed ten-byte instruction
/// operand used by SGDT and SIDT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DescriptorTable {
    /// Inclusive byte limit of the table.
    pub limit: u16,
    /// Linear base address of the table.
    pub base: u64,
}

/// Packed memory operand written by SGDT and SIDT in 64-bit mode.
#[repr(C, packed)]
struct RawDescriptorTable {
    /// Inclusive byte limit.
    limit: u16,
    /// Linear base address.
    base: u64,
}

/// CPUID register result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuidResult {
    /// EAX result.
    pub eax: u32,
    /// EBX result.
    pub ebx: u32,
    /// ECX result.
    pub ecx: u32,
    /// EDX result.
    pub edx: u32,
}

/// Executes CPUID with the supplied leaf and subleaf.
#[must_use]
pub fn cpuid(leaf: u32, subleaf: u32) -> CpuidResult {
    // SAFETY: CPUID is available in x86-64 mode and has no memory-safety preconditions.
    let result = unsafe { __cpuid_count(leaf, subleaf) };
    CpuidResult {
        eax: result.eax,
        ebx: result.ebx,
        ecx: result.ecx,
        edx: result.edx,
    }
}

/// Returns whether VMX is advertised by CPUID.01H:ECX[5].
#[must_use]
pub fn has_vmx() -> bool {
    cpuid(1, 0).ecx & (1 << 5) != 0
}

/// Reads GDTR with SGDT.
#[must_use]
pub fn sgdt() -> DescriptorTable {
    let mut raw = RawDescriptorTable { limit: 0, base: 0 };
    // SAFETY: `raw` is the architecturally required writable ten-byte operand.
    unsafe {
        asm!(
            "sgdt [{raw}]",
            raw = in(reg) ptr::addr_of_mut!(raw),
            options(nostack, preserves_flags)
        );
    }
    DescriptorTable {
        limit: raw.limit,
        base: raw.base,
    }
}

/// Reads IDTR with SIDT.
#[must_use]
pub fn sidt() -> DescriptorTable {
    let mut raw = RawDescriptorTable { limit: 0, base: 0 };
    // SAFETY: `raw` is the architecturally required writable ten-byte operand.
    unsafe {
        asm!(
            "sidt [{raw}]",
            raw = in(reg) ptr::addr_of_mut!(raw),
            options(nostack, preserves_flags)
        );
    }
    DescriptorTable {
        limit: raw.limit,
        base: raw.base,
    }
}

/// Defines a reader for a 16-bit segment or system selector.
macro_rules! selector_reader {
    ($name:ident, $instruction:literal, $documentation:literal) => {
        #[doc = $documentation]
        #[inline(always)]
        #[must_use]
        pub fn $name() -> u16 {
            let selector: u16;
            // SAFETY: the instruction only copies architectural selector state.
            unsafe {
                asm!(
                    $instruction,
                    selector = out(reg) selector,
                    options(nomem, nostack, preserves_flags)
                );
            }
            selector
        }
    };
}

selector_reader!(read_es, "mov {selector:x}, es", "Reads the ES selector.");
selector_reader!(read_cs, "mov {selector:x}, cs", "Reads the CS selector.");
selector_reader!(read_ss, "mov {selector:x}, ss", "Reads the SS selector.");
selector_reader!(read_ds, "mov {selector:x}, ds", "Reads the DS selector.");
selector_reader!(read_fs, "mov {selector:x}, fs", "Reads the FS selector.");
selector_reader!(read_gs, "mov {selector:x}, gs", "Reads the GS selector.");
selector_reader!(
    read_ldtr,
    "sldt {selector:x}",
    "Reads the local descriptor table selector."
);
selector_reader!(
    read_tr,
    "str {selector:x}",
    "Reads the task-register selector."
);

/// Reads RSP at the inlined call site.
#[allow(clippy::inline_always)]
#[inline(always)]
#[must_use]
pub fn read_rsp() -> u64 {
    let value: u64;
    // SAFETY: the instruction only copies RSP.
    unsafe {
        asm!("mov {value}, rsp", value = out(reg) value, options(nomem, nostack, preserves_flags));
    };
    value
}

/// Reads RFLAGS.
#[must_use]
pub fn read_rflags() -> u64 {
    let value: u64;
    // SAFETY: PUSHFQ/POP use one balanced stack slot and preserve RFLAGS.
    unsafe { asm!("pushfq", "pop {value}", value = out(reg) value, options(preserves_flags)) };
    value
}

/// Reads DR7.
#[must_use]
pub fn read_dr7() -> u64 {
    let value: u64;
    // SAFETY: reading DR7 is valid at CPL0, where the monitor always runs.
    unsafe {
        asm!("mov {value}, dr7", value = out(reg) value, options(nomem, nostack, preserves_flags));
    };
    value
}

/// Returns the effective byte limit reported by LSL for `selector`.
///
/// `None` means that the selector is null or is not visible at the current
/// privilege level.
#[must_use]
pub fn segment_limit(selector: u16) -> Option<u32> {
    let mut limit = 0_u32;
    let valid: u8;
    // SAFETY: LSL only reads descriptor metadata and reports failure through ZF.
    unsafe {
        asm!(
            "lsl {limit:e}, {selector:x}",
            "setz {valid}",
            limit = inout(reg) limit,
            selector = in(reg) selector,
            valid = lateout(reg_byte) valid,
            options(nomem, nostack)
        );
    }
    (valid != 0).then_some(limit)
}

/// Returns LAR access rights in the layout used by VMCS guest AR fields.
///
/// `None` means that the selector is null or is not visible at the current
/// privilege level. A VMCS caller should encode such a segment as unusable.
#[must_use]
pub fn segment_access_rights(selector: u16) -> Option<u32> {
    let mut access_rights = 0_u32;
    let valid: u8;
    // SAFETY: LAR only reads descriptor metadata and reports failure through ZF.
    unsafe {
        asm!(
            "lar {access_rights:e}, {selector:x}",
            "setz {valid}",
            access_rights = inout(reg) access_rights,
            selector = in(reg) selector,
            valid = lateout(reg_byte) valid,
            options(nomem, nostack)
        );
    }
    (valid != 0).then_some(vmcs_access_rights(access_rights))
}

/// Reads the base of a present code, data, LDT, or TSS descriptor in `gdtr`.
///
/// Null selectors, LDT selectors, out-of-limit descriptors, and unsupported
/// system-descriptor types return `None`.
///
/// # Safety
///
/// `gdtr.base..=gdtr.base + gdtr.limit` must remain readable for the duration
/// of this call.
#[must_use]
pub unsafe fn gdt_segment_base(gdtr: DescriptorTable, selector: u16) -> Option<u64> {
    if selector & !0x7 == 0 || selector & 0x4 != 0 {
        return None;
    }

    let offset = usize::from(selector & !0x7);
    if offset.checked_add(7)? > usize::from(gdtr.limit) {
        return None;
    }
    let address = gdtr.base.checked_add(offset as u64)? as *const u64;
    // SAFETY: the caller guarantees the range and the limit check covers this word.
    let low = unsafe { ptr::read_unaligned(address) };
    if low & (1 << 47) == 0 {
        return None;
    }

    let system = low & (1 << 44) == 0;
    let high = if system {
        let descriptor_type = (low >> 40) & 0xf;
        if !matches!(descriptor_type, 2 | 9 | 11)
            || offset.checked_add(15)? > usize::from(gdtr.limit)
        {
            return None;
        }
        // SAFETY: the caller guarantees the range and the limit check covers this word.
        unsafe { ptr::read_unaligned(address.add(1)) }
    } else {
        0
    };
    Some(descriptor_base(low, high))
}

/// Converts the masked LAR result to Intel's VMCS access-rights layout.
const fn vmcs_access_rights(lar: u32) -> u32 {
    (lar >> 8) & 0xf0ff
}

/// Extracts the base from an eight- or sixteen-byte GDT descriptor.
const fn descriptor_base(low: u64, high: u64) -> u64 {
    ((low >> 16) & 0xffff)
        | (((low >> 32) & 0xff) << 16)
        | (((low >> 56) & 0xff) << 24)
        | ((high & 0xffff_ffff) << 32)
}

/// Reads one model-specific register.
///
/// # Safety
///
/// The caller must run at CPL0 and ensure `msr` is implemented and readable.
#[must_use]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: upheld by the caller.
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags)
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// Writes one model-specific register.
///
/// # Safety
///
/// The caller must run at CPL0 and ensure the value is valid for `msr`.
pub unsafe fn wrmsr(msr: u32, value: u64) {
    // SAFETY: upheld by the caller.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Reads one extended control register.
///
/// # Safety
///
/// `xcr` must be readable on this CPU and CR4.OSXSAVE must be set.
#[must_use]
pub unsafe fn xgetbv(xcr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: upheld by the caller.
    unsafe {
        asm!(
            "xgetbv",
            in("ecx") xcr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags)
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// Writes one extended control register.
///
/// # Safety
///
/// `xcr` and `value` must form an architecturally valid combination and
/// CR4.OSXSAVE must be set.
pub unsafe fn xsetbv(xcr: u32, value: u64) {
    // SAFETY: upheld by the caller.
    unsafe {
        asm!(
            "xsetbv",
            in("ecx") xcr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Reads CR0.
#[must_use]
pub fn read_cr0() -> u64 {
    let value: u64;
    // SAFETY: reading CR0 is valid at CPL0, where the monitor always runs.
    unsafe { asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Writes CR0.
///
/// # Safety
///
/// `value` must preserve a valid execution mode and satisfy VMX fixed bits.
pub unsafe fn write_cr0(value: u64) {
    // SAFETY: upheld by the caller.
    unsafe { asm!("mov cr0, {}", in(reg) value, options(nomem, nostack, preserves_flags)) };
}

/// Reads CR3.
#[must_use]
pub fn read_cr3() -> u64 {
    let value: u64;
    // SAFETY: reading CR3 is valid at CPL0, where the monitor always runs.
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Reads CR4.
#[must_use]
pub fn read_cr4() -> u64 {
    let value: u64;
    // SAFETY: reading CR4 is valid at CPL0, where the monitor always runs.
    unsafe { asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Writes CR4.
///
/// # Safety
///
/// `value` must preserve a valid execution mode and satisfy VMX fixed bits.
pub unsafe fn write_cr4(value: u64) {
    // SAFETY: upheld by the caller.
    unsafe { asm!("mov cr4, {}", in(reg) value, options(nomem, nostack, preserves_flags)) };
}

/// Writes an 8-bit value to an I/O port.
///
/// # Safety
///
/// The caller must own the port and run with sufficient I/O privilege.
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: upheld by the caller.
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
}

/// Reads an 8-bit value from an I/O port.
///
/// # Safety
///
/// The caller must own the port and run with sufficient I/O privilege.
#[must_use]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: upheld by the caller.
    unsafe { asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack)) };
    value
}

#[cfg(test)]
mod tests {
    use super::DescriptorTable;
    use super::cpuid;
    use super::gdt_segment_base;
    use super::vmcs_access_rights;

    #[test]
    fn cpuid_leaf_zero_is_available() {
        assert!(cpuid(0, 0).eax >= 1);
    }

    #[test]
    fn decodes_vmcs_access_rights_and_a_64_bit_tss_base() {
        assert_eq!(vmcs_access_rights(0x00af_9b00), 0xa09b);

        let base = 0x1234_5678_9abc_def0_u64;
        let low = ((base & 0xffff) << 16)
            | (((base >> 16) & 0xff) << 32)
            | (0x89_u64 << 40)
            | (((base >> 24) & 0xff) << 56);
        let descriptor = [0, low, base >> 32];
        let gdtr = DescriptorTable {
            limit: 23,
            base: descriptor.as_ptr() as u64,
        };

        // SAFETY: `gdtr` covers the local two-word descriptor for this call.
        assert_eq!(unsafe { gdt_segment_base(gdtr, 8) }, Some(base));
        // SAFETY: the same valid table is used to exercise rejected selectors.
        assert_eq!(unsafe { gdt_segment_base(gdtr, 0) }, None);
        // SAFETY: the same valid table is used to exercise rejected selectors.
        assert_eq!(unsafe { gdt_segment_base(gdtr, 12) }, None);
        // SAFETY: the descriptor's second word lies outside this shortened table.
        assert_eq!(
            unsafe {
                gdt_segment_base(
                    DescriptorTable {
                        limit: 15,
                        base: gdtr.base,
                    },
                    8,
                )
            },
            None
        );
    }
}
