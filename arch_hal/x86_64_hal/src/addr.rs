//! Physical-address types used at the VMX boundary.

macro_rules! physical_address {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
        #[repr(transparent)]
        pub struct $name(u64);

        impl $name {
            /// Creates an address after checking the architectural page alignment.
            pub const fn new(address: u64) -> Option<Self> {
                if address & 0xfff == 0 {
                    Some(Self(address))
                } else {
                    None
                }
            }

            /// Returns the raw physical address.
            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

physical_address!(HostPhys, "A machine physical page address.");
physical_address!(GuestPhys, "A guest physical page address.");
physical_address!(VmxonPhys, "A VMXON physical page address.");
physical_address!(VmcsPhys, "A VMCS physical page address.");
physical_address!(EptPhys, "An EPT paging-structure physical page address.");

#[cfg(test)]
mod tests {
    use super::VmcsPhys;

    #[test]
    fn vmcs_address_requires_page_alignment() {
        assert_eq!(VmcsPhys::new(0x2000).map(VmcsPhys::get), Some(0x2000));
        assert_eq!(VmcsPhys::new(0x2001), None);
    }
}
