//! Intel VMX instruction wrappers and capability policy.

use crate::addr::VmcsPhys;
use crate::addr::VmxonPhys;
use core::arch::asm;

pub use crate::vmcs::VM_INSTRUCTION_ERROR;

/// IA32_VMX_BASIC.
pub const IA32_VMX_BASIC: u32 = 0x0480;
/// IA32_VMX_PINBASED_CTLS.
pub const IA32_VMX_PINBASED_CTLS: u32 = 0x0481;
/// IA32_VMX_PROCBASED_CTLS.
pub const IA32_VMX_PROCBASED_CTLS: u32 = 0x0482;
/// IA32_VMX_EXIT_CTLS.
pub const IA32_VMX_EXIT_CTLS: u32 = 0x0483;
/// IA32_VMX_ENTRY_CTLS.
pub const IA32_VMX_ENTRY_CTLS: u32 = 0x0484;
/// IA32_VMX_MISC.
pub const IA32_VMX_MISC: u32 = 0x0485;
/// IA32_VMX_CR0_FIXED0.
pub const IA32_VMX_CR0_FIXED0: u32 = 0x0486;
/// IA32_VMX_CR0_FIXED1.
pub const IA32_VMX_CR0_FIXED1: u32 = 0x0487;
/// IA32_VMX_CR4_FIXED0.
pub const IA32_VMX_CR4_FIXED0: u32 = 0x0488;
/// IA32_VMX_CR4_FIXED1.
pub const IA32_VMX_CR4_FIXED1: u32 = 0x0489;
/// IA32_VMX_VMCS_ENUM.
pub const IA32_VMX_VMCS_ENUM: u32 = 0x048a;
/// IA32_VMX_PROCBASED_CTLS2.
pub const IA32_VMX_PROCBASED_CTLS2: u32 = 0x048b;
/// IA32_VMX_EPT_VPID_CAP.
pub const IA32_VMX_EPT_VPID_CAP: u32 = 0x048c;
/// IA32_VMX_TRUE_PINBASED_CTLS.
pub const IA32_VMX_TRUE_PINBASED_CTLS: u32 = 0x048d;
/// IA32_VMX_TRUE_PROCBASED_CTLS.
pub const IA32_VMX_TRUE_PROCBASED_CTLS: u32 = 0x048e;
/// IA32_VMX_TRUE_EXIT_CTLS.
pub const IA32_VMX_TRUE_EXIT_CTLS: u32 = 0x048f;
/// IA32_VMX_TRUE_ENTRY_CTLS.
pub const IA32_VMX_TRUE_ENTRY_CTLS: u32 = 0x0490;
/// IA32_VMX_VMFUNC.
pub const IA32_VMX_VMFUNC: u32 = 0x0491;
/// IA32_VMX_PROCBASED_CTLS3.
pub const IA32_VMX_PROCBASED_CTLS3: u32 = 0x0492;

/// Outcome reported through RFLAGS.CF and RFLAGS.ZF by a VMX instruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmxStatus {
    /// The instruction completed successfully.
    Success,
    /// VMfailValid; `VM_INSTRUCTION_ERROR` describes the failure.
    FailValid,
    /// VMfailInvalid; there is no current valid VMCS error field.
    FailInvalid,
}

impl VmxStatus {
    /// Decodes the architectural flags written by a VMX instruction.
    const fn from_flags(carry: u8, zero: u8) -> Self {
        if carry != 0 {
            Self::FailInvalid
        } else if zero != 0 {
            Self::FailValid
        } else {
            Self::Success
        }
    }
}

/// Parsed fields from IA32_VMX_BASIC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VmxBasic {
    /// VMCS revision identifier placed in VMXON/VMCS pages.
    pub revision_id: u32,
    /// Hardware-reported VMCS region size.
    pub region_size: u16,
    /// Required VMCS memory type encoding.
    pub memory_type: u8,
    /// Whether VMX physical operands are limited to 32 bits.
    pub physical_address_width_32: bool,
    /// Whether the IA32_VMX_TRUE_* control MSRs are available.
    pub true_controls: bool,
}

impl VmxBasic {
    /// Parses IA32_VMX_BASIC.
    #[must_use]
    pub const fn from_msr(value: u64) -> Self {
        Self {
            revision_id: (value as u32) & 0x7fff_ffff,
            region_size: ((value >> 32) & 0x1fff) as u16,
            physical_address_width_32: value & (1 << 48) != 0,
            memory_type: ((value >> 50) & 0x0f) as u8,
            true_controls: value & (1 << 55) != 0,
        }
    }
}

/// Applies an IA32_VMX control capability MSR to requested control bits.
///
/// Low-half bits are required-one controls; high-half bits are allowed-one
/// controls, matching Intel's `(requested | low) & high` algorithm.
#[must_use]
pub const fn adjust_controls(requested: u32, capability: u64) -> u32 {
    (requested | capability as u32) & (capability >> 32) as u32
}

/// Restricts optional allowed-one controls advertised to a trusted L1.
///
/// Hardware-required-one bits are retained even when omitted from `allowed`.
#[must_use]
pub const fn restrict_controls(capability: u64, allowed: u32) -> u64 {
    let required = capability as u32;
    let can_be_one = (capability >> 32) as u32;
    required as u64 | ((((can_be_one & allowed) | required) as u64) << 32)
}

/// Decodes the memory operand of a VMX instruction executed in 64-bit mode.
///
/// VM-exit instruction information supplies the base, index, scale, address
/// size, and segment. Exit qualification supplies only the displacement.
/// The register callback uses Intel's 0..15 GPR encoding.
#[must_use]
pub fn memory_operand_address_64(
    instruction_info: u32,
    displacement: u64,
    mut read_register: impl FnMut(u8) -> Option<u64>,
    fs_base: u64,
    gs_base: u64,
) -> Option<u64> {
    if instruction_info & (1 << 10) != 0 {
        return None;
    }
    let address_size = (instruction_info >> 7) & 7;
    let segment = (instruction_info >> 15) & 7;
    if address_size > 2 || segment > 5 {
        return None;
    }

    let mut offset = match address_size {
        0 => displacement & u64::from(u16::MAX),
        1 => displacement & u64::from(u32::MAX),
        2 => displacement,
        _ => unreachable!(),
    };
    if instruction_info & (1 << 27) == 0 {
        offset = offset.wrapping_add(read_register(((instruction_info >> 23) & 0xf) as u8)?);
    }
    if instruction_info & (1 << 22) == 0 {
        let index = read_register(((instruction_info >> 18) & 0xf) as u8)?;
        offset = offset.wrapping_add(index.wrapping_shl(instruction_info & 3));
    }
    offset = match address_size {
        0 => offset & u64::from(u16::MAX),
        1 => offset & u64::from(u32::MAX),
        2 => offset,
        _ => unreachable!(),
    };

    Some(offset.wrapping_add(match segment {
        4 => fs_base,
        5 => gs_base,
        _ => 0,
    }))
}

/// Returns the two GPR indices encoded by a register-form VMX instruction.
///
/// Intel calls bits 6:3 "register 1" and bits 31:28 "register 2". Their
/// meaning depends on the instruction; VMREAD uses register 1 as its
/// destination, while VMWRITE uses it as the value source.
#[must_use]
pub const fn register_operand_indices(instruction_info: u32) -> Option<(u8, u8)> {
    if instruction_info & (1 << 10) == 0 {
        return None;
    }
    Some((
        ((instruction_info >> 3) & 0xf) as u8,
        ((instruction_info >> 28) & 0xf) as u8,
    ))
}

/// Executes VMXON on a page containing the hardware revision identifier.
///
/// # Safety
///
/// VMX prerequisites must be active and `region` must identify an exclusively
/// owned, cacheable VMXON page initialized for this CPU.
pub unsafe fn vmxon(region: VmxonPhys) -> VmxStatus {
    let physical = region.get();
    let carry: u8;
    let zero: u8;
    // SAFETY: upheld by the caller; the instruction itself validates the VMX region.
    unsafe {
        asm!(
            "vmxon [{physical}]",
            "setc {carry}",
            "setz {zero}",
            physical = in(reg) &physical,
            carry = lateout(reg_byte) carry,
            zero = lateout(reg_byte) zero,
            options(nostack)
        );
    }
    VmxStatus::from_flags(carry, zero)
}

/// Leaves VMX operation.
///
/// # Safety
///
/// The caller must currently be in VMX root operation.
pub unsafe fn vmxoff() -> VmxStatus {
    let carry: u8;
    let zero: u8;
    // SAFETY: upheld by the caller.
    unsafe {
        asm!(
            "vmxoff",
            "setc {carry}",
            "setz {zero}",
            carry = lateout(reg_byte) carry,
            zero = lateout(reg_byte) zero,
            options(nostack)
        );
    }
    VmxStatus::from_flags(carry, zero)
}

/// Clears a VMCS and resets its launch state.
///
/// # Safety
///
/// `region` must be an exclusively owned hardware-compatible VMCS page.
pub unsafe fn vmclear(region: VmcsPhys) -> VmxStatus {
    let physical = region.get();
    let carry: u8;
    let zero: u8;
    // SAFETY: upheld by the caller.
    unsafe {
        asm!(
            "vmclear [{physical}]",
            "setc {carry}",
            "setz {zero}",
            physical = in(reg) &physical,
            carry = lateout(reg_byte) carry,
            zero = lateout(reg_byte) zero,
            options(nostack)
        );
    }
    VmxStatus::from_flags(carry, zero)
}

/// Makes a VMCS current.
///
/// # Safety
///
/// `region` must be an initialized hardware-compatible VMCS page.
pub unsafe fn vmptrld(region: VmcsPhys) -> VmxStatus {
    let physical = region.get();
    let carry: u8;
    let zero: u8;
    // SAFETY: upheld by the caller.
    unsafe {
        asm!(
            "vmptrld [{physical}]",
            "setc {carry}",
            "setz {zero}",
            physical = in(reg) &physical,
            carry = lateout(reg_byte) carry,
            zero = lateout(reg_byte) zero,
            options(nostack)
        );
    }
    VmxStatus::from_flags(carry, zero)
}

/// Stores the current VMCS physical pointer.
///
/// # Safety
///
/// The caller must currently be in VMX root operation and `destination` must
/// be writable for eight bytes.
pub unsafe fn vmptrst(destination: &mut u64) -> VmxStatus {
    let carry: u8;
    let zero: u8;
    // SAFETY: upheld by the caller.
    unsafe {
        asm!(
            "vmptrst [{destination}]",
            "setc {carry}",
            "setz {zero}",
            destination = in(reg) destination,
            carry = lateout(reg_byte) carry,
            zero = lateout(reg_byte) zero,
            options(nostack)
        );
    }
    VmxStatus::from_flags(carry, zero)
}

/// Operand for INVEPT.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C, align(16))]
pub struct InveptDescriptor {
    /// EPT pointer for single-context invalidation.
    pub ept_pointer: u64,
    /// Must be zero.
    pub reserved: u64,
}

/// Operand for INVVPID.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C, align(16))]
pub struct InvvpidDescriptor {
    /// Virtual-processor identifier.
    pub vpid: u16,
    /// Must be zero.
    pub reserved: [u16; 3],
    /// Linear address for individual-address invalidation.
    pub linear_address: u64,
}

impl InvvpidDescriptor {
    /// Decodes the two little-endian words of an architectural m128 operand.
    #[must_use]
    pub const fn from_words(first: u64, linear_address: u64) -> Self {
        Self {
            vpid: first as u16,
            reserved: [
                (first >> 16) as u16,
                (first >> 32) as u16,
                (first >> 48) as u16,
            ],
            linear_address,
        }
    }
}

/// Executes INVEPT.
///
/// # Safety
///
/// The caller must be in VMX root operation and provide a supported kind and
/// architecturally valid descriptor.
pub unsafe fn invept(kind: u64, descriptor: &InveptDescriptor) -> VmxStatus {
    let carry: u8;
    let zero: u8;
    // SAFETY: upheld by the caller; hardware validates the kind and descriptor.
    unsafe {
        asm!(
            "invept {kind}, [{descriptor}]",
            "setc {carry}",
            "setz {zero}",
            kind = in(reg) kind,
            descriptor = in(reg) descriptor,
            carry = lateout(reg_byte) carry,
            zero = lateout(reg_byte) zero,
            options(nostack)
        );
    }
    VmxStatus::from_flags(carry, zero)
}

/// Executes INVVPID.
///
/// # Safety
///
/// The caller must be in VMX root operation and provide a supported kind and
/// architecturally valid descriptor.
pub unsafe fn invvpid(kind: u64, descriptor: &InvvpidDescriptor) -> VmxStatus {
    let carry: u8;
    let zero: u8;
    // SAFETY: upheld by the caller; hardware validates the kind and descriptor.
    unsafe {
        asm!(
            "invvpid {kind}, [{descriptor}]",
            "setc {carry}",
            "setz {zero}",
            kind = in(reg) kind,
            descriptor = in(reg) descriptor,
            carry = lateout(reg_byte) carry,
            zero = lateout(reg_byte) zero,
            options(nostack)
        );
    }
    VmxStatus::from_flags(carry, zero)
}

/// Reads a field from the current VMCS.
///
/// # Safety
///
/// A valid VMCS must be current and `field` must be a supported encoding.
pub unsafe fn vmread(field: u32) -> Result<u64, VmxStatus> {
    let value: u64;
    let carry: u8;
    let zero: u8;
    // SAFETY: upheld by the caller; hardware validates the field encoding.
    unsafe {
        asm!(
            "vmread {value}, {field}",
            "setc {carry}",
            "setz {zero}",
            value = lateout(reg) value,
            field = in(reg) u64::from(field),
            carry = lateout(reg_byte) carry,
            zero = lateout(reg_byte) zero,
            options(nostack)
        );
    }
    match VmxStatus::from_flags(carry, zero) {
        VmxStatus::Success => Ok(value),
        failure => Err(failure),
    }
}

/// Writes a field in the current VMCS.
///
/// # Safety
///
/// A valid VMCS must be current and the field/value pair must be supported.
pub unsafe fn vmwrite(field: u32, value: u64) -> VmxStatus {
    let carry: u8;
    let zero: u8;
    // SAFETY: upheld by the caller; hardware validates the field/value pair.
    unsafe {
        asm!(
            "vmwrite {field}, {value}",
            "setc {carry}",
            "setz {zero}",
            value = in(reg) value,
            field = in(reg) u64::from(field),
            carry = lateout(reg_byte) carry,
            zero = lateout(reg_byte) zero,
            options(nostack)
        );
    }
    VmxStatus::from_flags(carry, zero)
}

/// Executes VMLAUNCH for the current VMCS.
///
/// On successful entry this function returns only after some host entry path
/// explicitly resumes its caller.
///
/// # Safety
///
/// Every required host, guest, and control field must be valid.
pub unsafe fn vmlaunch() -> VmxStatus {
    // SAFETY: inherited from this function's contract.
    unsafe { vm_entry_instruction(false) }
}

/// Executes VMRESUME for the current VMCS.
///
/// # Safety
///
/// The current VMCS must be launched and contain valid entry state.
pub unsafe fn vmresume() -> VmxStatus {
    // SAFETY: inherited from this function's contract.
    unsafe { vm_entry_instruction(true) }
}

/// Executes VMCALL from VMX non-root operation.
///
/// # Safety
///
/// The current execution context must be a guest whose monitor implements the
/// selected hypercall ABI.
pub unsafe fn vmcall() {
    // SAFETY: upheld by the caller; VMCALL unconditionally transfers to VMX root.
    unsafe { asm!("vmcall", options(nostack)) };
}

/// Shared VM-entry instruction wrapper.
unsafe fn vm_entry_instruction(resume: bool) -> VmxStatus {
    let carry: u8;
    let zero: u8;
    // SAFETY: upheld by the caller.
    unsafe {
        if resume {
            asm!(
                "vmresume",
                "setc {carry}",
                "setz {zero}",
                carry = lateout(reg_byte) carry,
                zero = lateout(reg_byte) zero,
                options(nostack)
            );
        } else {
            asm!(
                "vmlaunch",
                "setc {carry}",
                "setz {zero}",
                carry = lateout(reg_byte) carry,
                zero = lateout(reg_byte) zero,
                options(nostack)
            );
        }
    }
    VmxStatus::from_flags(carry, zero)
}

#[cfg(test)]
mod tests {
    use super::InvvpidDescriptor;
    use super::VmxBasic;
    use super::adjust_controls;
    use super::memory_operand_address_64;
    use super::register_operand_indices;
    use super::restrict_controls;

    #[test]
    fn capability_adjustment_enforces_required_and_allowed_bits() {
        let capability = 0b0010_u64 | (u64::from(0b1110_u32) << 32);
        assert_eq!(adjust_controls(0b1101, capability), 0b1110);
        assert_eq!(
            restrict_controls(capability, 0b0100),
            0b0010 | (0b0110_u64 << 32)
        );
    }

    #[test]
    fn vmx_basic_fields_are_parsed() {
        let raw = 0x1234_u64 | (0x1000_u64 << 32) | (6_u64 << 50) | (1_u64 << 55);
        assert_eq!(
            VmxBasic::from_msr(raw),
            VmxBasic {
                revision_id: 0x1234,
                region_size: 0x1000,
                memory_type: 6,
                physical_address_width_32: false,
                true_controls: true,
            }
        );
    }

    #[test]
    fn linux_vmxon_stack_operand_is_decoded() {
        let rsp = 0xffff_8f07_006d_4000;
        assert_eq!(
            memory_operand_address_64(
                0x6261_4924,
                0,
                |register| (register == 4).then_some(rsp),
                0,
                0,
            ),
            Some(rsp)
        );
    }

    #[test]
    fn linux_vmwrite_register_operands_are_decoded() {
        assert_eq!(register_operand_indices(0x0361_cd34), Some((6, 0)));
        assert_eq!(register_operand_indices(0x0361_c934), None);
    }

    #[test]
    fn invvpid_m128_operand_is_decoded_without_losing_reserved_bits() {
        assert_eq!(
            InvvpidDescriptor::from_words(0x7766_5544_3322_1100, 0xffff_8000_1234_5000),
            InvvpidDescriptor {
                vpid: 0x1100,
                reserved: [0x3322, 0x5544, 0x7766],
                linear_address: 0xffff_8000_1234_5000,
            }
        );
    }
}
