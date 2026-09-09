//! Read-only exit information retained while the hardware direct VMCS is idle.
//!
//! This is not guest/host VMCS composition: hardware remains authoritative.
//! L1 must not be allowed to write exit information (VMX_MISC[29] is hidden).
//! Drop the snapshot before any VM entry, VMCLEAR, VMCS switch or VMX lifetime
//! transition. Never cache VM_INSTRUCTION_ERROR: later VMfail changes it.

use x86_64_hal::addr::VmcsPhys;
use x86_64_hal::vmcs;

/// Mandatory VMX exit information plus the GPA field of the required EPT CPU.
const FIELDS: [u32; 10] = [
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
];

/// One completed hardware snapshot belonging to one CPU-owned direct VMCS.
pub struct ExitSnapshot {
    owner: VmcsPhys,
    values: [u64; FIELDS.len()],
}

impl ExitSnapshot {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_immutable_exit_fields_and_exact_width_aliases_are_retained() {
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
            assert_eq!((field >> 10) & 3, 1);
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
            vmcs::GUEST_RIP,
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
