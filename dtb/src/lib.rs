#![cfg_attr(not(test), no_std)]

use core::ffi::CStr;
use core::ffi::c_char;
use core::ops::ControlFlow;
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::string::String;

#[cfg(alloc)]
enum DeviceProperty {
    String(&'static str),
    StringList(Vec<&'static str>),
    U32(u32),
    Cells(Vec<u32>),
    Empty,
}

#[cfg(alloc)]
struct DeviceNode<'a> {
    name: &'static [u8],
    full_name: &'static [u8],
    properties: HashMap<&'static [u8], DeviceProperty>,
    parent: Option<NonNull<&'a DeviceNode<'a>>>,
    child: Option<Vec<NonNull<&'a DeviceNode<'a>>>>,
}

pub use dtb_parser::DtbParser;

mod dtb_parser {
    use super::*;
    use big_endian::CharStringIter;
    use big_endian::Dtb;
    use big_endian::FdtProperty;

    struct SimpleDeviceNode<'a> {
        parent: Option<&'a SimpleDeviceNode<'a>>,
        address_cells: u32,
        size_cells: u32,
        reg: Option<(usize, u32)>,
        ranges: Option<usize>,
    }

    impl<'a> SimpleDeviceNode<'a> {
        const ADDRESS_CELLS: &'static str = "#address-cells";
        const SIZE_CELLS: &'static str = "#size-cells";
        const PROP_COMPATIBLE: &'static str = "compatible";
        const PROP_DEVICE_NAME: &'static str = "device_type";
        const PROP_REG: &'static str = "reg";
        const PROP_RANGES: &'static str = "ranges";

        fn new<'b: 'a>(parent: Option<&'b SimpleDeviceNode>) -> Self {
            Self {
                address_cells: 2,
                size_cells: 1,
                reg: None,
                ranges: None,
                parent,
            }
        }

        // address is assumed to point to the FDT_PROP token
        fn parse_prop(
            &mut self,
            parser: &DtbParser,
            address: &mut usize,
            device_name: Option<&str>,
            compatible_name: Option<&str>,
        ) -> Result<bool, &'static str> {
            *address += DtbParser::SIZEOF_FDT_TOKEN;
            let property = unsafe { &*(*address as *const FdtProperty) };
            #[cfg(test)]
            assert_eq!(size_of::<FdtProperty>(), 8);
            *address += size_of::<FdtProperty>();
            let name = Dtb::read_char_str(
                parser.dtb_header.get_string_start_address() + property.get_name_offset() as usize,
            )?;
            pr_debug!(
                "FDT_PROP offset: {}, str: {}",
                property.get_name_offset(),
                name
            );
            let mut result = false;
            if let Some(s) = match name {
                Self::ADDRESS_CELLS => {
                    self.address_cells = Dtb::read_u32_from_ptr(*address);
                    pr_debug!("address_cells: {}", self.address_cells);
                    Some(size_of::<u32>())
                }
                Self::SIZE_CELLS => {
                    self.size_cells = Dtb::read_u32_from_ptr(*address);
                    pr_debug!("size_cells: {}", self.size_cells);
                    Some(size_of::<u32>())
                }
                Self::PROP_COMPATIBLE => {
                    if let Some(compatible_name) = compatible_name {
                        for str in CharStringIter::new(*address, property.get_property_len()) {
                            if compatible_name == str? {
                                result = true;
                            }
                        }
                    }
                    None
                }
                Self::PROP_DEVICE_NAME => {
                    if let Some(device_name) = device_name
                        && device_name == Dtb::read_char_str(*address)?
                    {
                        result = true;
                    }
                    None
                }
                Self::PROP_REG => {
                    self.reg = Some((*address, property.get_property_len()));
                    Some(
                        self.parent
                            .ok_or("'reg' property should not be located at the root node")
                            .map(|node| {
                                node.address_cells as usize * size_of::<u32>()
                                    + node.size_cells as usize * size_of::<u32>()
                            })?,
                    )
                }
                Self::PROP_RANGES => {
                    self.ranges = Some(*address);
                    if property.get_property_len() != 0 {
                        pr_debug!(
                            "parent address: {}, child address: {}, child_size: {}",
                            self.parent.unwrap().address_cells,
                            self.address_cells,
                            self.size_cells
                        );
                        None
                    } else {
                        Some(0)
                    }
                }
                _ => None,
            } {
                // if s > property.get_property_len() as usize {
                //     pr_debug!(
                //         "invalid size!!! expected: {} actually: {}",
                //         property.get_property_len(),
                //         s
                //     );
                //     return Err("invalid size");
                // }
            }
            *address += property
                .get_property_len()
                .next_multiple_of(DtbParser::ALIGNMENT) as usize;
            Ok(result)
        }

        fn read_reg_internal(&self, offset: usize) -> Result<Option<(usize, usize)>, &'static str> {
            if let Some(reg) = self.reg {
                let (address_cells, size_cells) = self
                    .parent
                    .map(|node| (node.address_cells, node.size_cells))
                    .unwrap();
                if address_cells as usize > (size_of::<usize>() / size_of::<u32>())
                    || size_cells as usize > (size_of::<usize>() / size_of::<u32>())
                {
                    return Err("address or size cells overflow usize");
                }
                let address = Dtb::read_regs(reg.0 + offset, address_cells)?;
                let len = Dtb::read_regs(reg.0 + offset + address.1, size_cells)?;
                pr_debug!("reg: address: {:#x}, size: {:#x}", address.0, len.0);
                return Ok(Some((address.0, len.0)));
            }
            Ok(None)
        }

        fn read_round_internal(&self) -> Result<Option<(usize, usize, usize)>, &'static str> {
            if let Some(reg) = self.ranges {
                let child_address = Dtb::read_regs(reg, self.address_cells)?;
                let parent_address =
                    Dtb::read_regs(reg + child_address.1, self.parent.unwrap().address_cells)?;
                let parent_len =
                    Dtb::read_regs(reg + child_address.1 + parent_address.1, self.size_cells)?;
                #[cfg(test)]
                assert_eq!(parent_len.1, self.size_cells as usize * size_of::<u32>());
                return Ok(Some((child_address.0, parent_address.0, parent_len.0)));
            }
            Ok(None)
        }

        fn calculate_address_internal(
            &self,
            address: &(usize, usize),
        ) -> Result<(usize, usize), &'static str> {
            let address_child = {
                if self.ranges.is_some() {
                    let parent = self.read_round_internal()?.unwrap();
                    if parent.0 + parent.2 < address.0 + address.1 {
                        return Err("ranges size overflow");
                    }
                    pr_debug!(
                        "child_address: {:#x}, parent_address: {:#x}, child_len: {:#x}",
                        parent.0,
                        parent.1,
                        parent.2
                    );
                    (address.0 - parent.0 + parent.1, address.1)
                } else {
                    *address
                }
            };
            if let Some(s) = self.parent {
                return s.calculate_address_internal(&address_child);
            }
            Ok(address_child)
        }
    }

    struct DeviceAddressIter<'a> {
        prop: &'a SimpleDeviceNode<'a>,
        remain: Option<u32>,
    }

    impl<'a> DeviceAddressIter<'a> {
        fn new(prop: &'a SimpleDeviceNode) -> Self {
            Self { prop, remain: None }
        }
        fn next_internal(&mut self) -> Result<Option<(usize, usize)>, &'static str> {
            let (_address, size) = self.prop.reg.ok_or("reg property is none")?;
            let remain = self.remain.get_or_insert(size);
            if *remain == 0 {
                return Ok(None);
            }
            pr_debug!("read reg offset at: {}", size - *remain);
            let result = self.prop.calculate_address_internal(
                &self
                    .prop
                    .read_reg_internal(size as usize - *remain as usize)?
                    .ok_or("reg property is none")?,
            )?;
            *remain -= self
                .prop
                .parent
                .map(|parent| (parent.address_cells + parent.size_cells) * size_of::<u32>() as u32)
                .unwrap();
            Ok(Some(result))
        }
    }

    impl<'a> Iterator for DeviceAddressIter<'a> {
        type Item = Result<(usize, usize), &'static str>;

        fn next(&mut self) -> Option<Self::Item> {
            self.next_internal().transpose()
        }
    }

    pub struct DtbParser {
        dtb_header: Dtb,
    }

    impl DtbParser {
        const SIZEOF_FDT_TOKEN: usize = 4;
        const ALIGNMENT: u32 = 4;
        const FDT_BEGIN_NODE: [u8; Self::SIZEOF_FDT_TOKEN] = [0x00, 0x00, 0x00, 0x01];
        const FDT_END_NODE: [u8; Self::SIZEOF_FDT_TOKEN] = [0x00, 0x00, 0x00, 0x02];
        const FDT_PROP: [u8; Self::SIZEOF_FDT_TOKEN] = [0x00, 0x00, 0x00, 0x03];
        const FDT_NOP: [u8; Self::SIZEOF_FDT_TOKEN] = [0x00, 0x00, 0x00, 0x04];
        const FDT_END: [u8; Self::SIZEOF_FDT_TOKEN] = [0x00, 0x00, 0x00, 0x09];
        pub fn init(dtb_address: usize) -> Result<Self, &'static str> {
            let dtb = Dtb::new(dtb_address)?;
            let parser = Self { dtb_header: dtb };
            Ok(parser)
        }
        fn skip_nop(&self, address: &mut usize) {
            while *address < self.dtb_header.get_struct_end_address()
                && Self::get_types(address) == Self::FDT_NOP
            {
                *address += Self::SIZEOF_FDT_TOKEN;
            }
        }

        fn get_types(address: &usize) -> [u8; 4] {
            unsafe { *(*address as *const [u8; Self::SIZEOF_FDT_TOKEN]) }
        }

        pub fn find_node<F>(
            &self,
            device_name: Option<&str>,
            compatible_name: Option<&str>,
            f: &mut F,
        ) -> Result<(), &'static str>
        where
            F: FnMut((usize, usize)) -> ControlFlow<()>,
        {
            if device_name.is_some() && compatible_name.is_some()
                || device_name.is_none() && compatible_name.is_none()
            {
                return Err(
                    "device name and compatible name cannot be searched for at the same time",
                );
            }
            let mut pointer = self.dtb_header.get_struct_start_address();
            self.skip_nop(&mut pointer);
            let _result =
                self.find_node_recursive(&mut pointer, device_name, compatible_name, None, f)?;
            // TODO
            // if Self::get_types(&pointer) != Self::FDT_END {
            //     pr_debug!(
            //         "failed to parse all of the dtb node: {:?}",
            //         Self::get_types(&pointer)
            //     );
            //     return Err("failed to parse all of the dtb node");
            // }
            Ok(())
        }

        fn find_node_recursive<F>(
            &self,
            pointer: &mut usize,
            device_name: Option<&str>,
            compatible_name: Option<&str>,
            node_info: Option<&SimpleDeviceNode>,
            f: &mut F,
        ) -> Result<ControlFlow<()>, &'static str>
        where
            F: FnMut((usize, usize)) -> ControlFlow<()>,
        {
            if Self::get_types(pointer) != Self::FDT_BEGIN_NODE {
                return Err("pointer is not begin node");
            }
            let mut prop = SimpleDeviceNode::new(node_info);
            *pointer += Self::SIZEOF_FDT_TOKEN;
            let node_name = Dtb::read_char_str(*pointer)?;
            *pointer += (node_name.len() + 1/* null terminator */)
                .next_multiple_of(Self::ALIGNMENT as usize);
            pr_debug!("node name: {}", node_name);
            let mut find_in_this_node = false;
            loop {
                match Self::get_types(pointer) {
                    Self::FDT_NOP => *pointer += Self::SIZEOF_FDT_TOKEN,
                    Self::FDT_PROP => {
                        if prop.parse_prop(self, pointer, device_name, compatible_name)? {
                            find_in_this_node = true;
                        }
                    }
                    Self::FDT_BEGIN_NODE | Self::FDT_END_NODE => break,
                    _ => return Err("Unexpected token inside node"),
                }
            }

            if find_in_this_node {
                for address in DeviceAddressIter::new(&prop) {
                    if f(address?).is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }

            loop {
                match Self::get_types(pointer) {
                    Self::FDT_NOP => *pointer += Self::SIZEOF_FDT_TOKEN,
                    Self::FDT_BEGIN_NODE => {
                        if self
                            .find_node_recursive(
                                pointer,
                                device_name,
                                compatible_name,
                                Some(&prop),
                                f,
                            )?
                            .is_break()
                        {
                            return Ok(ControlFlow::Break(()));
                        }
                    }
                    Self::FDT_END_NODE => {
                        *pointer += Self::SIZEOF_FDT_TOKEN;
                        return Ok(ControlFlow::Continue(()));
                    }
                    _ => {
                        pr_debug!(
                            "find an unknown or unexpected token: {:?}, address: 0x{:#?}",
                            Self::get_types(pointer),
                            *pointer - self.dtb_header.get_struct_start_address()
                        );
                        return Err("find an unknown or unexpected token while parsing the DTB");
                    }
                }
            }
        }
    }

    mod big_endian {
        // big endianで読み出さないといけないので、modで囲って関連関数を使わないと呼び出せないように
        use super::*;
        use crate::pr_debug;
        #[repr(C)]
        struct FtdHeader {
            magic: u32,             // should contain 0xd00dfeed (big-endian)
            total_size: u32,        // total size of the DTB in bytes
            off_dt_struct: u32,     // structure block offset (byte)
            off_dt_strings: u32,    // strings block offset (byte)
            off_mem_rsvmap: u32,    // memory reservation block offset (byte)
            version: u32,           // version of the device tree data structure
            last_comp_version: u32, // lowest version of the device tree data structure (backward compatible)
            boot_cpuid_phys: u32,   // physical CPU id
            size_dt_strings: u32,   // strings block section
            size_dt_struct: u32,    // structure block section
        }
        #[repr(C)]
        pub struct FdtProperty {
            property_len: u32,
            name_offset: u32,
        }

        impl FdtProperty {
            pub fn get_property_len(&self) -> u32 {
                u32::from_be(self.property_len)
            }
            pub fn get_name_offset(&self) -> u32 {
                u32::from_be(self.name_offset)
            }
        }

        #[repr(C)]
        pub struct FdtReserveEntry {
            address: u64,
            size: u64,
        }

        impl FdtReserveEntry {
            pub fn get_address(&self) -> u64 {
                u64::from_be(self.address)
            }
            pub fn get_size(&self) -> u64 {
                u64::from_be(self.size)
            }
        }

        pub struct Dtb {
            address: &'static FtdHeader,
        }

        impl Dtb {
            const DTB_VERSION: u32 = 17;
            const DTB_HEADER_MAGIC: u32 = 0xd00d_feed;
            pub fn new(address: usize) -> Result<Dtb, &'static str> {
                let ftb = Self {
                    address: unsafe { &*(address as *const FtdHeader) },
                };
                let header = ftb.address;
                pr_debug!("dtb version: {}", u32::from_be(header.last_comp_version));
                if u32::from_be(header.magic) != Self::DTB_HEADER_MAGIC {
                    return Err("invalid magic");
                }
                if u32::from_be(header.last_comp_version) > Self::DTB_VERSION {
                    return Err("this dtb is not compatible with the version 17");
                }
                Ok(ftb)
            }
            pub fn get_struct_start_address(&self) -> usize {
                self.address as *const _ as usize
                    + u32::from_be(self.address.off_dt_struct) as usize
            }
            pub fn get_struct_end_address(&self) -> usize {
                self.get_struct_start_address() + u32::from_be(self.address.size_dt_struct) as usize
            }
            pub fn get_string_start_address(&self) -> usize {
                self.address as *const _ as usize
                    + u32::from_be(self.address.off_dt_strings) as usize
            }
            pub fn get_string_end_address(&self) -> usize {
                self.get_string_start_address()
                    + u32::from_be(self.address.size_dt_strings) as usize
            }
            pub fn get_memory_reservation_start_address(&self) -> usize {
                self.address as *const _ as usize
                    + u32::from_be(self.address.off_mem_rsvmap) as usize
            }
            pub fn read_u32_from_ptr(address: usize) -> u32 {
                u32::from_be(unsafe { *(address as *const u32) })
            }
            // reads null-terminated strings and covert to &str
            pub fn read_char_str(address: usize) -> Result<&'static str, &'static str> {
                let str = unsafe { CStr::from_ptr(address as *const c_char) };
                str.to_str().map_err(|_| "failed to convert &Cstr to &str")
            }
            // read 'reg' property value
            pub fn read_regs(address: usize, size: u32) -> Result<(usize, usize), &'static str> {
                let mut address_result = 0;
                let mut address_consumed = 0;
                pr_debug!("read_reg: {}", size);
                for _ in 0..size {
                    address_result <<= 32;
                    address_result +=
                        u32::from_be(unsafe { *((address + address_consumed) as *const u32) })
                            as usize;
                    address_consumed += size_of::<u32>();
                }
                Ok((address_result, address_consumed))
            }
        }
        // an iterator over a list of null terminated strings within a property
        pub struct CharStringIter {
            pointer: usize,
            remain_size: u32,
        }
        impl CharStringIter {
            pub fn new(pointer: usize, remain_size: u32) -> Self {
                Self {
                    pointer,
                    remain_size,
                }
            }
            fn next_internal(&mut self) -> Result<&'static str, &'static str> {
                let str = Dtb::read_char_str(self.pointer);
                if let Ok(s) = str {
                    let str_size = s.len() + 1 /* null terminator */;
                    self.pointer += str_size;
                    self.remain_size = self
                        .remain_size
                        .checked_sub(str_size.try_into().map_err(|_| "over flow")?)
                        .ok_or("too small")?;
                }
                str
            }
        }
        impl Iterator for CharStringIter {
            type Item = Result<&'static str, &'static str>;
            fn next(&mut self) -> Option<Self::Item> {
                if self.remain_size == 0 {
                    return None;
                }
                Some(self.next_internal())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::result;
    use std::cell::RefCell;

    const PL011_DEBUG_UART_ADDRESS: usize = 0x10_7D00_1000;
    const PL011_DEBUG_UART_SIZE: usize = 0x200;
    const MEMORY_ADDRESS: usize = 0x0;
    const MEMORY_SIZE: usize = 0x2800_0000;
    #[test]
    fn it_works() {
        let test_data = std::fs::read("test/test.dtb").expect("failed to load dtb files");
        let test_data_addr = test_data.as_ptr() as usize;
        let parser = DtbParser::init(test_data_addr).unwrap();

        let mut counter = 0;
        parser
            .find_node(None, Some("arm,pl011"), &mut |(address, size)| {
                pr_debug!("find pl011 node, address: {} size: {}", address, size);
                assert_eq!(address, PL011_DEBUG_UART_ADDRESS);
                assert_eq!(size, PL011_DEBUG_UART_SIZE);
                counter += 1;
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(counter, 1);

        counter = 0;
        parser
            .find_node(Some("memory"), None, &mut |(address, size)| {
                pr_debug!("find memory node, address: {} size: {}", address, size);
                assert_eq!(address, MEMORY_ADDRESS);
                assert_eq!(size, MEMORY_SIZE);
                counter += 1;
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(counter, 1);
        counter = 0;
        parser
            .find_node(None, Some("arm,gic-400"), &mut |(address, size)| {
                pr_debug!("find gic node, address: {} size: {}", address, size);
                counter += 1;
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(counter, 4);
    }
}

#[cfg(test)]
#[macro_export]
macro_rules! pr_debug {
    ($fmt:expr) => (println!($fmt));
    ($fmt:expr, $($arg:tt)*) => (println!($fmt, $($arg)*));
}

#[cfg(not(test))]
#[macro_export]
macro_rules! pr_debug {
    ($fmt:expr) => {};
    ($fmt:expr, $($arg:tt)*) => {};
}
