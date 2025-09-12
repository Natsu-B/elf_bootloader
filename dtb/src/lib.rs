#![cfg_attr(not(test), no_std)]

use core::ffi::CStr;
use core::ffi::c_char;
use core::ops::ControlFlow;

pub use dtb_parser::DtbParser;

mod dtb_parser {
    use super::*;
    use big_endian::CharStringIter;
    use big_endian::Dtb;
    use big_endian::FdtProperty;
    use big_endian::FdtReserveEntry;
    use core::mem::size_of;

    trait DtbStructData: Sized {
        fn new(parent: Option<*const Self>) -> Self;
    }

    struct PropertyData {
        head_addr: usize,
        len: u32,
    }

    struct SimpleDeviceNode {
        parent: Option<*const SimpleDeviceNode>,
        address_cells: u32,
        size_cells: u32,
        reg: Option<PropertyData>,
        ranges: Option<usize>,
    }

    impl DtbStructData for SimpleDeviceNode {
        fn new(parent: Option<*const Self>) -> Self {
            Self {
                address_cells: 2,
                size_cells: 1,
                reg: None,
                ranges: None,
                parent,
            }
        }
    }

    enum ReservedMemoryData {
        Static {
            reg: PropertyData,
        },
        Dynamic {
            size: Option<PropertyData>, // require
            alignment: Option<PropertyData>,
            alloc_ranges: Option<PropertyData>,
        },
    }

    enum ReservedMemoryNode {
        Unused(Option<*const ReservedMemoryNode>),
        Parent {
            address_cells: u32,
            size_cells: u32,
        },
        Child {
            parent: *const ReservedMemoryNode,
            data: Option<ReservedMemoryData>,
        },
    }

    impl DtbStructData for ReservedMemoryNode {
        fn new(parent: Option<*const Self>) -> Self {
            Self::Unused(parent)
        }
    }

    impl ReservedMemoryNode {
        fn assume_parent_and_get_property(&self) -> Result<(u32, u32), &'static str> {
            if let Self::Parent {
                address_cells,
                size_cells,
            } = self
            {
                Ok((*address_cells, *size_cells))
            } else {
                Err("reserved-memory: expected parent when reading cells")
            }
        }
    }

    impl SimpleDeviceNode {
        const ADDRESS_CELLS: &'static str = "#address-cells";
        const SIZE_CELLS: &'static str = "#size-cells";
        const PROP_COMPATIBLE: &'static str = "compatible";
        const PROP_DEVICE_NAME: &'static str = "device_type";
        const PROP_REG: &'static str = "reg";
        const PROP_RANGES: &'static str = "ranges";
        const PROP_SIZE: &'static str = "size";
        const PROP_ALIGNMENT: &'static str = "alignment";
        const PROP_ALLOC_RANGES: &'static str = "alloc-ranges";

        fn parent_ref(&self) -> Option<&Self> {
            self.parent.map(|p| unsafe { &*p })
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
            if let Some(_s) = match name {
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
                    if property.get_property_len() != 0 {
                        self.reg = Some(PropertyData {
                            head_addr: *address,
                            len: property.get_property_len(),
                        });
                        Some(
                            self.parent_ref()
                                .ok_or("'reg' property should not be located at the root node")
                                .map(|node| {
                                    node.address_cells as usize * size_of::<u32>()
                                        + node.size_cells as usize * size_of::<u32>()
                                })?,
                        )
                    } else {
                        None
                    }
                }
                Self::PROP_RANGES => {
                    self.ranges = Some(*address);
                    if property.get_property_len() != 0 {
                        pr_debug!(
                            "parent address: {}, child address: {}, child_size: {}",
                            self.parent_ref().unwrap().address_cells,
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
            if let Some(reg) = &self.reg {
                let (address_cells, size_cells) = self
                    .parent_ref()
                    .map(|node| (node.address_cells, node.size_cells))
                    .unwrap();
                if address_cells as usize > (size_of::<usize>() / size_of::<u32>())
                    || size_cells as usize > (size_of::<usize>() / size_of::<u32>())
                {
                    return Err("address or size cells overflow usize");
                }
                let address = Dtb::read_regs(reg.head_addr + offset, address_cells)?;
                let len = Dtb::read_regs(reg.head_addr + offset + address.1, size_cells)?;
                pr_debug!("reg: address: {:#x}, size: {:#x}", address.0, len.0);
                return Ok(Some((address.0, len.0)));
            }
            Ok(None)
        }

        fn read_round_internal(&self) -> Result<Option<(usize, usize, usize)>, &'static str> {
            if let Some(reg) = self.ranges {
                let child_address = Dtb::read_regs(reg, self.address_cells)?;
                let parent_address = Dtb::read_regs(
                    reg + child_address.1,
                    self.parent_ref().unwrap().address_cells,
                )?;
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
                return unsafe { (&*s).calculate_address_internal(&address_child) };
            }
            Ok(address_child)
        }
    }

    struct DeviceAddressIter<'a> {
        prop: &'a SimpleDeviceNode,
        remain: Option<u32>,
    }

    impl<'a> DeviceAddressIter<'a> {
        fn new(prop: &'a SimpleDeviceNode) -> Self {
            Self { prop, remain: None }
        }
        fn next_internal(&mut self) -> Result<Option<(usize, usize)>, &'static str> {
            let PropertyData {
                head_addr: _address,
                len: size,
            } = self.prop.reg.as_ref().ok_or("reg property is none")?;
            let remain = self.remain.get_or_insert(*size);
            if *remain == 0 {
                return Ok(None);
            }
            pr_debug!("read reg offset at: {}", size - *remain);
            let result = self.prop.calculate_address_internal(
                &self
                    .prop
                    .read_reg_internal(*size as usize - *remain as usize)?
                    .ok_or("reg property is none")?,
            )?;
            *remain -= self
                .prop
                .parent_ref()
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

        fn walk_struct<P, C, T>(
            &self,
            pointer: &mut usize,
            node_info: Option<&T>,
            parse_property: &mut P,
            calculate_property: &mut C,
            remaining_depth: Option<u32>,
        ) -> Result<ControlFlow<()>, &'static str>
        where
            T: DtbStructData,
            P: FnMut(
                &mut T,
                &'static str,
                &DtbParser,
                &mut usize,
            ) -> Result<(bool, Option<u32>), &'static str>,
            C: FnMut(&mut T) -> Result<ControlFlow<()>, &'static str>,
        {
            let mut remaining_depth = remaining_depth;
            if Self::get_types(pointer) != Self::FDT_BEGIN_NODE {
                return Err("walk_struct: expected FDT_BEGIN_NODE");
            }
            let mut prop = T::new(node_info.map(|p| p as *const T));
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
                        let result = parse_property(&mut prop, node_name, self, pointer)?;
                        remaining_depth = result.1;
                        if result.0 {
                            find_in_this_node = true;
                        }
                    }
                    Self::FDT_BEGIN_NODE | Self::FDT_END_NODE => break,
                    _ => return Err("walk_struct: unexpected token inside node"),
                }
            }

            if find_in_this_node && calculate_property(&mut prop)?.is_break() {
                return Ok(ControlFlow::Break(()));
            }

            loop {
                match Self::get_types(pointer) {
                    Self::FDT_NOP => *pointer += Self::SIZEOF_FDT_TOKEN,
                    // If depth limit exists and reached 0, skip this subtree
                    Self::FDT_BEGIN_NODE if remaining_depth == Some(0) => {
                        self.skip_node(pointer)?;
                    }
                    Self::FDT_BEGIN_NODE => {
                        if self
                            .walk_struct(
                                pointer,
                                Some(&prop),
                                parse_property,
                                calculate_property,
                                remaining_depth.map(|f| f.saturating_sub(1)),
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
                        return Err("walk_struct: unknown or unexpected token while parsing DTB");
                    }
                }
            }
        }

        fn skip_node(&self, pointer: &mut usize) -> Result<(), &'static str> {
            if Self::get_types(pointer) != Self::FDT_BEGIN_NODE {
                return Err("skip_node: expected FDT_BEGIN_NODE");
            }
            let mut nest = 0usize;
            loop {
                match Self::get_types(pointer) {
                    Self::FDT_NOP => {
                        *pointer += Self::SIZEOF_FDT_TOKEN;
                    }
                    Self::FDT_BEGIN_NODE => {
                        // consume token + node name (nul-terminated, 4-byte aligned)
                        *pointer += Self::SIZEOF_FDT_TOKEN;
                        let node_name = Dtb::read_char_str(*pointer)?;
                        *pointer +=
                            (node_name.len() + 1).next_multiple_of(Self::ALIGNMENT as usize);
                        nest += 1;
                    }
                    Self::FDT_PROP => {
                        *pointer += Self::SIZEOF_FDT_TOKEN;
                        let property = unsafe { &*(*pointer as *const FdtProperty) };
                        *pointer += size_of::<FdtProperty>()
                            + property
                                .get_property_len()
                                .next_multiple_of(Self::ALIGNMENT)
                                as usize;
                    }
                    Self::FDT_END_NODE => {
                        *pointer += Self::SIZEOF_FDT_TOKEN;
                        nest -= 1;
                        if nest == 0 {
                            break;
                        }
                    }
                    _ => return Err("skip_node: unknown or unexpected token"),
                }
            }
            Ok(())
        }

        pub fn find_node<F>(
            &self,
            device_name: Option<&str>,
            compatible_name: Option<&str>,
            f: &mut F,
        ) -> Result<(), &'static str>
        where
            F: FnMut(usize, usize) -> ControlFlow<()>,
        {
            if (device_name.is_some() && compatible_name.is_some())
                || (device_name.is_none() && compatible_name.is_none())
            {
                return Err(
                    "device name and compatible name cannot be searched for at the same time",
                );
            }
            let mut pointer = self.dtb_header.get_struct_start_address();
            self.skip_nop(&mut pointer);

            // parse_property closure: parse props and indicate match
            let mut parse_property = |prop: &mut SimpleDeviceNode,
                                      _: &'static str,
                                      parser: &DtbParser,
                                      cursor: &mut usize|
             -> Result<(bool, Option<u32>), &'static str> {
                prop.parse_prop(parser, cursor, device_name, compatible_name)
                    .map(|b| (b, None))
            };

            // calcualte_property closure: emit addresses for matched node
            let mut calculate_property =
                |prop: &mut SimpleDeviceNode| -> Result<ControlFlow<()>, &'static str> {
                    for entry in DeviceAddressIter::new(prop) {
                        if f(entry?.0, entry?.1) == ControlFlow::Break(()) {
                            return Ok(ControlFlow::Break(()));
                        }
                    }
                    Ok(ControlFlow::Continue(()))
                };

            if self
                .walk_struct(
                    &mut pointer,
                    None::<&SimpleDeviceNode>,
                    &mut parse_property,
                    &mut calculate_property,
                    None,
                )?
                .is_continue()
            {
                self.skip_nop(&mut pointer);
                if Self::get_types(&pointer) != Self::FDT_END {
                    pr_debug!(
                        "failed to parse all of the dtb node: {:?}",
                        Self::get_types(&pointer)
                    );
                    return Err("struct block: did not end with FDT_END");
                }
            }
            Ok(())
        }

        pub fn find_memory_reservation_block<F>(&self, f: &mut F)
        where
            F: FnMut(usize, usize) -> ControlFlow<()>,
        {
            let mut ptr = self.dtb_header.get_memory_reservation_start_address();
            loop {
                let addr = FdtReserveEntry::get_address(ptr);
                let size = FdtReserveEntry::get_size(ptr);
                if addr == 0 && size == 0 {
                    return;
                }
                if f(addr as usize, size as usize) == ControlFlow::Break(()) {
                    return;
                }
                ptr += size_of::<FdtReserveEntry>();
            }
        }

        pub fn find_reserved_memory_node<F, D>(
            &self,
            f: &mut F,
            dynamic: &mut D,
        ) -> Result<(), &'static str>
        where
            F: FnMut(usize, usize) -> ControlFlow<()>,
            D: FnMut(usize, Option<usize>, Option<(usize, usize)>) -> Result<ControlFlow<()>, ()>,
        {
            let mut ptr = self.dtb_header.get_struct_start_address();
            self.skip_nop(&mut ptr);
            let mut parse_property = |prop: &mut ReservedMemoryNode,
                                      node_name: &'static str,
                                      parser: &DtbParser,
                                      cursor: &mut usize|
             -> Result<(bool, Option<u32>), &'static str> {
                pr_debug!("property parse start");
                *cursor += DtbParser::SIZEOF_FDT_TOKEN;
                let property = unsafe { &*(*cursor as *const FdtProperty) };
                *cursor += size_of::<FdtProperty>();
                let name = Dtb::read_char_str(
                    parser.dtb_header.get_string_start_address()
                        + property.get_name_offset() as usize,
                )?;
                if let ReservedMemoryNode::Unused(parent) = prop {
                    if let Some(parent) = parent
                        && let ReservedMemoryNode::Parent {
                            address_cells: _,
                            size_cells: _,
                        } = unsafe { &**parent }
                    {
                        *prop = ReservedMemoryNode::Child {
                            parent: *parent,
                            data: None,
                        };
                    } else if node_name == "reserved-memory" {
                        *prop = ReservedMemoryNode::Parent {
                            address_cells: 2,
                            size_cells: 1,
                        };
                    }
                }
                let mut matched_node = false;
                match prop {
                    ReservedMemoryNode::Parent {
                        address_cells,
                        size_cells,
                    } => {
                        pr_debug!("parent");
                        match name {
                            SimpleDeviceNode::ADDRESS_CELLS => {
                                *address_cells = Dtb::read_u32_from_ptr(*cursor)
                            }
                            SimpleDeviceNode::SIZE_CELLS => {
                                *size_cells = Dtb::read_u32_from_ptr(*cursor);
                            }
                            _ => {}
                        }
                    }
                    ReservedMemoryNode::Child { parent: _, data } => {
                        pr_debug!("child");
                        match name {
                            SimpleDeviceNode::PROP_REG => {
                                if data.is_some() {
                                    return Err("reserved-memory child: duplicate 'reg' property");
                                }
                                if property.get_property_len() != 0 {
                                    *data = Some(ReservedMemoryData::Static {
                                        reg: PropertyData {
                                            head_addr: *cursor,
                                            len: property.get_property_len(),
                                        },
                                    });
                                    matched_node = true;
                                }
                            }
                            SimpleDeviceNode::PROP_SIZE
                            | SimpleDeviceNode::PROP_ALIGNMENT
                            | SimpleDeviceNode::PROP_ALLOC_RANGES => {
                                if data.is_none() {
                                    *data = Some(ReservedMemoryData::Dynamic {
                                        size: None,
                                        alignment: None,
                                        alloc_ranges: None,
                                    });
                                }
                                let data = match data.as_mut().unwrap() {
                                    ReservedMemoryData::Static { reg: _ } => {
                                        return Err(
                                            "reserved-memory child: dynamic property with existing 'reg'",
                                        );
                                    }
                                    ReservedMemoryData::Dynamic {
                                        size,
                                        alignment,
                                        alloc_ranges,
                                    } => match name {
                                        SimpleDeviceNode::PROP_SIZE => size,
                                        SimpleDeviceNode::PROP_ALIGNMENT => alignment,
                                        SimpleDeviceNode::PROP_ALLOC_RANGES => alloc_ranges,
                                        _ => unreachable!(),
                                    },
                                };

                                if data.is_some() {
                                    return Err(
                                        "reserved-memory child: duplicate dynamic property",
                                    );
                                }
                                *data = Some(PropertyData {
                                    head_addr: *cursor,
                                    len: property.get_property_len(),
                                });
                                matched_node = true;
                            }
                            _ => {}
                        }
                    }
                    ReservedMemoryNode::Unused(_) => {}
                }

                // advance over property value (4-byte aligned)
                *cursor += property
                    .get_property_len()
                    .next_multiple_of(DtbParser::ALIGNMENT) as usize;

                // Only constrain depth when we are at reserved-memory
                let is_reserved_memory = node_name == "reserved-memory";
                if is_reserved_memory {
                    Ok((true, Some(1)))
                } else {
                    Ok((matched_node, None))
                }
            };
            let mut calculate_property =
                |prop: &mut ReservedMemoryNode| -> Result<ControlFlow<()>, &'static str> {
                    if let ReservedMemoryNode::Child { parent, data } = prop
                        && let Some(data) = data
                    {
                        let (address_cells, size_cells) =
                            unsafe { *&(**parent).assume_parent_and_get_property() }?;
                        if address_cells > 2 || size_cells > 2 {
                            return Err(
                                "reserved-memory: address-cells/size-cells > 2 not supported",
                            );
                        }
                        match data {
                            ReservedMemoryData::Static { reg } => {
                                let stride =
                                    (address_cells + size_cells) as usize * size_of::<u32>();
                                if reg.len == 0 || (reg.len as usize) % stride != 0 {
                                    return Err(
                                        "reserved-memory static: 'reg' length not multiple of stride",
                                    );
                                }
                                let mut consumed = 0;
                                loop {
                                    let addr =
                                        Dtb::read_regs(reg.head_addr + consumed, address_cells)?;
                                    consumed += addr.1;
                                    let size =
                                        Dtb::read_regs(reg.head_addr + consumed, size_cells)?;
                                    consumed += size.1;
                                    if f(addr.0, size.0).is_break() {
                                        return Ok(ControlFlow::Break(()));
                                    }
                                    if consumed == reg.len as usize {
                                        return Ok(ControlFlow::Continue(()));
                                    }
                                    if consumed > reg.len as usize {
                                        return Err(
                                            "reserved-memory static: overrun while reading 'reg' entries",
                                        );
                                    }
                                }
                            }
                            ReservedMemoryData::Dynamic {
                                size,
                                alignment,
                                alloc_ranges,
                            } => {
                                // Validate property lengths (bytes vs cells)
                                let sc_bytes = size_cells as usize * size_of::<u32>();
                                let ac_bytes = address_cells as usize * size_of::<u32>();
                                if size.is_none() {
                                    return Err("reserved-memory dynamic: missing 'size' property");
                                }
                                if size.as_ref().is_some_and(|x| x.len as usize != sc_bytes) {
                                    return Err("reserved-memory dynamic: 'size' length mismatch");
                                }
                                if alignment
                                    .as_ref()
                                    .is_some_and(|x| x.len as usize != sc_bytes)
                                {
                                    return Err(
                                        "reserved-memory dynamic: 'alignment' length mismatch",
                                    );
                                }
                                if alloc_ranges.as_ref().is_some_and(|x| {
                                    let stride = ac_bytes + sc_bytes;
                                    (x.len as usize) == 0 || (x.len as usize) % stride != 0
                                }) {
                                    return Err(
                                        "reserved-memory dynamic: 'alloc-ranges' length not multiple of stride",
                                    );
                                }
                                let size = size.as_ref().unwrap();
                                let alloc_size = Dtb::read_regs(size.head_addr, size_cells)?.0;
                                let alignment = if let Some(alignment) = alignment {
                                    Some(Dtb::read_regs(alignment.head_addr, size_cells)?.0)
                                } else {
                                    None
                                };
                                if let Some(alloc_ranges) = alloc_ranges {
                                    let mut consumed = 0;
                                    loop {
                                        let addr = Dtb::read_regs(
                                            alloc_ranges.head_addr + consumed,
                                            address_cells,
                                        )?;
                                        consumed += addr.1;
                                        let size = Dtb::read_regs(
                                            alloc_ranges.head_addr + consumed,
                                            size_cells,
                                        )?;
                                        consumed += size.1;
                                        if let Ok(result) =
                                            dynamic(alloc_size, alignment, Some((addr.0, size.0)))
                                        {
                                            return Ok(result);
                                        }
                                        if consumed == alloc_ranges.len as usize {
                                            return Ok(ControlFlow::Continue(()));
                                        }
                                        if consumed > alloc_ranges.len as usize {
                                            return Err(
                                                "reserved-memory dynamic: overrun while reading 'alloc-ranges'",
                                            );
                                        }
                                    }
                                } else {
                                    return dynamic(alloc_size, alignment, None)
                                        .or_else(|()| Ok(ControlFlow::Continue(())));
                                }
                            }
                        }
                    }
                    Ok(ControlFlow::Continue(()))
                };
            if self
                .walk_struct(
                    &mut ptr,
                    None::<&ReservedMemoryNode>,
                    &mut parse_property,
                    &mut calculate_property,
                    Some(1),
                )?
                .is_continue()
            {
                self.skip_nop(&mut ptr);
                if Self::get_types(&ptr) != Self::FDT_END {
                    pr_debug!(
                        "failed to parse all of the dtb node: {:?}",
                        Self::get_types(&ptr)
                    );
                    return Err("struct block: did not end with FDT_END");
                }
            }
            Ok(())
        }
    }

    mod big_endian {
        use core::ptr;

        #[allow(clippy::assertions_on_constants)]
        const _: () = assert!(size_of::<FdtProperty>() == 8);
        const _: () = assert!(size_of::<FdtReserveEntry>() == 16);

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
            pub fn get_address(ptr: usize) -> u64 {
                u64::from_be(unsafe { ptr::read_unaligned(ptr as *const u64) })
            }
            pub fn get_size(ptr: usize) -> u64 {
                u64::from_be(unsafe {
                    ptr::read_unaligned((ptr + 8/* address size */) as *const u64)
                })
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
    use std::path::PathBuf;

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
            .find_node(None, Some("arm,pl011"), &mut |address, size| {
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
            .find_node(Some("memory"), None, &mut |address, size| {
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
            .find_node(None, Some("arm,gic-400"), &mut |address, size| {
                pr_debug!("find gic node, address: {} size: {}", address, size);
                counter += 1;
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(counter, 4);
    }

    #[test]
    fn reserved_memory_generated_dtb() {
        // The build script places compiled DTBs in OUT_DIR
        let out_dir = env!("OUT_DIR");
        let mut path = PathBuf::from(out_dir);
        path.push("reserved_memory.dtb");
        assert!(
            path.exists(),
            "{} not found. dtc is required to build DTS fixtures.",
            path.display()
        );
        let test_data = std::fs::read(&path).expect("failed to load generated dtb file");
        let test_data_addr = test_data.as_ptr() as usize;
        let parser = DtbParser::init(test_data_addr).unwrap();

        let mut captured: Option<(usize, usize)> = None;
        parser
            .find_reserved_memory_node(
                &mut |addr, size| {
                    captured = Some((addr, size));
                    ControlFlow::Break(())
                },
                &mut |_, _, _| -> Result<ControlFlow<()>, ()> { unreachable!() },
            )
            .unwrap();
        let (addr, size) = captured.expect("no reserved-memory region found");
        assert_eq!(addr, 0x20);
        assert_eq!(size, 0x10);
    }

    #[test]
    fn reserved_memory_dynamic_generated_dtb() {
        // The build script places compiled DTBs in OUT_DIR
        let out_dir = env!("OUT_DIR");
        let mut path = PathBuf::from(out_dir);
        path.push("reserved_memory_dynamic.dtb");
        assert!(
            path.exists(),
            "{} not found. dtc is required to build DTS fixtures.",
            path.display()
        );

        let test_data = std::fs::read(&path).expect("failed to load generated dtb file");
        let test_data_addr = test_data.as_ptr() as usize;
        let parser = DtbParser::init(test_data_addr).unwrap();

        // Static regions should not be called in this DTS
        let mut static_called = false;
        // Capture dynamic result
        let mut dynamic_captured: Option<(usize, Option<usize>, Option<(usize, usize)>)> = None;

        parser
            .find_reserved_memory_node(
                &mut |addr, size| {
                    static_called = true;
                    pr_debug!(
                        "unexpected static reserved-memory: {:#x}, {:#x}",
                        addr,
                        size
                    );
                    ControlFlow::Continue(())
                },
                &mut |alloc_size, alignment, alloc_range| {
                    dynamic_captured = Some((alloc_size, alignment, alloc_range));
                    Ok(ControlFlow::Break(()))
                },
            )
            .expect("failed to parse reserved-memory node");

        assert!(
            !static_called,
            "static reserved-memory entry unexpectedly called"
        );
        let (alloc_size, alignment, alloc_range) =
            dynamic_captured.expect("no dynamic reserved-memory captured");

        assert_eq!(alloc_size, 0x0001_0000);
        assert_eq!(alignment, Some(0x0001_0000));
        assert_eq!(alloc_range, Some((0x4000_0000, 0x1000_0000)));
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
