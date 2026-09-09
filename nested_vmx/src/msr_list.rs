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

#[cfg(test)]
mod tests {
    use super::*;

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
