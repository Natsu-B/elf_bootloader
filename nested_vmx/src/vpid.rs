//! Exclusive per-pCPU ownership of the nonzero hardware VPID namespace.
//!
//! The carrier must keep VPID disabled. One trusted L1 VMX execution period
//! then owns tags 1..=65535 directly; there is no VMCS-specific tag rewriting.
//! A VPID denotes an address-space context, not a VMCS, so VMCLEAR/VMPTRLD must
//! not recycle it behind L1's back. L1's INVVPID operations retain their exact
//! architectural scope. A complete invalidation separates unrelated leases.
//!
//! Intel does not require VMXON/VMXOFF themselves to invalidate cached mappings
//! (SDM, "Operations that Invalidate TLBs and Paging-Structure Caches"). The
//! lease boundary below deliberately provides the stronger L0 lifetime rule.

use x86_64_hal::addr::VmxonPhys;

/// A rejected ownership transition; failed invalidation never transfers tags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The current CPU already has a namespace owner.
    AlreadyOwned,
    /// A release does not name this CPU's active L1 VMX execution period.
    WrongOwner,
    /// The bounded generation counter cannot advance without wrapping.
    GenerationExhausted,
    /// Hardware did not complete the required all-context invalidation.
    InvalidationFailed,
}

/// Stored in the owning CPU's immovable private GS-bound runtime state.
/// This metadata must never be migrated independently of its VMX CPU.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct Namespace {
    owner: Option<VmxonPhys>,
    generation: u64,
}

impl Namespace {
    /// Starts with no L1 owner. Publication requires an untagged carrier.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            owner: None,
            generation: 0,
        }
    }

    /// A tagged carrier would collide with the exclusive L1 namespace.
    #[must_use]
    pub const fn carrier_supported(secondary_controls: u32) -> bool {
        secondary_controls & crate::SECONDARY_ENABLE_VPID == 0
    }

    /// Identifies the currently owned L1 VMX execution period on this CPU.
    #[must_use]
    pub fn owns(&self, owner: VmxonPhys) -> bool {
        self.owner == Some(owner)
    }

    /// Number of fully invalidated, successfully acquired execution periods.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Grants the entire nonzero namespace only after local hardware INVVPID
    /// type 2 succeeds. The callback must run on this namespace's pinned CPU,
    /// with no L1/L2 running and no tagged carrier or other owner using its tags.
    pub fn acquire(
        &mut self,
        owner: VmxonPhys,
        invalidate: impl FnOnce() -> bool,
    ) -> Result<(), Error> {
        if self.owner.is_some() {
            return Err(Error::AlreadyOwned);
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(Error::GenerationExhausted)?;
        if !invalidate() {
            return Err(Error::InvalidationFailed);
        }
        self.generation = generation;
        self.owner = Some(owner);
        Ok(())
    }

    /// Invalidates before releasing ownership. A failed callback retains the
    /// old owner, so callers cannot report a completed L1 VMXOFF or reuse tags.
    /// The same pinned-CPU/no-running-guest contract as acquire applies.
    pub fn release(
        &mut self,
        owner: VmxonPhys,
        invalidate: impl FnOnce() -> bool,
    ) -> Result<(), Error> {
        if !self.owns(owner) {
            return Err(Error::WrongOwner);
        }
        if !invalidate() {
            return Err(Error::InvalidationFailed);
        }
        self.owner = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonzero_tags_are_exclusive_and_reuse_requires_completed_flushes() {
        let a = VmxonPhys::new(0x1000).unwrap();
        let b = VmxonPhys::new(0x2000).unwrap();
        let mut namespace = Namespace::new();
        let mut flushes = 0;
        assert!(!namespace.owns(a));
        assert_eq!(
            namespace.acquire(a, || {
                flushes += 1;
                true
            }),
            Ok(())
        );
        assert!(namespace.owns(a));
        assert_eq!(namespace.generation(), 1);
        assert_eq!(
            namespace.acquire(b, || panic!("must not touch hardware")),
            Err(Error::AlreadyOwned)
        );
        assert_eq!(
            namespace.release(b, || panic!("must not touch hardware")),
            Err(Error::WrongOwner)
        );
        assert_eq!(
            namespace.release(a, || false),
            Err(Error::InvalidationFailed)
        );
        assert!(namespace.owns(a));
        assert_eq!(
            namespace.release(a, || {
                flushes += 1;
                true
            }),
            Ok(())
        );
        assert!(!namespace.owns(a));
        assert_eq!(
            namespace.acquire(b, || false),
            Err(Error::InvalidationFailed)
        );
        assert!(!namespace.owns(b));
        assert_eq!(namespace.generation(), 1);
        assert_eq!(
            namespace.acquire(b, || {
                flushes += 1;
                true
            }),
            Ok(())
        );
        assert_eq!(namespace.generation(), 2);
        assert_eq!(flushes, 3);
    }

    #[test]
    fn same_vmxon_address_after_release_is_a_new_lifetime() {
        let owner = VmxonPhys::new(0x1000).unwrap();
        let mut namespace = Namespace::new();
        for generation in 1..=4096 {
            assert_eq!(namespace.acquire(owner, || true), Ok(()));
            assert_eq!(namespace.generation(), generation);
            assert_eq!(namespace.release(owner, || true), Ok(()));
        }
        namespace.generation = u64::MAX;
        assert_eq!(
            namespace.acquire(owner, || panic!("must not flush on overflow")),
            Err(Error::GenerationExhausted)
        );
        assert_eq!(namespace.generation(), u64::MAX);
        assert!(!namespace.owns(owner));
    }

    #[test]
    fn cpu_local_namespaces_do_not_transfer_another_cpus_lease() {
        let a = VmxonPhys::new(0x1000).unwrap();
        let b = VmxonPhys::new(0x2000).unwrap();
        let mut cpu_a = Namespace::new();
        let mut cpu_b = Namespace::new();
        assert_eq!(cpu_a.acquire(a, || true), Ok(()));
        assert_eq!(cpu_b.acquire(b, || true), Ok(()));
        assert_eq!(
            cpu_b.release(a, || panic!("wrong CPU owner")),
            Err(Error::WrongOwner)
        );
        assert_eq!(cpu_a.release(a, || true), Ok(()));
        assert!(cpu_b.owns(b));
        assert_eq!(cpu_b.generation(), 1);
        assert!(Namespace::carrier_supported(0));
        assert!(Namespace::carrier_supported(crate::SECONDARY_ENABLE_EPT));
        assert!(!Namespace::carrier_supported(crate::SECONDARY_ENABLE_VPID));
        assert!(!Namespace::carrier_supported(u32::MAX));
    }
}
