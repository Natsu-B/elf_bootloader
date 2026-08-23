//! MSI-X capability structure and table handling.

use core::ptr::slice_from_raw_parts;

use typestate::ReadWrite;
use typestate::bitregs;

bitregs! {
    /// MSI-X capability configuration register.
    pub struct PciCapabilityMsiXConfigurations: u32 {
        pub id@[7:0],
        pub next_ptr@[15:8],
        pub table_size@[26:16],
        reserved@[29:27],
        pub function_mask@[30:30],
        pub msi_x_enable@[31:31],
    }
}

impl PciCapabilityMsiXConfigurations {
    /// Returns the number of MSI-X table entries (table_size + 1).
    pub fn table_entry_count(&self) -> usize {
        (self.get(PciCapabilityMsiXConfigurations::table_size) as usize) + 1
    }
}

bitregs! {
    /// MSI-X table offset and BAR indicator.
    pub struct PciCapabilityMsiXTableOffset: u32 {
        pub bir@[2:0],
        pub offset@[31:3],
    }
}

impl PciCapabilityMsiXTableOffset {
    /// Returns the table offset in bytes (bits 31:3 as-is, low 3 bits cleared).
    pub fn offset_bytes(&self) -> u32 {
        self.get_raw(PciCapabilityMsiXTableOffset::offset)
    }
}

bitregs! {
    /// MSI-X PBA (Pending Bit Array) offset and BAR indicator.
    pub struct PciCapabilityMsiXPbaOffset: u32 {
        pub bir@[2:0],
        pub offset@[31:3],
    }
}

/// MSI-X capability structure in PCI configuration space.
#[repr(C)]
#[derive(Debug)]
pub struct PciCapabilityMsiX {
    /// MSI-X configuration word.
    pub configurations: ReadWrite<PciCapabilityMsiXConfigurations>,
    /// Table offset and BAR indicator.
    pub table_offset: ReadWrite<PciCapabilityMsiXTableOffset>,
    /// PBA offset and BAR indicator.
    pub pba_offset: ReadWrite<PciCapabilityMsiXPbaOffset>,
}

impl PciCapabilityMsiX {
    /// Reinterprets the first 3 elements of a u32 slice as this capability.
    pub fn from_array<'a>(array: &'a [u32]) -> Option<&'a Self> {
        let (head3, _) = array.split_first_chunk::<3>()?;
        Some(unsafe { &*(head3.as_ptr() as *const Self) })
    }
}

bitregs! {
    /// MSI-X table entry vector control field.
    pub struct PciMsiXTableVectorControl: u32 {
        pub mask@[0:0],
        reserved@[31:1] [ignore],
    }
}

/// A single MSI-X table entry (16 bytes).
#[repr(C)]
#[derive(Debug)]
pub struct PciMsiXTable {
    /// Lower 32 bits of the message address.
    pub message_address_low: ReadWrite<u32>,
    /// Upper 32 bits of the message address.
    pub message_address_high: ReadWrite<u32>,
    /// Message data value.
    pub message_data: ReadWrite<u32>,
    /// Vector control (mask bit).
    pub vector_control: ReadWrite<PciMsiXTableVectorControl>,
}

impl PciMsiXTable {
    /// Reinterprets a u32 slice as an array of MSI-X table entries.
    pub fn from_array<'a>(array: &'a [u32], size: u32) -> Option<&'a [Self]> {
        let (head, _) =
            array.split_at_checked(size as usize * size_of::<PciMsiXTable>() / size_of::<u32>())?;
        Some(unsafe { &*slice_from_raw_parts(head.as_ptr() as *const Self, size as usize) })
    }
}

#[cfg(test)]
mod tests {
    use super::PciCapabilityMsiXConfigurations;
    use super::PciCapabilityMsiXTableOffset;

    #[test]
    fn msix_table_offset_decoding() {
        let x = 0xDEAD_BEEF;
        let table_offset = PciCapabilityMsiXTableOffset::from_bits(x);
        assert_eq!(table_offset.offset_bytes(), x & !0x7);
        assert_eq!(table_offset.get(PciCapabilityMsiXTableOffset::bir), x & 0x7);
    }

    #[test]
    fn msix_table_entry_count_decoding() {
        let zero = PciCapabilityMsiXConfigurations::new()
            .set(PciCapabilityMsiXConfigurations::table_size, 0);
        assert_eq!(zero.table_entry_count(), 1);

        let max = PciCapabilityMsiXConfigurations::new()
            .set(PciCapabilityMsiXConfigurations::table_size, 2047);
        assert_eq!(max.table_entry_count(), 2048);
    }
}
