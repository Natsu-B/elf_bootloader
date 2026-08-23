// Address domains share representation and arithmetic while remaining distinct types.
macro_rules! define_addr {
    ($($name:ident),+ $(,)?) => {$(
        #[repr(transparent)]
        #[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub struct $name(usize);

        impl $name {
            pub const fn new(value: usize) -> Self {
                Self(value)
            }

            pub const fn as_usize(self) -> usize {
                self.0
            }

            pub const fn checked_add(self, rhs: usize) -> Option<Self> {
                match self.0.checked_add(rhs) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            pub const fn checked_sub(self, rhs: usize) -> Option<Self> {
                match self.0.checked_sub(rhs) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }
        }
    )+};
}

define_addr!(PhysAddr, VirtAddr, IpaAddr);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_arithmetic() {
        let p = PhysAddr::new(0x1000);
        assert_eq!(p.checked_add(0x20).unwrap().as_usize(), 0x1020);
        assert_eq!(p.checked_sub(0x20).unwrap().as_usize(), 0x0fe0);
        assert!(PhysAddr::new(usize::MAX).checked_add(1).is_none());
        assert!(VirtAddr::new(0).checked_sub(1).is_none());
        assert_eq!(
            IpaAddr::new(0x2000).checked_add(0x10).unwrap().as_usize(),
            0x2010
        );
    }
}
