//! Architectural CR0 writes when VMX forces a private hardware NE bit.
//!
//! Intel SDM Vol. 2 MOV-to-control-register exceptions and Vol. 3A 5.4.1
//! (PAE PDPTE loading). These helpers do not execute a control-register write.

/// Numeric-error delivery must remain enabled in the hardware VMX guest CR0,
/// even when a resetting AP has not enabled it in its visible CR0 yet.
pub const CR0_NE: u64 = 1 << 5;
const PE: u64 = 1;
const ET: u64 = 1 << 4;
const WP: u64 = 1 << 16;
const NW: u64 = 1 << 29;
const CD: u64 = 1 << 30;
const PG: u64 = 1 << 31;
const PAE: u64 = 1 << 5;
const PCIDE: u64 = 1 << 17;
const CET: u64 = 1 << 23;
const LME: u64 = 1 << 8;
const LMA: u64 = 1 << 10;
const DEFINED: u64 = 0x3f | WP | (1 << 18) | NW | CD | PG;

/// A complete validated update; callers publish nothing if validation fails.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cr0Write {
    /// L1-visible CR0, including hardwired ET and ignored reserved bits.
    pub visible: u64,
    /// EFER with LMA reflecting the new paging mode, preserving other bits.
    pub efer: u64,
    /// Legacy PAE PDPTE registers must be checked/loaded before committing.
    pub load_pdpt: bool,
}

/// Validates MOV-to-CR0. `None` means #GP(0), with the old state unchanged.
/// Supply fixed-bit limits only when L1 itself is in VMX operation; unrestricted
/// guest relaxations belong to L0's carrier, not to L1's VMX-root contract.
#[must_use]
pub fn cr0_write(
    old: u64,
    requested: u64,
    cr4: u64,
    efer: u64,
    cs_long: bool,
    vmx_fixed: Option<(u64, u64)>,
) -> Option<Cr0Write> {
    if requested >> 32 != 0 {
        return None;
    }
    let value = (requested | ET) & DEFINED;
    if (value & (NW | CD) == NW)
        || (value & (PG | PE) == PG)
        || (value & PG == 0 && ((cs_long && efer & LMA != 0) || cr4 & PCIDE != 0))
        || (value & WP == 0 && cr4 & CET != 0)
        || (efer & LME != 0 && old & PG == 0 && value & PG != 0 && (cr4 & PAE == 0 || cs_long))
        || vmx_fixed.is_some_and(|(required, allowed)| {
            value & required != required || value & !allowed != 0
        })
    {
        return None;
    }
    let efer = (efer & !LMA)
        | if value & PG != 0 && efer & LME != 0 {
            LMA
        } else {
            0
        };
    Some(Cr0Write {
        visible: value,
        efer,
        load_pdpt: value & PG != 0
            && cr4 & PAE != 0
            && efer & LMA == 0
            && (old ^ value) & (PG | CD | NW) != 0,
    })
}

/// PAE uses CR3[31:5], unlike IA-32e. All four entries must be captured and
/// validated before changing any VMCS field. NX is reserved in these PDPTEs.
#[must_use]
pub const fn pae_pdpt_base(cr3: u64) -> u64 {
    cr3 & 0xffff_ffe0
}

/// Checks a cached legacy PAE PDPTE against MAXPHYADDR, not a long-mode PDPTE.
#[must_use]
pub fn valid_pae_pdpte(value: u64, physical_bits: u8) -> bool {
    (12..=52).contains(&physical_bits)
        && (value & 1 == 0 || value & (!((1u64 << physical_bits) - 1) | 0x1e6) == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ne_virtualization_keeps_reset_state_and_long_mode_transitions_architectural() {
        for ne in [0, CR0_NE] {
            let reset = cr0_write(0x60000010, 0x60000010 | ne, 0, 0, false, None).unwrap();
            assert_eq!(reset.visible, 0x60000010 | ne);
            assert_eq!(reset.efer, 0);
            assert!(!reset.load_pdpt);
            let paging = cr0_write(PE | ET, PE | ET | PG | ne, PAE, LME, false, None).unwrap();
            assert_eq!(paging.efer, LME | LMA);
            assert!(!paging.load_pdpt);
            let disabled =
                cr0_write(paging.visible, PE | ET | ne, PAE, paging.efer, false, None).unwrap();
            assert_eq!(disabled.efer, LME);
        }
        assert_eq!(
            cr0_write(ET, 0xffff, 0, 0, false, None).unwrap().visible,
            0x3f
        );
        assert!(cr0_write(ET, 0, 0, 0, false, None).is_some());
        for (old, new, cr4, efer, cs_long) in [
            (ET, 1 << 63, 0, 0, false),
            (ET, NW, 0, 0, false),
            (ET, PG, 0, 0, false),
            (ET, PG | PE, 0, LME, false),
            (ET, PG | PE, PAE, LME, true),
            (PG | PE | ET, PE, PAE, LME | LMA, true),
            (PG | PE | ET, PE, PCIDE, 0, false),
            (ET | WP, ET, CET, 0, false),
        ] {
            assert!(cr0_write(old, new, cr4, efer, cs_long, None).is_none());
        }
        assert!(
            cr0_write(
                PG | PE | ET | CR0_NE,
                PG | PE | ET,
                PAE,
                LME | LMA,
                true,
                Some((PG | PE | CR0_NE, u32::MAX as u64))
            )
            .is_none()
        );
    }

    #[test]
    fn legacy_pae_reload_and_reserved_bits_do_not_use_long_mode_rules() {
        let enabled = PG | PE | ET;
        assert!(
            cr0_write(ET | PE, enabled, PAE, 0, false, None)
                .unwrap()
                .load_pdpt
        );
        assert!(
            !cr0_write(enabled, enabled | CR0_NE, PAE, 0, false, None)
                .unwrap()
                .load_pdpt
        );
        assert!(
            cr0_write(enabled, enabled | CD, PAE, 0, false, None)
                .unwrap()
                .load_pdpt
        );
        assert_eq!(pae_pdpt_base(0xffff_ffff_1234_567f), 0x12345660);
        for width in [12, 36, 48, 52] {
            for value in [0, u64::MAX - 1, 1, 0xe19, ((1u64 << width) - 4096) | 1] {
                assert!(valid_pae_pdpte(value, width));
            }
            for bit in [1, 2, 5, 6, 7, 8, 63, width] {
                assert!(!valid_pae_pdpte(1 | (1u64 << bit), width));
            }
        }
        for width in [0, 11, 53, 64, 255] {
            assert!(!valid_pae_pdpte(0, width));
        }
    }
}
