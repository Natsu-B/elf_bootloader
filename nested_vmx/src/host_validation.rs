//! Checks on original L1 host fields hidden by Direct-VMCS patching.
//!
//! Intel SDM VM-entry host checks deliberately do not validate HOST_RSP, nor
//! selector RPL/TI in the 32-bit SYSENTER_CS MSR field. Do not invent additional
//! VMfail conditions for them. Descriptor/MSR bases use the CPU's maximum
//! canonical width; HOST_RIP uses the host CR4.LA57 value loaded on VM exit.

use crate::VmcsField;
use x86_64_hal::paging::is_canonical;

const EXIT_HOST_64: u64 = 1 << 9;
const EXIT_LOAD_PAT: u64 = 1 << 19;
const EXIT_LOAD_EFER: u64 = 1 << 21;
const EXIT_LOAD_CET: u64 = 1 << 28;

/// Architectural limits of the CPU executing the nested VM-entry instruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Required-one CR0 bits in VMX operation.
    pub cr0_fixed0: u64,
    /// Allowed-one CR0 bits in VMX operation.
    pub cr0_fixed1: u64,
    /// Required-one CR4 bits in VMX operation.
    pub cr4_fixed0: u64,
    /// Allowed-one CR4 bits in VMX operation.
    pub cr4_fixed1: u64,
    /// CPUID physical-address width.
    pub physical_bits: u8,
    /// CPU maximum canonical-address width, independent of current CR4.LA57.
    pub linear_bits: u8,
    /// LAM permits CR3 bits 62:61 in both host and guest VMCS state.
    pub lam: bool,
    /// Non-reserved Intel EFER bits for the exposed CPU.
    pub efer_allowed: u64,
    /// Non-reserved S_CET bits, including CPUID SHSTK/IBT availability.
    pub cet_allowed: u64,
    /// L1's EFER.LMA at the time it executes VM entry.
    pub l1_ia32e: bool,
}

/// Why pre-patch validation failed. Missing fields/limits are L0 invariants,
/// unlike an architecturally invalid L1 host value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Invalid CPU limits supplied by the monitor.
    Limits,
    /// A required field was absent from the saved patch manifest.
    Missing(VmcsField),
    /// The host address-space-size control disagrees with L1's current mode.
    AddressSpaceSize,
    /// Original L1 field fails an architectural host-state check.
    Field(VmcsField),
}

/// Validates host values before L0 hides them. Other VM-entry checks, including
/// controls, launch state and unpatched PERF_GLOBAL_CTRL, remain hardware-owned.
/// The caller must preserve their priority when recording a failure: do not
/// blindly publish error 8 before hardware checks an invalid control or launch.
pub fn validate(
    limits: Limits,
    exit_controls: u64,
    mut read: impl FnMut(VmcsField) -> Option<u64>,
) -> Result<(), Error> {
    use VmcsField::*;
    if !(12..=52).contains(&limits.physical_bits)
        || !matches!(limits.linear_bits, 48 | 57)
        || limits.cr0_fixed0 & !limits.cr0_fixed1 != 0
        || limits.cr4_fixed0 & !limits.cr4_fixed1 != 0
    {
        return Err(Error::Limits);
    }
    let mut get = |field| read(field).ok_or(Error::Missing(field));
    let host64 = exit_controls & EXIT_HOST_64 != 0;
    if host64 != limits.l1_ia32e {
        return Err(Error::AddressSpaceSize);
    }
    let cr0 = get(HostCr0)?;
    let cr4 = get(HostCr4)?;
    // VM exit does not switch CD/NW, and VM entry never checks these two bits
    // in HOST_CR0. Unrestricted-guest PE/PG relaxations do not apply to hosts.
    let cr0_checked = !((1 << 29) | (1 << 30));
    if (cr0 & limits.cr0_fixed0 & cr0_checked) != (limits.cr0_fixed0 & cr0_checked)
        || cr0 & !limits.cr0_fixed1 & cr0_checked != 0
        || (cr4 & (1 << 23) != 0 && cr0 & (1 << 16) == 0)
    {
        return Err(Error::Field(HostCr0));
    }
    if cr4 & limits.cr4_fixed0 != limits.cr4_fixed0
        || cr4 & !limits.cr4_fixed1 != 0
        || (host64 && cr4 & (1 << 5) == 0)
        || (!host64 && cr4 & (1 << 17) != 0)
    {
        return Err(Error::Field(HostCr4));
    }
    let cr3_allowed = ((1_u64 << limits.physical_bits) - 1) | if limits.lam { 3 << 61 } else { 0 };
    if get(HostCr3)? & !cr3_allowed != 0 {
        return Err(Error::Field(HostCr3));
    }
    for field in [
        HostCsSelector,
        HostSsSelector,
        HostDsSelector,
        HostEsSelector,
        HostFsSelector,
        HostGsSelector,
        HostTrSelector,
    ] {
        let selector = get(field)?;
        if selector & 7 != 0
            || ((field == HostCsSelector
                || field == HostTrSelector
                || (field == HostSsSelector && !host64))
                && selector == 0)
        {
            return Err(Error::Field(field));
        }
    }
    for field in [
        HostFsBase,
        HostGsBase,
        HostGdtrBase,
        HostIdtrBase,
        HostTrBase,
        HostIa32SysenterEsp,
        HostIa32SysenterEip,
    ] {
        if !is_canonical(get(field)?, limits.linear_bits) {
            return Err(Error::Field(field));
        }
    }
    let rip = get(HostRip)?;
    if if host64 {
        !is_canonical(rip, if cr4 & (1 << 12) != 0 { 57 } else { 48 })
    } else {
        rip >> 32 != 0
    } {
        return Err(Error::Field(HostRip));
    }
    if exit_controls & EXIT_LOAD_PAT != 0 {
        let pat = get(HostIa32Pat)?;
        for index in 0..8 {
            if !matches!((pat >> (index * 8)) & 255, 0 | 1 | 4 | 5 | 6 | 7) {
                return Err(Error::Field(HostIa32Pat));
            }
        }
    }
    if exit_controls & EXIT_LOAD_EFER != 0 {
        let efer = get(HostIa32Efer)?;
        if efer & !limits.efer_allowed != 0
            || (efer & (1 << 8) != 0) != host64
            || (efer & (1 << 10) != 0) != host64
        {
            return Err(Error::Field(HostIa32Efer));
        }
    }
    if exit_controls & EXIT_LOAD_CET != 0 {
        let cet = get(HostSCet)?;
        let ssp = get(HostSsp)?;
        if cet & !limits.cet_allowed != 0
            || cet & (3 << 10) == (3 << 10)
            || (if host64 {
                !is_canonical(cet, limits.linear_bits)
            } else {
                cet >> 32 != 0
            })
        {
            return Err(Error::Field(HostSCet));
        }
        if ssp & 3 != 0
            || (if host64 {
                !is_canonical(ssp, limits.linear_bits)
            } else {
                ssp >> 32 != 0
            })
        {
            return Err(Error::Field(HostSsp));
        }
        if !is_canonical(get(HostInterruptSspTable)?, limits.linear_bits) {
            return Err(Error::Field(HostInterruptSspTable));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DIRECT_VMCS_PATCH_MANIFEST;
    use VmcsField::*;

    fn limits() -> Limits {
        Limits {
            cr0_fixed0: (1 << 31) | 1,
            cr0_fixed1: 0xffff_ffff,
            cr4_fixed0: 1 << 13,
            cr4_fixed1: 0x01ff_ffff,
            physical_bits: 46,
            linear_bits: 57,
            lam: false,
            efer_allowed: 0xd01,
            cet_allowed: !0x3c0,
            l1_ia32e: true,
        }
    }

    fn good(field: VmcsField) -> u64 {
        match field {
            HostCr0 => (1 << 31) | (1 << 16) | 1,
            HostCr3 => 0x1000,
            HostCr4 => (1 << 13) | (1 << 5),
            HostCsSelector => 8,
            HostTrSelector => 24,
            HostIa32Pat => 0x0007_0406_0007_0406,
            HostIa32Efer => 0xd01,
            _ => 0,
        }
    }

    fn changed(limits: Limits, controls: u64, field: VmcsField, value: u64) -> Result<(), Error> {
        validate(limits, controls, |current| {
            Some(if current == field {
                value
            } else {
                good(current)
            })
        })
    }

    #[test]
    fn selector_rpl_ti_and_null_constraints_are_not_hidden() {
        for field in [
            HostCsSelector,
            HostSsSelector,
            HostDsSelector,
            HostEsSelector,
            HostFsSelector,
            HostGsSelector,
            HostTrSelector,
        ] {
            for bad in [1, 2, 3, 4, 7, 0xffff] {
                assert_eq!(
                    changed(limits(), EXIT_HOST_64, field, bad),
                    Err(Error::Field(field))
                );
            }
            assert_eq!(
                changed(limits(), EXIT_HOST_64, field, 0),
                if matches!(field, HostCsSelector | HostTrSelector) {
                    Err(Error::Field(field))
                } else {
                    Ok(())
                }
            );
        }
    }

    #[test]
    fn fixed_bits_and_host_cet_wp_are_checked_without_guest_relaxations() {
        for (field, bad) in [
            (HostCr0, 1),
            (HostCr0, 1 << 31),
            (HostCr0, u64::MAX),
            (HostCr4, 1 << 5),
            (HostCr4, 1 << 13),
            (HostCr4, u64::MAX),
        ] {
            assert_eq!(
                changed(limits(), EXIT_HOST_64, field, bad),
                Err(Error::Field(field))
            );
        }
        assert_eq!(
            validate(limits(), EXIT_HOST_64, |field| Some(match field {
                HostCr0 => good(field) & !(1 << 16),
                HostCr4 => good(field) | (1 << 23),
                _ => good(field),
            })),
            Err(Error::Field(HostCr0))
        );
        let mut cpu = limits();
        cpu.cr0_fixed1 &= !((1 << 29) | (1 << 30));
        assert_eq!(
            changed(cpu, EXIT_HOST_64, HostCr0, good(HostCr0) | (3 << 29)),
            Ok(())
        );
    }

    #[test]
    fn cr3_width_lam_and_no_flush_bit_follow_architecture() {
        for bits in [32, 46, 52] {
            let mut cpu = limits();
            cpu.physical_bits = bits;
            assert_eq!(changed(cpu, EXIT_HOST_64, HostCr3, (1 << bits) - 1), Ok(()));
            for bit in [bits, 60, 61, 62, 63] {
                assert_eq!(
                    changed(cpu, EXIT_HOST_64, HostCr3, 1 << bit),
                    Err(Error::Field(HostCr3))
                );
            }
            cpu.lam = true;
            for value in [1 << 61, 1 << 62, 3 << 61] {
                assert_eq!(changed(cpu, EXIT_HOST_64, HostCr3, 0x1234 | value), Ok(()));
            }
            assert_eq!(
                changed(cpu, EXIT_HOST_64, HostCr3, 1 << 63),
                Err(Error::Field(HostCr3))
            );
        }
    }

    #[test]
    fn bases_and_sysenter_use_static_width_but_rip_uses_loaded_la57() {
        for field in [
            HostFsBase,
            HostGsBase,
            HostGdtrBase,
            HostIdtrBase,
            HostTrBase,
            HostIa32SysenterEsp,
            HostIa32SysenterEip,
        ] {
            assert_eq!(changed(limits(), EXIT_HOST_64, field, 1 << 48), Ok(()));
            assert_eq!(
                changed(limits(), EXIT_HOST_64, field, 1 << 57),
                Err(Error::Field(field))
            );
            let mut cpu = limits();
            cpu.linear_bits = 48;
            assert_eq!(
                changed(cpu, EXIT_HOST_64, field, 1 << 48),
                Err(Error::Field(field))
            );
        }
        assert_eq!(
            changed(limits(), EXIT_HOST_64, HostRip, 1 << 48),
            Err(Error::Field(HostRip))
        );
        assert_eq!(
            validate(limits(), EXIT_HOST_64, |field| Some(match field {
                HostRip => 1 << 48,
                HostCr4 => good(field) | (1 << 12),
                _ => good(field),
            })),
            Ok(())
        );
    }

    #[test]
    fn pat_and_efer_checks_follow_exit_load_controls() {
        for byte in 0..=255 {
            let valid = matches!(byte, 0 | 1 | 4 | 5 | 6 | 7);
            for index in 0..8 {
                let value = (good(HostIa32Pat) & !(255 << (index * 8))) | (byte << (index * 8));
                assert_eq!(
                    changed(limits(), EXIT_HOST_64 | EXIT_LOAD_PAT, HostIa32Pat, value),
                    if valid {
                        Ok(())
                    } else {
                        Err(Error::Field(HostIa32Pat))
                    }
                );
            }
        }
        assert_eq!(
            changed(limits(), EXIT_HOST_64, HostIa32Pat, u64::MAX),
            Ok(())
        );
        assert_eq!(
            changed(limits(), EXIT_HOST_64, HostIa32Efer, u64::MAX),
            Ok(())
        );
        for value in [0, 1 << 8, 1 << 10, 0xd03, u64::MAX] {
            assert_eq!(
                changed(limits(), EXIT_HOST_64 | EXIT_LOAD_EFER, HostIa32Efer, value),
                Err(Error::Field(HostIa32Efer))
            );
        }
        assert_eq!(
            validate(
                limits(),
                EXIT_HOST_64 | EXIT_LOAD_EFER | EXIT_LOAD_PAT,
                |field| Some(good(field))
            ),
            Ok(())
        );
    }

    #[test]
    fn mode_mismatch_and_legacy_ss_pcide_rip_constraints() {
        assert_eq!(
            validate(limits(), 0, |field| Some(good(field))),
            Err(Error::AddressSpaceSize)
        );
        let mut cpu = limits();
        cpu.l1_ia32e = false;
        let legacy = |field| {
            Some(if field == HostSsSelector {
                16
            } else {
                good(field)
            })
        };
        assert_eq!(validate(cpu, 0, legacy), Ok(()));
        assert_eq!(
            validate(cpu, 0, |field| Some(good(field))),
            Err(Error::Field(HostSsSelector))
        );
        for (bad_field, value) in [(HostCr4, good(HostCr4) | (1 << 17)), (HostRip, 1 << 32)] {
            assert_eq!(
                validate(cpu, 0, |field| if field == bad_field {
                    Some(value)
                } else {
                    legacy(field)
                }),
                Err(Error::Field(bad_field))
            );
        }
    }

    #[test]
    fn no_invented_rsp_or_sysenter_selector_failures_and_manifest_is_complete() {
        assert_eq!(changed(limits(), EXIT_HOST_64, HostRsp, 1 << 57), Ok(()));
        assert_eq!(
            changed(limits(), EXIT_HOST_64, HostIa32SysenterCs, 0xffff_ffff),
            Ok(())
        );
        assert_eq!(
            validate(
                limits(),
                EXIT_HOST_64 | EXIT_LOAD_PAT | EXIT_LOAD_EFER | EXIT_LOAD_CET,
                |field| {
                    assert!(
                        DIRECT_VMCS_PATCH_MANIFEST
                            .iter()
                            .any(|patch| patch.field == field)
                    );
                    Some(good(field))
                }
            ),
            Ok(())
        );
        assert_eq!(
            validate(limits(), EXIT_HOST_64, |_| None),
            Err(Error::Missing(HostCr0))
        );
        let mut cpu = limits();
        cpu.physical_bits = 64;
        assert_eq!(
            validate(cpu, EXIT_HOST_64, |_| panic!(
                "invalid limits must not read fields"
            )),
            Err(Error::Limits)
        );
    }

    #[test]
    fn conditional_cet_checks_do_not_enable_cet_capabilities() {
        let controls = EXIT_HOST_64 | EXIT_LOAD_CET;
        for (field, value) in [
            (HostSCet, 1 << 6),
            (HostSCet, 3 << 10),
            (HostSCet, 1 << 57),
            (HostSsp, 1),
            (HostSsp, 1 << 57),
            (HostInterruptSspTable, 1 << 57),
        ] {
            assert_eq!(
                changed(limits(), controls, field, value),
                Err(Error::Field(field))
            );
            assert_eq!(changed(limits(), EXIT_HOST_64, field, value), Ok(()));
        }
    }
}
