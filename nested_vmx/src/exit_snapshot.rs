//! Hardware exit and selected guest fields retained while a direct VMCS is idle.
//!
//! This is not guest/host VMCS composition: entry still uses L1's hardware VMCS.
//! L1 must not be allowed to write exit information (VMX_MISC[29] is hidden).
//! Four mandatory guest fields can accumulate L1 writes while that VMCS is
//! stopped. Flush them to the same hardware VMCS before entry, VMCLEAR, VMCS
//! selection or VMX lifetime transition, then discard the snapshot. Never cache
//! VM_INSTRUCTION_ERROR: later VMfail changes it. No new entry VMCS is built.

use x86_64_hal::addr::VmcsPhys;
use x86_64_hal::vmcs;

/// Mandatory exit information and the four guest fields dominating measured
/// Hyper-V VMREAD misses. Capturing them while the direct VMCS is current avoids
/// switching it back in for each L1 read. No field changes while L2 is stopped
/// except through explicit VMWRITE. Successful capture proves the four writable
/// encodings exist; VMWRITE does not validate their VM-entry values.
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
    /// Only indices of the four mandatory guest fields can become dirty.
    dirty: u16,
}

const _: () = assert!(FIELDS.len() <= u16::BITS as usize);

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
        Some(Self {
            owner,
            values,
            dirty: 0,
        })
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

    /// Queue an exact mandatory writable field for the stopped hardware VMCS.
    /// Return false without mutation for a foreign owner/encoding. The caller
    /// must first validate VMX operation, CPL, the current pointer and the
    /// complete memory operand.
    /// VMWRITE checks field existence/access, not VM-entry validity of its value;
    /// VMsucceed must still update flags without clearing VM_INSTRUCTION_ERROR.
    #[must_use]
    pub fn queue_write(&mut self, owner: VmcsPhys, field: u32, value: u64) -> bool {
        if owner != self.owner {
            return false;
        }
        let Some(value) = Self::guest_value(field, value) else {
            return false;
        };
        let Some(index) = FIELDS.iter().position(|&candidate| candidate == field) else {
            return false;
        };
        if self.values[index] != value {
            self.values[index] = value;
            self.dirty |= 1 << index;
        }
        true
    }

    /// Exact hardware VMCS to select before materializing pending guest writes.
    #[must_use]
    pub const fn owner(&self) -> VmcsPhys {
        self.owner
    }

    /// A clean snapshot can be retired without selecting another hardware VMCS.
    #[must_use]
    pub const fn has_pending_writes(&self) -> bool {
        self.dirty != 0
    }

    /// Flush to the already-current, exclusively owned hardware VMCS. Wrong
    /// ownership or a failed write returns false; failed/unattempted fields stay
    /// dirty. The caller must not enter, clear, release or discard dirty state
    /// after failure. Entry validity remains hardware's responsibility.
    pub fn flush(&mut self, current: VmcsPhys, mut write: impl FnMut(u32, u64) -> bool) -> bool {
        if current != self.owner {
            return false;
        }
        for (index, field) in FIELDS.into_iter().enumerate() {
            let bit = 1 << index;
            if self.dirty & bit == 0 {
                continue;
            }
            if !write(field, self.values[index]) {
                return false;
            }
            self.dirty &= !bit;
        }
        true
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
    fn queued_guest_writes_require_owner_and_exact_width() {
        let owner = VmcsPhys::new(0x1000).unwrap();
        let foreign = VmcsPhys::new(0x2000).unwrap();
        let mut snapshot = ExitSnapshot::capture(owner, |_| Some(7)).unwrap();
        for field in FIELDS {
            let expected = match field {
                vmcs::GUEST_RIP | vmcs::GUEST_RFLAGS => u64::MAX,
                vmcs::GUEST_CS_AR_BYTES | vmcs::GUEST_INTERRUPTIBILITY_INFO => u32::MAX.into(),
                _ => 7,
            };
            assert!(!snapshot.queue_write(foreign, field, u64::MAX));
            assert!(!snapshot.queue_write(owner, field + 1, u64::MAX));
            assert_eq!(snapshot.read(owner, field), Some(7));
            assert_eq!(snapshot.queue_write(owner, field, u64::MAX), expected != 7);
            assert_eq!(snapshot.read(owner, field), Some(expected));
        }
        // A new hardware exit changes guest state independently of L1 writes.
        let snapshot = ExitSnapshot::capture(owner, |_| Some(19)).unwrap();
        assert_eq!(snapshot.read(owner, vmcs::GUEST_RIP), Some(19));
    }

    #[test]
    fn identical_writes_stay_clean_and_changes_flush_once_with_exact_width() {
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
            assert_eq!(snapshot.queue_write(owner, field, 7), writable);
            assert!(!snapshot.queue_write(foreign, field, 7));
            assert!(!snapshot.queue_write(owner, field + 1, 7));
            assert!(!snapshot.has_pending_writes());
            let narrow = matches!(
                field,
                vmcs::GUEST_CS_AR_BYTES | vmcs::GUEST_INTERRUPTIBILITY_INFO
            );
            assert_eq!(
                snapshot.queue_write(owner, field, 0xffff_ffff_0000_0007),
                writable
            );
            assert_eq!(snapshot.has_pending_writes(), writable && !narrow);
            assert_eq!(snapshot.queue_write(owner, field, 8), writable);
            assert_eq!(snapshot.queue_write(owner, field, 8), writable);
            let mut writes = 0;
            assert!(snapshot.flush(owner, |actual_field, value| {
                assert_eq!((actual_field, value), (field, 8));
                writes += 1;
                true
            }));
            assert_eq!(writes, usize::from(writable));
            assert!(!snapshot.has_pending_writes());
            assert!(snapshot.flush(owner, |_, _| panic!("clean field written twice")));
        }
        for field in [vmcs::HOST_RIP, vmcs::VM_INSTRUCTION_ERROR, 0x8000, u32::MAX] {
            assert!(!snapshot.queue_write(owner, field, 7));
        }
    }

    #[test]
    fn failed_flush_retains_each_uncommitted_field_and_never_uses_another_vmcs() {
        let owner = VmcsPhys::new(0x1000).unwrap();
        let foreign = VmcsPhys::new(0x2000).unwrap();
        let fields = &FIELDS[10..];
        for fail_at in 0..fields.len() {
            let mut snapshot = ExitSnapshot::capture(owner, |_| Some(7)).unwrap();
            for &field in fields {
                assert!(snapshot.queue_write(owner, field, 8));
                assert!(snapshot.queue_write(owner, field, 7)); // Back to original is still pending.
            }
            assert_eq!(snapshot.owner(), owner);
            assert!(!snapshot.flush(foreign, |_, _| panic!("foreign VMCS written")));
            let mut attempts = 0;
            assert!(!snapshot.flush(owner, |field, value| {
                assert_eq!((field, value), (fields[attempts], 7));
                attempts += 1;
                attempts - 1 != fail_at
            }));
            assert_eq!(attempts, fail_at + 1);
            assert!(snapshot.has_pending_writes());
            let mut remaining = fail_at;
            assert!(snapshot.flush(owner, |field, value| {
                assert_eq!((field, value), (fields[remaining], 7));
                remaining += 1;
                true
            }));
            assert_eq!(remaining, fields.len());
            assert!(!snapshot.has_pending_writes());
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
