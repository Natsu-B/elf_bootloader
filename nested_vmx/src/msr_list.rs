//! Bounded physical MSR-list formats shared by Direct entry and reflection.
//!
//! Layout failures are VM-entry control failures. Entry contents are checked
//! later, in list order; a failed entry load is not VMfailValid, and a failed
//! exit load/store is a nested VMX abort. The runtime must preserve that order.
//! See Intel SDM Vol. 3C, "Checks on VM-Execution Control Fields", "Loading
//! MSRs" and "Saving MSRs":
//! <https://cdrdv2-public.intel.com/868137/325462-089-sdm-vol-1-2abcd-3abcd-4.pdf>.

use crate::MAX_MSR_MIRROR_ENTRIES;

/// Size and required base alignment of an architectural MSR-list entry.
pub const ENTRY_BYTES: u64 = 16;

/// A list whose full physical range satisfies the advertised VMX contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct List {
    address: u64,
    count: u32,
}

/// Rejected VM-entry control metadata, before any list contents are accessed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The CPU's physical-address width is unavailable or unsupported.
    PhysicalWidth,
    /// Count exceeds the advertised heap-free capacity.
    Count,
    /// A nonempty list's base is not 16-byte aligned.
    Alignment,
    /// The last byte would wrap the physical-address arithmetic.
    Overflow,
    /// At least one byte is outside the CPU's physical-address width.
    Address,
}

impl List {
    /// Validates the complete range, ignoring the address when count is zero.
    /// No pointer is dereferenced, and a valid list may start at physical zero.
    pub const fn new(address: u64, count: u32, physical_bits: u8) -> Result<Self, Error> {
        if physical_bits < 12 || physical_bits > 52 {
            return Err(Error::PhysicalWidth);
        }
        if count > MAX_MSR_MIRROR_ENTRIES {
            return Err(Error::Count);
        }
        if count != 0 {
            if address & (ENTRY_BYTES - 1) != 0 {
                return Err(Error::Alignment);
            }
            let Some(last) = address.checked_add(count as u64 * ENTRY_BYTES - 1) else {
                return Err(Error::Overflow);
            };
            if last >> physical_bits != 0 {
                return Err(Error::Address);
            }
        }
        Ok(Self { address, count })
    }

    /// L1's original base, including ignored bits of an empty list.
    #[must_use]
    pub const fn address(self) -> u64 {
        self.address
    }

    /// Number of validated entries, at most 512.
    #[must_use]
    pub const fn count(self) -> u32 {
        self.count
    }

    /// Checked address of one entry. Empty/out-of-bounds accesses are rejected.
    #[must_use]
    pub const fn entry_address(self, index: u32) -> Option<u64> {
        if index < self.count {
            self.address.checked_add(index as u64 * ENTRY_BYTES)
        } else {
            None
        }
    }
}

/// The three architectural occasions for processing an MSR list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    /// Load L2 guest values after successful control/host/guest-state checks.
    EntryLoad,
    /// Store L2 values before loading L1's host state.
    ExitStore,
    /// Load L1's values after its VMCS host state has been restored.
    ExitLoad,
}

/// Exact hardware entry. Reserved bits are retained, never silently sanitized.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Entry {
    /// RDMSR/WRMSR number.
    pub index: u32,
    /// Must be zero for every list operation.
    pub reserved: u32,
    /// The load value, or captured store result.
    pub value: u64,
}

const _: () = assert!(core::mem::size_of::<Entry>() == ENTRY_BYTES as usize);
const _: () = assert!(core::mem::offset_of!(Entry, value) == 8);

impl Entry {
    /// Creates one reserved-bits-clear entry without asserting MSR support.
    #[must_use]
    pub const fn new(index: u32, value: u64) -> Self {
        Self {
            index,
            reserved: 0,
            value,
        }
    }

    /// Checks architectural exclusions outside SMM. Actual per-CPU MSR access
    /// and value validation remain necessary after this check; success here
    /// does not authorize an unguarded RDMSR/WRMSR in L0.
    #[must_use]
    pub const fn format_valid(self, operation: Operation) -> bool {
        if self.reserved != 0 || self.index >> 8 == 8 {
            return false;
        }
        match operation {
            Operation::EntryLoad | Operation::ExitLoad => {
                // FS/GS bases and the SMM-only monitor-control MSR cannot load.
                self.index != 0xc000_0100 && self.index != 0xc000_0101 && self.index != 0x9b
            }
            // SMBASE cannot be read by a non-SMM VM exit. FS/GS stores are legal.
            Operation::ExitStore => self.index != 0x9e,
        }
    }
}

/// Where to obtain a normal L2 exit's L1-visible MSR-store value. Reading the
/// live root MSR for an automatically switched register would expose L0 state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitStoreSource {
    /// Hardware saved the L2 register into this mandatory guest VMCS field.
    GuestField(u32),
    /// DEBUGCTL is cleared on VM exit, even when SAVE_DEBUG_CONTROLS is clear.
    PrivateDebugCapture,
    /// A requested HOST_PERF_GLOBAL_CTRL load can overwrite the live value.
    PrivatePerfCapture,
    /// Match the capability policy presented to the virtual L1 CPU by RDMSR.
    VmxCapability,
    /// Neither VMX nor L0 changes this MSR; a guarded root read is required.
    Live,
}

/// Classifies value ownership, not access validity. Format checks and guarded
/// RDMSR/model validation remain necessary; late entry failure performs no store.
#[must_use]
pub const fn exit_store_source(index: u32) -> ExitStoreSource {
    use x86_64_hal::vmcs;
    match index {
        0x277 => ExitStoreSource::GuestField(vmcs::GUEST_IA32_PAT),
        0xc000_0080 => ExitStoreSource::GuestField(vmcs::GUEST_IA32_EFER),
        0xc000_0100 => ExitStoreSource::GuestField(vmcs::GUEST_FS_BASE),
        0xc000_0101 => ExitStoreSource::GuestField(vmcs::GUEST_GS_BASE),
        0x174 => ExitStoreSource::GuestField(vmcs::GUEST_SYSENTER_CS),
        0x175 => ExitStoreSource::GuestField(vmcs::GUEST_SYSENTER_ESP),
        0x176 => ExitStoreSource::GuestField(vmcs::GUEST_SYSENTER_EIP),
        0x1d9 => ExitStoreSource::PrivateDebugCapture,
        0x38f => ExitStoreSource::PrivatePerfCapture,
        0x480..=0x492 => ExitStoreSource::VmxCapability,
        _ => ExitStoreSource::Live,
    }
}

/// The two MSRs whose L0-private restoration can hide inherited guest values.
/// These values are architectural images, not an L1/L2 software context switch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PatEfer {
    /// IA32_PAT image.
    pub pat: u64,
    /// IA32_EFER image, including the current LMA bit.
    pub efer: u64,
}

impl PatEfer {
    /// Calculates VM-entry inheritance when L0 has forced private MSR controls.
    /// LOAD_EFER=0 still sets LMA from IA32e guest mode. LME changes with that
    /// mode only when guest CR0.PG=1; paging-disabled guests retain prior LME.
    #[must_use]
    pub const fn entry(self, guest: Self, controls: u32, guest_cr0: u64) -> Self {
        let mut efer = guest.efer;
        if controls & crate::ENTRY_LOAD_IA32_EFER == 0 {
            let ia32e = controls & crate::ENTRY_IA32E_MODE != 0;
            efer = (self.efer & !(1 << 10)) | ((ia32e as u64) << 10);
            if guest_cr0 & (1 << 31) != 0 {
                efer = (efer & !(1 << 8)) | ((ia32e as u64) << 8);
            }
        }
        Self {
            pat: if controls & crate::ENTRY_LOAD_IA32_PAT != 0 {
                guest.pat
            } else {
                self.pat
            },
            efer,
        }
    }

    /// Reconstructs these MSRs after hardware reports entry MSR-load failure.
    /// `self` is the successfully loaded/inherited guest-state image. Hardware
    /// processes the immutable entry mirror in order and identifies the failed
    /// entry with a one-based qualification; only earlier entries took effect.
    /// This must not be used for an invalid-guest-state failure (reason 33),
    /// whose guest-state loading may be partial, or for immediate VMfail.
    pub fn failed_load(self, mirror: &[Entry], qualification: u64) -> Option<Self> {
        let completed = usize::try_from(qualification.checked_sub(1)?).ok()?;
        if mirror.len() > MAX_MSR_MIRROR_ENTRIES as usize || completed >= mirror.len() {
            return None;
        }
        let mut state = self;
        for entry in &mirror[..completed] {
            // A reported successful prefix cannot contain a format violation.
            if !entry.format_valid(Operation::EntryLoad) {
                return None;
            }
            match entry.index {
                0x277 => state.pat = entry.value,
                // MSR-list loads, like WRMSR, ignore the supplied LMA bit.
                0xc000_0080 => state.efer = (entry.value & !(1 << 10)) | (state.efer & (1 << 10)),
                _ => {}
            }
        }
        Some(state)
    }

    /// Applies original VM-exit controls before L1's exit MSR-load list. Without
    /// LOAD_EFER, VM exit still sets LME/LMA from host address-space size.
    #[must_use]
    pub const fn exit(self, host: Self, controls: u32) -> Self {
        Self {
            pat: if controls & crate::EXIT_LOAD_IA32_PAT != 0 {
                host.pat
            } else {
                self.pat
            },
            efer: if controls & crate::EXIT_LOAD_IA32_EFER != 0 {
                host.efer
            } else {
                (self.efer & !0x500)
                    | if controls & crate::EXIT_HOST_ADDRESS_SPACE_SIZE != 0 {
                        0x500
                    } else {
                        0
                    }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_store_ownership_never_reads_private_host_replacements() {
        use x86_64_hal::vmcs;
        for (index, field) in [
            (0x277, vmcs::GUEST_IA32_PAT),
            (0xc000_0080, vmcs::GUEST_IA32_EFER),
            (0xc000_0100, vmcs::GUEST_FS_BASE),
            (0xc000_0101, vmcs::GUEST_GS_BASE),
            (0x174, vmcs::GUEST_SYSENTER_CS),
            (0x175, vmcs::GUEST_SYSENTER_ESP),
            (0x176, vmcs::GUEST_SYSENTER_EIP),
        ] {
            assert_eq!(exit_store_source(index), ExitStoreSource::GuestField(field));
        }
        assert_eq!(
            exit_store_source(0x1d9),
            ExitStoreSource::PrivateDebugCapture
        );
        assert_eq!(
            exit_store_source(0x38f),
            ExitStoreSource::PrivatePerfCapture
        );
        for index in 0x480..=0x492 {
            assert_eq!(exit_store_source(index), ExitStoreSource::VmxCapability);
        }
        for index in [
            0x10,
            0x479,
            0x493,
            0xc000_0082,
            0xc000_0102,
            0xc000_0103,
            u32::MAX,
        ] {
            assert_eq!(exit_store_source(index), ExitStoreSource::Live);
        }
        // Classification is not a permission: malformed entries must fail
        // before a value source (including a harmless VMCS field) is consumed.
        assert!(
            !Entry {
                index: 0x277,
                reserved: 1,
                value: 0
            }
            .format_valid(Operation::ExitStore)
        );
        assert!(!Entry::new(0x800, 0).format_valid(Operation::ExitStore));
    }

    #[test]
    fn pat_efer_inherit_only_architecturally_unloaded_state() {
        let l1 = PatEfer {
            pat: 6,
            efer: 0x901,
        };
        let l2 = PatEfer {
            pat: 0,
            efer: 0xd01,
        };
        for load_pat in [0, crate::ENTRY_LOAD_IA32_PAT] {
            for load_efer in [0, crate::ENTRY_LOAD_IA32_EFER] {
                for ia32e in [0, 1 << 9] {
                    for paging in [0, 1 << 31] {
                        let state = l1.entry(l2, load_pat | load_efer | ia32e, paging);
                        assert_eq!(state.pat, if load_pat != 0 { l2.pat } else { l1.pat });
                        if load_efer != 0 {
                            assert_eq!(state.efer, l2.efer);
                        } else {
                            assert_eq!(state.efer & !0x500, l1.efer & !0x500);
                            assert_eq!(state.efer & 0x400 != 0, ia32e != 0);
                            assert_eq!(state.efer & 0x100 != 0, paging == 0 || ia32e != 0);
                        }
                    }
                    let state = l2.exit(l1, (load_pat << 5) | (load_efer << 6) | ia32e);
                    assert_eq!(state.pat, if load_pat != 0 { l1.pat } else { l2.pat });
                    assert_eq!(
                        state.efer,
                        if load_efer != 0 {
                            l1.efer
                        } else {
                            (l2.efer & !0x500) | if ia32e != 0 { 0x500 } else { 0 }
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn failed_entry_load_preserves_only_the_successful_ordered_prefix() {
        let initial = PatEfer {
            pat: 6,
            efer: 0xd01,
        };
        let entries = [
            Entry::new(0x277, 0x606),
            Entry::new(0xc000_0103, 99),
            Entry::new(0x277, 0x60606),
            Entry::new(0xc000_0080, 0x901),
            Entry::new(u32::MAX, 0),
        ];
        assert_eq!(initial.failed_load(&entries, 1), Some(initial));
        assert_eq!(initial.failed_load(&entries, 2).unwrap().pat, 0x606);
        assert_eq!(initial.failed_load(&entries, 3).unwrap().pat, 0x606);
        assert_eq!(initial.failed_load(&entries, 4).unwrap().pat, 0x60606);
        assert_eq!(initial.failed_load(&entries, 5).unwrap().efer, 0xd01);
        for qualification in [0, 6, u64::MAX] {
            assert_eq!(initial.failed_load(&entries, qualification), None);
        }
        assert_eq!(initial.failed_load(&[], 1), None);
        let bad = [Entry::new(0xc000_0100, 0), Entry::new(0x277, 0)];
        assert_eq!(initial.failed_load(&bad, 2), None);
        assert_eq!(initial.failed_load(&[entries[0]; 513], 1), None);
    }

    #[test]
    fn empty_lists_ignore_the_address_but_not_capacity_or_cpu_width() {
        for address in [0, 1, u64::MAX] {
            let list = List::new(address, 0, 52).unwrap();
            assert_eq!(list.address(), address);
            assert_eq!(list.count(), 0);
            assert_eq!(list.entry_address(0), None);
        }
        for bits in [0, 11, 53, 64, u8::MAX] {
            assert_eq!(List::new(0, 0, bits), Err(Error::PhysicalWidth));
        }
        for count in [513, u32::MAX] {
            assert_eq!(List::new(0, count, 52), Err(Error::Count));
        }
    }

    #[test]
    fn complete_ranges_enforce_alignment_width_and_checked_last_byte() {
        for bits in [12, 32, 36, 48, 52] {
            let limit = 1_u64 << bits;
            for count in [1, 2, 256] {
                let first = limit - u64::from(count) * ENTRY_BYTES;
                let list = List::new(first, count, bits).unwrap();
                assert_eq!(list.entry_address(count - 1), Some(limit - ENTRY_BYTES));
                assert_eq!(list.entry_address(count), None);
                assert_eq!(
                    List::new(first + ENTRY_BYTES, count, bits),
                    Err(Error::Address)
                );
            }
            assert!(List::new(0, 1, bits).is_ok());
            assert_eq!(List::new(limit, 1, bits), Err(Error::Address));
        }
        assert!(List::new((1 << 52) - 8192, 512, 52).is_ok());
        for offset in 1..16 {
            assert_eq!(List::new(4096 + offset, 1, 52), Err(Error::Alignment));
        }
        assert_eq!(List::new(u64::MAX - 15, 2, 52), Err(Error::Overflow));
        assert_eq!(List::new(u64::MAX - 15, 1, 52), Err(Error::Address));
    }

    #[test]
    fn operation_specific_exclusions_do_not_confuse_loads_with_stores() {
        for operation in [
            Operation::EntryLoad,
            Operation::ExitStore,
            Operation::ExitLoad,
        ] {
            for index in 0x800..=0x8ff {
                assert!(!Entry::new(index, 0).format_valid(operation));
            }
            assert!(
                !Entry {
                    index: 0x10,
                    reserved: 1,
                    value: 0
                }
                .format_valid(operation)
            );
            assert!(Entry::new(0xc000_0103, u64::MAX).format_valid(operation));
            // An unknown MSR is not a format error. The hardware-access stage
            // must return the appropriate load failure/exit abort separately.
            assert!(Entry::new(u32::MAX, 0).format_valid(operation));
        }
        for index in [0xc000_0100, 0xc000_0101, 0x9b] {
            assert!(!Entry::new(index, 0).format_valid(Operation::EntryLoad));
            assert!(!Entry::new(index, 0).format_valid(Operation::ExitLoad));
            assert!(Entry::new(index, 0).format_valid(Operation::ExitStore));
        }
        assert!(!Entry::new(0x9e, 0).format_valid(Operation::ExitStore));
        assert_eq!(core::mem::align_of::<Entry>(), 16);
    }
}
