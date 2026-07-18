#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AccessClass {
    /// Normal memory semantics; splitting/unaligned accesses do not change behavior.
    NormalMemory,
    /// Device or MMIO semantics; splitting/unaligned accesses can change device-visible effects.
    DeviceMmio,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SplitPolicy {
    /// Never split an access.
    Never,
    /// Allow split only when a side-effect-free probe confirms each sub-access is safe.
    OnlyIfProbe,
    /// Always allow split emulation (intended for `NormalMemory` only).
    Always,
}

#[derive(Copy, Clone, Debug)]
pub enum MmioError {
    Unhandled,
    Fault,
}

/// Backend for emulating guest accesses to an MMIO or normal-memory region.
///
/// Implementors own their state and synchronization. This keeps the emulator independent from
/// raw context pointers and makes the lifetime and thread-safety boundary explicit.
pub trait MmioHandler: Sync {
    /// Reads one value from `ipa` using `size` bytes.
    fn read(&self, ipa: u64, size: u8) -> Result<u64, MmioError>;

    /// Writes one `size`-byte value to `ipa`.
    fn write(&self, ipa: u64, size: u8, value: u64) -> Result<(), MmioError>;

    /// Reads a pair atomically enough for guest-visible LDP semantics.
    fn read_pair(
        &self,
        _ipa0: u64,
        _ipa1: u64,
        _size: u8,
    ) -> Option<Result<(u64, u64), MmioError>> {
        None
    }

    /// Writes a pair atomically enough for guest-visible STP semantics.
    fn write_pair(
        &self,
        _ipa0: u64,
        _ipa1: u64,
        _size: u8,
        _value0: u64,
        _value1: u64,
    ) -> Option<Result<(), MmioError>> {
        None
    }

    /// Checks a split sub-access without producing device-visible side effects.
    fn probe_subaccess(&self, _ipa: u64, _size: u8, _is_write: bool) -> bool {
        false
    }

    /// Describes whether accesses have normal-memory or device semantics.
    fn access_class(&self) -> AccessClass;

    /// Selects when the emulator may split an access.
    fn split_policy(&self) -> SplitPolicy;

    /// Returns whether splitting is always safe without probing individual sub-accesses.
    #[inline]
    fn can_split_without_probe(&self) -> bool {
        self.access_class() == AccessClass::NormalMemory
            && self.split_policy() == SplitPolicy::Always
    }

    /// Returns whether one sub-access may participate in split emulation.
    #[inline]
    fn can_split_subaccess(&self, ipa: u64, size: u8, is_write: bool) -> bool {
        match self.split_policy() {
            SplitPolicy::Never => false,
            SplitPolicy::Always => {
                debug_assert!(
                    self.access_class() == AccessClass::NormalMemory,
                    "Always split policy is only safe for normal memory"
                );
                self.access_class() == AccessClass::NormalMemory
            }
            SplitPolicy::OnlyIfProbe => self.probe_subaccess(ipa, size, is_write),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHandler {
        access_class: AccessClass,
        split_policy: SplitPolicy,
        probe_result: bool,
    }

    impl MmioHandler for TestHandler {
        fn read(&self, _ipa: u64, _size: u8) -> Result<u64, MmioError> {
            Ok(0)
        }

        fn write(&self, _ipa: u64, _size: u8, _value: u64) -> Result<(), MmioError> {
            Ok(())
        }

        fn probe_subaccess(&self, _ipa: u64, _size: u8, _is_write: bool) -> bool {
            self.probe_result
        }

        fn access_class(&self) -> AccessClass {
            self.access_class
        }

        fn split_policy(&self) -> SplitPolicy {
            self.split_policy
        }
    }

    fn make_handler(
        access_class: AccessClass,
        split_policy: SplitPolicy,
        probe_result: bool,
    ) -> TestHandler {
        TestHandler {
            access_class,
            split_policy,
            probe_result,
        }
    }

    #[test]
    fn device_never_disallows_split() {
        let h = make_handler(AccessClass::DeviceMmio, SplitPolicy::Never, false);
        assert!(!h.can_split_subaccess(0, 4, false));
        let h_probe = make_handler(AccessClass::DeviceMmio, SplitPolicy::Never, true);
        assert!(!h_probe.can_split_subaccess(0, 4, false));
    }

    #[test]
    fn device_probe_requires_probe() {
        let h = make_handler(AccessClass::DeviceMmio, SplitPolicy::OnlyIfProbe, false);
        assert!(!h.can_split_subaccess(0, 4, false));
        let h_probe = make_handler(AccessClass::DeviceMmio, SplitPolicy::OnlyIfProbe, true);
        assert!(h_probe.can_split_subaccess(0, 4, false));
    }

    #[test]
    fn normal_always_allows_without_probe() {
        let h = make_handler(AccessClass::NormalMemory, SplitPolicy::Always, false);
        assert!(h.can_split_without_probe());
        assert!(h.can_split_subaccess(0, 8, true));
    }

    #[test]
    fn normal_probe_tracks_probe() {
        let h = make_handler(AccessClass::NormalMemory, SplitPolicy::OnlyIfProbe, false);
        assert!(!h.can_split_subaccess(0, 8, false));
        let h_probe = make_handler(AccessClass::NormalMemory, SplitPolicy::OnlyIfProbe, true);
        assert!(h_probe.can_split_subaccess(0, 8, false));
    }
}
