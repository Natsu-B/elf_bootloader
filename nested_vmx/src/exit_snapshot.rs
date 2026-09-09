//! Hardware exit and selected guest fields retained while a direct VMCS is idle.
//!
//! This is not guest/host VMCS composition: hardware remains authoritative.
//! L1 must not be allowed to write exit information (VMX_MISC[29] is hidden).
//! Drop the snapshot before any VM entry, VMCLEAR, VMCS switch or VMX lifetime
//! transition. Never cache VM_INSTRUCTION_ERROR: later VMfail changes it.
//! Changed guest fields are write-through, after hardware VMWRITE succeeds.
//! Exact idempotent guest writes need no hardware change. This never defers a
//! changed hardware write or constructs a software entry VMCS.

use x86_64_hal::addr::VmcsPhys;
use x86_64_hal::vmcs;

/// Mandatory exit information and the four guest fields dominating measured
/// Hyper-V VMREAD misses. Capturing them while the direct VMCS is current avoids
/// switching it back in for each L1 read. No field changes while L2 is stopped
/// except through explicit, hardware-validated VMWRITE.
const FIELDS: [u32; 14] = [
    vmcs::VM_EXIT_REASON,
    vmcs::VM_EXIT_INTR_INFO,
    vmcs::VM_EXIT_INTR_ERROR_CODE,
    vmcs::IDT_VECTORING_INFO_FIELD,
    vmcs::IDT_VECTORING_ERROR_CODE,
    vmcs::VM_EXIT_INSTRUCTION_LEN,
    vmcs::VMX_INSTRUCTION_INFO,
    vmcs::EXIT_QUALIFICATION,
    vmcs::GUEST_LINEAR_ADDRESS,
    vmcs::GUEST_PHYSICAL_ADDRESS,
    vmcs::GUEST_RIP,
    vmcs::GUEST_RFLAGS,
    vmcs::GUEST_CS_AR_BYTES,
    vmcs::GUEST_INTERRUPTIBILITY_INFO,
];

/// One completed hardware snapshot belonging to one CPU-owned direct VMCS.
pub struct ExitSnapshot {
    owner: VmcsPhys,
    values: [u64; FIELDS.len()],
}

impl ExitSnapshot {
    /// Width of the four mandatory writable fields retained from hardware.
    fn guest_value(field: u32, value: u64) -> Option<u64> {
        match field {
            vmcs::GUEST_RIP | vmcs::GUEST_RFLAGS => Some(value),
            vmcs::GUEST_CS_AR_BYTES | vmcs::GUEST_INTERRUPTIBILITY_INFO => {
                Some(u64::from(value as u32))
            }
            _ => None,
        }
    }

    /// Capture while `owner` is current and stopped after a real VM exit.
    /// A failed read never publishes a partial snapshot. Undefined exit-field
    /// contents are retained verbatim, not synthesized as meaningful values.
    pub fn capture(owner: VmcsPhys, mut read: impl FnMut(u32) -> Option<u64>) -> Option<Self> {
        let mut values = [0; FIELDS.len()];
        for (value, field) in values.iter_mut().zip(FIELDS) {
            *value = read(field)?;
        }
        Some(Self { owner, values })
    }

    /// Read only a supported exact encoding for the original owner. The high
    /// alias exists only for the 64-bit GPA, not natural/32-bit components.
    #[must_use]
    pub fn read(&self, owner: VmcsPhys, field: u32) -> Option<u64> {
        if self.owner != owner {
            return None;
        }
        let (field, shift) = if field == vmcs::GUEST_PHYSICAL_ADDRESS + 1 {
            (vmcs::GUEST_PHYSICAL_ADDRESS, 32)
        } else {
            (field, 0)
        };
        FIELDS
            .iter()
            .position(|&candidate| candidate == field)
            .map(|index| self.values[index] >> shift)
    }

    /// Whether an exact mandatory writable field already has the requested
    /// value in the stopped hardware VMCS. The caller must first validate VMX
    /// operation, CPL, the current pointer and the complete memory operand.
    /// VMWRITE checks field existence/access, not VM-entry validity of its value;
    /// VMsucceed must still update flags without clearing VM_INSTRUCTION_ERROR.
    #[must_use]
    pub fn write_is_redundant(&self, owner: VmcsPhys, field: u32, value: u64) -> bool {
        Self::guest_value(field, value).is_some_and(|value| self.read(owner, field) == Some(value))
    }

    /// Update a retained guest field only after a successful VMWRITE.
    /// Failure, a foreign VMCS, read-only information, and unsupported aliases
    /// must not modify the snapshot. Width truncation matches x86-64 VMWRITE;
    /// entry validity checks still belong to the actual hardware VMCS.
    pub fn written(&mut self, owner: VmcsPhys, field: u32, value: u64, succeeded: bool) {
        if !succeeded || owner != self.owner {
            return;
        }
        let Some(value) = Self::guest_value(field, value) else {
            return;
        };
        if let Some(index) = FIELDS.iter().position(|&candidate| candidate == field) {
            self.values[index] = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_selected_hardware_fields_and_exact_width_aliases_are_retained() {
        assert_eq!(
            crate::restrict_vmx_capability(x86_64_hal::vmx::IA32_VMX_MISC, u64::MAX).unwrap()
                & (1 << 29),
            0,
            "snapshot validity requires read-only exit information"
        );
        let owner = VmcsPhys::new(0x1000).unwrap();
        let snapshot =
            ExitSnapshot::capture(owner, |field| Some(u64::from(field) << 32 | 7)).unwrap();
        for field in FIELDS {
            assert!(matches!((field >> 10) & 3, 1 | 2));
            assert!(
                !crate::DIRECT_VMCS_PATCH_MANIFEST
                    .iter()
                    .any(|patch| patch.field as u32 == field)
            );
            assert_eq!(
                snapshot.read(owner, field),
                Some(u64::from(field) << 32 | 7)
            );
            if field != vmcs::GUEST_PHYSICAL_ADDRESS {
                assert_eq!(snapshot.read(owner, field + 1), None);
            }
        }
        assert_eq!(
            snapshot.read(owner, vmcs::GUEST_PHYSICAL_ADDRESS + 1),
            Some(0x2400)
        );
        for field in [
            vmcs::VM_INSTRUCTION_ERROR,
            vmcs::GUEST_RSP,
            vmcs::HOST_RIP,
            0x8000,
            u32::MAX,
        ] {
            assert_eq!(snapshot.read(owner, field), None);
        }
        assert_eq!(
            snapshot.read(VmcsPhys::new(0x2000).unwrap(), vmcs::VM_EXIT_REASON),
            None
        );
    }

    #[test]
    fn guest_writes_require_hardware_success_owner_and_exact_width() {
        let owner = VmcsPhys::new(0x1000).unwrap();
        let foreign = VmcsPhys::new(0x2000).unwrap();
        let mut snapshot = ExitSnapshot::capture(owner, |_| Some(7)).unwrap();
        for field in FIELDS {
            let expected = match field {
                vmcs::GUEST_RIP | vmcs::GUEST_RFLAGS => u64::MAX,
                vmcs::GUEST_CS_AR_BYTES | vmcs::GUEST_INTERRUPTIBILITY_INFO => u32::MAX.into(),
                _ => 7,
            };
            snapshot.written(owner, field, u64::MAX, false);
            snapshot.written(foreign, field, u64::MAX, true);
            snapshot.written(owner, field + 1, u64::MAX, true);
            assert_eq!(snapshot.read(owner, field), Some(7));
            snapshot.written(owner, field, u64::MAX, true);
            assert_eq!(snapshot.read(owner, field), Some(expected));
        }
        // A new hardware exit changes guest state independently of L1 writes.
        let snapshot = ExitSnapshot::capture(owner, |_| Some(19)).unwrap();
        assert_eq!(snapshot.read(owner, vmcs::GUEST_RIP), Some(19));
    }

    #[test]
    fn only_exact_idle_guest_values_can_elide_a_write() {
        let owner = VmcsPhys::new(0x1000).unwrap();
        let foreign = VmcsPhys::new(0x2000).unwrap();
        let mut snapshot = ExitSnapshot::capture(owner, |_| Some(7)).unwrap();
        for field in FIELDS {
            let writable = matches!(
                field,
                vmcs::GUEST_RIP
                    | vmcs::GUEST_RFLAGS
                    | vmcs::GUEST_CS_AR_BYTES
                    | vmcs::GUEST_INTERRUPTIBILITY_INFO
            );
            assert_eq!(snapshot.write_is_redundant(owner, field, 7), writable);
            assert!(!snapshot.write_is_redundant(foreign, field, 7));
            assert!(!snapshot.write_is_redundant(owner, field + 1, 7));
            assert!(!snapshot.write_is_redundant(owner, field, 8));
            let narrow = matches!(
                field,
                vmcs::GUEST_CS_AR_BYTES | vmcs::GUEST_INTERRUPTIBILITY_INFO
            );
            assert_eq!(
                snapshot.write_is_redundant(owner, field, 0xffff_ffff_0000_0007),
                narrow
            );
            snapshot.written(owner, field, 8, false);
            assert_eq!(snapshot.write_is_redundant(owner, field, 7), writable);
            snapshot.written(owner, field, 8, true);
            assert_eq!(snapshot.write_is_redundant(owner, field, 8), writable);
            assert!(!snapshot.write_is_redundant(owner, field, 7));
        }
        for field in [vmcs::HOST_RIP, vmcs::VM_INSTRUCTION_ERROR, 0x8000, u32::MAX] {
            assert!(!snapshot.write_is_redundant(owner, field, 7));
        }
    }

    #[test]
    fn failure_cannot_publish_partial_values_or_reuse_an_old_generation() {
        let owner = VmcsPhys::new(0x1000).unwrap();
        for fail_at in 0..FIELDS.len() {
            let mut reads = 0;
            let snapshot = ExitSnapshot::capture(owner, |_| {
                reads += 1;
                (reads - 1 != fail_at).then_some(18)
            });
            assert!(snapshot.is_none());
            assert_eq!(reads, fail_at + 1);
        }
        let mut active = ExitSnapshot::capture(owner, |_| Some(18));
        assert_eq!(
            active.as_ref().unwrap().read(owner, vmcs::VM_EXIT_REASON),
            Some(18)
        );
        // Runtime invalidates before an entry/clear/switch/lifetime transition.
        assert!(active.take().is_some());
        assert!(active.is_none());
        active = ExitSnapshot::capture(owner, |_| Some(48));
        assert_eq!(
            active.as_ref().unwrap().read(owner, vmcs::VM_EXIT_REASON),
            Some(48)
        );
    }
}
