//! Pure policy for guest-visible XSAVE enablement and XSETBV faults.
//!
//! Intel SDM volume 1, section 13.3, and volume 2, XSETBV:
//! <https://cdrdv2-public.intel.com/868137/325462-089-sdm-vol-1-2abcd-3abcd-4.pdf>.

use crate::cpu::CpuidResult;

/// CR4.OSXSAVE enables the XSAVE instruction set, not static CPU availability.
pub const CR4_OSXSAVE: u64 = 1 << 18;
/// CR4.PKE enables user-page protection keys and their access instructions.
pub const CR4_PKE: u64 = 1 << 22;
/// CPUID.7.0:ECX.PKU, static protection-key availability.
pub const CPUID_PKU: u32 = 1 << 3;
/// CPUID.7.0:ECX.OSPKE, reflecting the executing context's CR4.PKE.
pub const CPUID_OSPKE: u32 = 1 << 4;
/// CPUID.1:ECX.XSAVE, a static feature bit.
pub const CPUID_XSAVE: u32 = 1 << 26;
/// CPUID.1:ECX.OSXSAVE, reflecting the executing context's CR4.OSXSAVE.
pub const CPUID_OSXSAVE: u32 = 1 << 27;

/// Reconstructs a guest control register from its hardware value and VMX shadow.
#[must_use]
pub const fn visible_cr(hardware: u64, mask: u64, shadow: u64) -> u64 {
    (hardware & !mask) | (shadow & mask)
}

/// Changes only the dynamic OSXSAVE bit, preserving unrelated CPUID features.
#[must_use]
pub const fn leaf1_for_cr4(mut leaf: CpuidResult, cr4: u64) -> CpuidResult {
    leaf.ecx &= !CPUID_OSXSAVE;
    if leaf.ecx & CPUID_XSAVE != 0 && cr4 & CR4_OSXSAVE != 0 {
        leaf.ecx |= CPUID_OSXSAVE;
    }
    leaf
}

/// Changes only leaf 7 subleaf 0's dynamic OSPKE bit, not PKU availability.
#[must_use]
pub const fn leaf7_for_cr4(mut leaf: CpuidResult, cr4: u64) -> CpuidResult {
    leaf.ecx &= !CPUID_OSPKE;
    if leaf.ecx & CPUID_PKU != 0 && cr4 & CR4_PKE != 0 {
        leaf.ecx |= CPUID_OSPKE;
    }
    leaf
}

/// Architectural fault before any XCR is changed; neither fault advances RIP.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XsetbvFault {
    /// XSAVE is unavailable or virtual CR4.OSXSAVE is clear: #UD.
    InvalidOpcode,
    /// Privilege, register index, reserved bits or dependencies are invalid: #GP(0).
    GeneralProtection,
}

/// Checks the currently advertised XCR0 mask and all architectural dependencies.
///
/// No host instruction executes here. The caller must obtain `supported` from
/// the same CPUID.0D.0 bitmap advertised to the guest on the executing CPU.
/// The #UD conditions take precedence over the #GP conditions.
pub const fn validate_xsetbv(
    xsave: bool,
    cr4: u64,
    cpl: u8,
    index: u32,
    value: u64,
    supported: u64,
) -> Result<(), XsetbvFault> {
    if !xsave || cr4 & CR4_OSXSAVE == 0 {
        return Err(XsetbvFault::InvalidOpcode);
    }
    let mpx = value & (3 << 3);
    let avx512 = value & (7 << 5);
    let amx = value & (3 << 17);
    if cpl != 0
        || index != 0
        || value & 1 == 0
        || value & !supported != 0
        || (value & 4 != 0 && value & 2 == 0)
        || (mpx != 0 && mpx != 3 << 3)
        || (avx512 != 0 && (avx512 != 7 << 5 || value & 6 != 6))
        || (amx != 0 && amx != 3 << 17)
    {
        return Err(XsetbvFault::GeneralProtection);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ospke_tracks_guest_pke_without_changing_static_capabilities() {
        for available in [0, CPUID_PKU] {
            for host in [0, CPUID_OSPKE] {
                for guest in [0, CR4_PKE] {
                    let input = CpuidResult {
                        eax: 2,
                        ebx: 3,
                        ecx: available | host | (1 << 9),
                        edx: 5,
                    };
                    let output = leaf7_for_cr4(input, guest);
                    assert_eq!(output.ecx & CPUID_OSPKE != 0, available != 0 && guest != 0);
                    assert_eq!(output.ecx & !CPUID_OSPKE, input.ecx & !CPUID_OSPKE);
                    assert_eq!((output.eax, output.ebx, output.edx), (2, 3, 5));
                }
            }
        }
    }

    #[test]
    fn virtual_cr4_and_osxsave_ignore_private_host_enablement() {
        for mask in [0, CR4_OSXSAVE, u64::MAX] {
            for hardware in [0, CR4_OSXSAVE] {
                for shadow in [0, CR4_OSXSAVE] {
                    let cr4 = visible_cr(hardware, mask, shadow);
                    let expected = if mask == 0 { hardware } else { shadow };
                    assert_eq!(cr4, expected);
                    for host in [0, CPUID_OSXSAVE] {
                        let input = CpuidResult {
                            eax: 7,
                            ebx: 11,
                            ecx: CPUID_XSAVE | host | 5,
                            edx: 13,
                        };
                        let output = leaf1_for_cr4(input, cr4);
                        assert_eq!(output.ecx & CPUID_OSXSAVE != 0, expected != 0);
                        assert_eq!(output.ecx & !CPUID_OSXSAVE, input.ecx & !CPUID_OSXSAVE);
                        assert_eq!((output.eax, output.ebx, output.edx), (7, 11, 13));
                        let absent = leaf1_for_cr4(CpuidResult { ecx: 0, ..input }, cr4);
                        assert_eq!(absent.ecx, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn xsetbv_fault_priority_index_privilege_and_reserved_bits() {
        for (xsave, cr4) in [(false, CR4_OSXSAVE), (true, 0)] {
            assert_eq!(
                validate_xsetbv(xsave, cr4, 3, 1, 0, 7),
                Err(XsetbvFault::InvalidOpcode)
            );
        }
        for (cpl, index, value) in [
            (3, 0, 3),
            (0, 1, 3),
            (0, u32::MAX, 3),
            (0, 0, 0),
            (0, 0, 5),
            (0, 0, 1 << 63 | 3),
        ] {
            assert_eq!(
                validate_xsetbv(true, CR4_OSXSAVE, cpl, index, value, 7),
                Err(XsetbvFault::GeneralProtection)
            );
        }
        for value in [1, 3, 7] {
            assert_eq!(validate_xsetbv(true, CR4_OSXSAVE, 0, 0, value, 7), Ok(()));
        }
    }

    #[test]
    fn xsetbv_checks_all_exposed_component_dependencies() {
        let supported = 0x2ff | (3 << 17);
        for group in [3 << 3, 7 << 5, 3 << 17] {
            for bit in 0..19 {
                if group & (1 << bit) != 0 {
                    assert_eq!(
                        validate_xsetbv(true, CR4_OSXSAVE, 0, 0, 7 | (1 << bit), supported),
                        Err(XsetbvFault::GeneralProtection)
                    );
                }
            }
            assert_eq!(
                validate_xsetbv(true, CR4_OSXSAVE, 0, 0, 7 | group, supported),
                Ok(())
            );
        }
        assert_eq!(
            validate_xsetbv(true, CR4_OSXSAVE, 0, 0, 3 | (7 << 5), supported),
            Err(XsetbvFault::GeneralProtection)
        );
        assert_eq!(
            validate_xsetbv(true, CR4_OSXSAVE, 0, 0, 1 | (1 << 9), supported),
            Ok(())
        );
    }
}
