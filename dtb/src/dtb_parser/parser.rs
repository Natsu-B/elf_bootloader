use core::mem::size_of;
use core::ops::ControlFlow;

use super::big_endian::Dtb;
use super::big_endian::FdtProperty;
use super::big_endian::FdtReserveEntry;
use super::types::DTB_ALIGN;
use super::types::NodeScope;
use super::types::TOKEN_SIZE;
use super::types::Unchecked;
use super::types::Validated;
use super::types::WalkError;
use super::types::WalkResult;
use super::types::read_u32_be;
use super::view::DtbNodeView;

/// DTB parser with typestate for header validation.
pub struct DtbParser<State = Validated> {
    dtb: Dtb<State>,
}

impl DtbParser<Validated> {
    pub fn init(dtb_address: usize) -> Result<Self, &'static str> {
        Ok(Self {
            dtb: Dtb::new(dtb_address)?,
        })
    }
}

impl DtbParser<Unchecked> {
    pub fn init_unchecked(dtb_address: usize) -> Result<Self, &'static str> {
        Ok(Self {
            dtb: Dtb::new_unchecked(dtb_address)?,
        })
    }

    pub fn validate(self) -> Result<DtbParser<Validated>, &'static str> {
        Ok(DtbParser {
            dtb: self.dtb.validate()?,
        })
    }
}

impl DtbParser<Validated> {
    pub(crate) const TOKEN_SIZE: usize = TOKEN_SIZE;

    pub(crate) const FDT_BEGIN_NODE: [u8; 4] = [0, 0, 0, 1];
    pub(crate) const FDT_END_NODE: [u8; 4] = [0, 0, 0, 2];
    pub(crate) const FDT_PROP: [u8; 4] = [0, 0, 0, 3];
    pub(crate) const FDT_NOP: [u8; 4] = [0, 0, 0, 4];
    pub(crate) const FDT_END: [u8; 4] = [0, 0, 0, 9];

    pub fn dtb_header(&self) -> &Dtb<Validated> {
        &self.dtb
    }

    pub fn get_size(&self) -> usize {
        self.dtb.total_size() as usize
    }

    pub(crate) fn token_at(address: usize) -> [u8; 4] {
        unsafe { *(address as *const [u8; 4]) }
    }

    pub(crate) fn read_token(&self, address: usize, end: usize) -> Result<[u8; 4], &'static str> {
        let next = address.checked_add(TOKEN_SIZE).ok_or("token: overflow")?;
        if next > end {
            return Err("token: overrun");
        }
        Ok(Self::token_at(address))
    }

    fn skip_nop(&self, cursor: &mut usize, end: usize) -> Result<(), &'static str> {
        while *cursor < end && Self::token_at(*cursor) == Self::FDT_NOP {
            *cursor += TOKEN_SIZE;
        }
        Ok(())
    }

    pub(crate) fn read_cstr_in_range(
        &self,
        start: usize,
        end: usize,
    ) -> Result<(&'static str, usize), &'static str> {
        if start >= end {
            return Err("cstr: empty range");
        }
        let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, end - start) };
        let nul = bytes
            .iter()
            .position(|&b| b == 0)
            .ok_or("cstr: missing nul")?;
        let s = core::str::from_utf8(&bytes[..nul]).map_err(|_| "cstr: invalid utf8")?;
        Ok((s, nul + 1))
    }

    pub(crate) fn node_name(
        &self,
        node_begin: usize,
        node_end: usize,
    ) -> Result<&'static str, &'static str> {
        if self.read_token(node_begin, node_end)? != Self::FDT_BEGIN_NODE {
            return Err("node_name: expected BEGIN_NODE");
        }
        let name_start = node_begin + TOKEN_SIZE;
        let (name, _) = self.read_cstr_in_range(name_start, node_end)?;
        Ok(name)
    }

    pub(crate) fn scan_node_properties(
        &self,
        node_begin: usize,
        node_end: usize,
        key: Option<&str>,
    ) -> Result<(Option<&'static [u8]>, usize), &'static str> {
        if self.read_token(node_begin, node_end)? != Self::FDT_BEGIN_NODE {
            return Err("node_props: expected BEGIN_NODE");
        }

        let name_start = node_begin + TOKEN_SIZE;
        let (_, name_len) = self.read_cstr_in_range(name_start, node_end)?;
        let padded = name_len.next_multiple_of(DTB_ALIGN);
        let mut cursor = name_start
            .checked_add(padded)
            .ok_or("node_props: name overflow")?;
        if cursor > node_end {
            return Err("node_props: name overrun");
        }

        let mut found: Option<&'static [u8]> = None;

        loop {
            if cursor >= node_end {
                return Err("node_props: unexpected end");
            }
            let token = self.read_token(cursor, node_end)?;
            if token == Self::FDT_NOP {
                cursor += TOKEN_SIZE;
                continue;
            }
            if token == Self::FDT_PROP {
                cursor += TOKEN_SIZE;

                let header_end = cursor
                    .checked_add(size_of::<FdtProperty>())
                    .ok_or("node_props: hdr overflow")?;
                if header_end > node_end {
                    return Err("node_props: hdr overrun");
                }
                let prop = unsafe { &*(cursor as *const FdtProperty) };
                cursor = header_end;

                let len = prop.len() as usize;
                let value_end = cursor.checked_add(len).ok_or("node_props: len overflow")?;
                if value_end > node_end {
                    return Err("node_props: value overrun");
                }

                let name = self
                    .dtb
                    .read_cstr_from_strings(prop.name_offset() as usize)?;
                if found.is_none() {
                    if let Some(key) = key {
                        if key == name {
                            found = Some(unsafe {
                                core::slice::from_raw_parts(cursor as *const u8, len)
                            });
                        }
                    }
                }

                let padded_len = len.next_multiple_of(DTB_ALIGN);
                cursor = cursor
                    .checked_add(padded_len)
                    .ok_or("node_props: pad overflow")?;
                if cursor > node_end {
                    return Err("node_props: pad overrun");
                }
                continue;
            }

            if token == Self::FDT_BEGIN_NODE || token == Self::FDT_END_NODE {
                break;
            }
            return Err("node_props: unexpected token");
        }

        Ok((found, cursor))
    }

    pub(crate) fn node_property_bytes(
        &self,
        node_begin: usize,
        node_end: usize,
        key: &str,
    ) -> Result<Option<&'static [u8]>, &'static str> {
        let (prop, _) = self.scan_node_properties(node_begin, node_end, Some(key))?;
        Ok(prop)
    }

    pub(crate) fn node_property_u32_be(
        &self,
        node_begin: usize,
        node_end: usize,
        key: &str,
    ) -> Result<Option<u32>, &'static str> {
        let Some(bytes) = self.node_property_bytes(node_begin, node_end, key)? else {
            return Ok(None);
        };
        if bytes.len() != size_of::<u32>() {
            return Err("property_u32: length != 4");
        }
        Ok(Some(read_u32_be(bytes)?))
    }

    pub(crate) fn node_view_from_scope<'dtb, 's>(
        &'dtb self,
        scope: NodeScope,
        ancestors: &'s [NodeScope],
    ) -> Result<DtbNodeView<'dtb, 's>, &'static str> {
        let name = self.node_name(scope.begin, scope.end)?;
        Ok(DtbNodeView {
            parser: self,
            begin: scope.begin,
            end: scope.end,
            name,
            ancestors,
        })
    }

    pub(crate) fn skip_node(&self, cursor: &mut usize, end: usize) -> Result<(), &'static str> {
        if self.read_token(*cursor, end)? != Self::FDT_BEGIN_NODE {
            return Err("skip_node: expected BEGIN_NODE");
        }

        let mut nest = 0usize;
        loop {
            if *cursor >= end {
                return Err("skip_node: overrun");
            }
            match Self::token_at(*cursor) {
                t if t == Self::FDT_NOP => {
                    *cursor += TOKEN_SIZE;
                }
                t if t == Self::FDT_BEGIN_NODE => {
                    *cursor += TOKEN_SIZE;
                    let (name, name_len) = self.read_cstr_in_range(*cursor, end)?;
                    let padded = name_len.next_multiple_of(DTB_ALIGN);
                    let _ = name;
                    *cursor = cursor
                        .checked_add(padded)
                        .ok_or("skip_node: name overflow")?;
                    nest += 1;
                }
                t if t == Self::FDT_PROP => {
                    *cursor += TOKEN_SIZE;
                    if *cursor + size_of::<FdtProperty>() > end {
                        return Err("skip_node: prop hdr overrun");
                    }
                    let prop = unsafe { &*(*cursor as *const FdtProperty) };
                    *cursor += size_of::<FdtProperty>();
                    let len = prop.len() as usize;
                    let padded = len.next_multiple_of(DTB_ALIGN);
                    *cursor = cursor
                        .checked_add(padded)
                        .ok_or("skip_node: prop overflow")?;
                }
                t if t == Self::FDT_END_NODE => {
                    *cursor += TOKEN_SIZE;
                    nest = nest.saturating_sub(1);
                    if nest == 0 {
                        break;
                    }
                }
                _ => return Err("skip_node: unexpected token"),
            }
        }
        Ok(())
    }

    pub fn root_node_view(&self) -> Result<DtbNodeView<'_, 'static>, &'static str> {
        let mut cursor = self.dtb.struct_start();
        let end = self.dtb.struct_end();
        self.skip_nop(&mut cursor, end)?;
        if self.read_token(cursor, end)? != Self::FDT_BEGIN_NODE {
            return Err("root: expected BEGIN_NODE");
        }
        let begin = cursor;
        let mut node_end = begin;
        self.skip_node(&mut node_end, end)?;
        let name = self.node_name(begin, node_end)?;
        Ok(DtbNodeView {
            parser: self,
            begin,
            end: node_end,
            name,
            ancestors: &[],
        })
    }

    pub fn for_each_node_view<T, E, F>(&self, f: &mut F) -> WalkResult<T, E>
    where
        F: for<'s> FnMut(DtbNodeView<'_, 's>) -> WalkResult<T, E>,
    {
        fn walk<'dtb, 's, T, E, F>(node: DtbNodeView<'dtb, 's>, f: &mut F) -> WalkResult<T, E>
        where
            F: for<'cs> FnMut(DtbNodeView<'dtb, 'cs>) -> WalkResult<T, E>,
        {
            match f(node)? {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(value) => return Ok(ControlFlow::Break(value)),
            }

            let child_result = node.for_each_child_view(&mut |child| walk(child, f))?;
            match child_result {
                ControlFlow::Continue(()) => Ok(ControlFlow::Continue(())),
                ControlFlow::Break(value) => Ok(ControlFlow::Break(value)),
            }
        }

        let root = self.root_node_view().map_err(WalkError::Dtb)?;
        walk(root, f)
    }

    pub fn find_node_view_by_phandle(
        &self,
        phandle: u32,
    ) -> Result<Option<DtbNodeView<'_, 'static>>, &'static str> {
        let result = self.for_each_node_view(&mut |node| {
            let primary = node.property_u32_be("phandle").map_err(WalkError::Dtb)?;
            let secondary = node
                .property_u32_be("linux,phandle")
                .map_err(WalkError::Dtb)?;
            if primary == Some(phandle) || secondary == Some(phandle) {
                let (begin, end) = node.struct_range();
                return Ok(ControlFlow::Break((begin, end, node.name())));
            }
            Ok(ControlFlow::Continue(()))
        });

        match result {
            Ok(ControlFlow::Continue(())) => Ok(None),
            Ok(ControlFlow::Break((begin, end, name))) => Ok(Some(DtbNodeView {
                parser: self,
                begin,
                end,
                name,
                ancestors: &[],
            })),
            Err(WalkError::Dtb(err)) => Err(err),
            Err(WalkError::User(())) => Err("find_node_view_by_phandle: unexpected user error"),
        }
    }

    pub fn with_node_view_by_phandle<F, R>(
        &self,
        phandle: u32,
        f: &mut F,
    ) -> Result<Option<R>, &'static str>
    where
        F: for<'a, 's> FnMut(DtbNodeView<'a, 's>) -> Result<R, &'static str>,
    {
        let result = self.for_each_node_view(&mut |node| {
            let primary = node.property_u32_be("phandle").map_err(WalkError::Dtb)?;
            let secondary = node
                .property_u32_be("linux,phandle")
                .map_err(WalkError::Dtb)?;
            if primary == Some(phandle) || secondary == Some(phandle) {
                let value = f(node).map_err(WalkError::User)?;
                return Ok(ControlFlow::Break(value));
            }
            Ok(ControlFlow::Continue(()))
        });

        match result {
            Ok(ControlFlow::Continue(())) => Ok(None),
            Ok(ControlFlow::Break(value)) => Ok(Some(value)),
            Err(WalkError::Dtb(err)) => Err(err),
            Err(WalkError::User(err)) => Err(err),
        }
    }

    pub fn find_nodes_by_compatible_view<T, E>(
        &self,
        compatible_name: &str,
        f: &mut impl FnMut(&DtbNodeView<'_, '_>, &'static str) -> WalkResult<T, E>,
    ) -> WalkResult<T, E> {
        self.for_each_node_view(&mut |node| {
            if node
                .compatible_contains(compatible_name)
                .map_err(WalkError::Dtb)?
            {
                return f(&node, node.name());
            }
            Ok(ControlFlow::Continue(()))
        })
    }

    pub fn find_node<T, E, F>(
        &self,
        device_name: Option<&str>,
        compatible_name: Option<&str>,
        f: &mut F,
    ) -> WalkResult<T, E>
    where
        F: FnMut(usize, usize) -> WalkResult<T, E>,
    {
        if (device_name.is_some() && compatible_name.is_some())
            || (device_name.is_none() && compatible_name.is_none())
        {
            return Err(WalkError::Dtb(
                "find_node: specify exactly one of device_name/compatible_name",
            ));
        }

        self.for_each_node_view(&mut |node| {
            let matched = if let Some(dev) = device_name {
                let prop = node.property_bytes("device_type").map_err(WalkError::Dtb)?;
                match prop {
                    Some(bytes) => {
                        core::str::from_utf8(bytes.split(|b| *b == 0).next().unwrap_or(bytes))
                            .ok()
                            .is_some_and(|s| s == dev)
                    }
                    None => false,
                }
            } else if let Some(comp) = compatible_name {
                node.compatible_contains(comp).map_err(WalkError::Dtb)?
            } else {
                false
            };

            if !matched {
                return Ok(ControlFlow::Continue(()));
            }

            let mut it = node.reg_iter().map_err(WalkError::Dtb)?;
            while let Some(r) = it.next() {
                let (addr, size) = r.map_err(WalkError::Dtb)?;
                match f(addr, size)? {
                    ControlFlow::Continue(()) => {}
                    ControlFlow::Break(value) => return Ok(ControlFlow::Break(value)),
                }
            }
            Ok(ControlFlow::Continue(()))
        })
    }

    pub fn find_memory_reservation_block<F>(&self, f: &mut F)
    where
        F: FnMut(usize, usize) -> ControlFlow<()>,
    {
        let mut ptr = self.dtb.mem_rsvmap_start();
        loop {
            let addr = FdtReserveEntry::get_address(ptr);
            let size = FdtReserveEntry::get_size(ptr);
            if addr == 0 && size == 0 {
                return;
            }
            if f(addr as usize, size as usize).is_break() {
                return;
            }
            ptr += size_of::<FdtReserveEntry>();
        }
    }

    pub fn property_u32_be_by_phandle(
        &self,
        phandle: u32,
        key: &str,
    ) -> Result<Option<u32>, &'static str> {
        self.with_node_view_by_phandle(phandle, &mut |node| node.property_u32_be(key))
            .map(Option::flatten)
    }

    pub fn find_reserved_memory_node<E, F, D>(
        &self,
        f: &mut F,
        dynamic: &mut D,
    ) -> WalkResult<(), E>
    where
        F: FnMut(usize, usize) -> WalkResult<(), E>,
        D: FnMut(usize, Option<usize>, Option<(usize, usize)>) -> WalkResult<(), E>,
    {
        self.for_each_node_view(&mut |node| {
            if node.name() != "reserved-memory" {
                return Ok(ControlFlow::Continue(()));
            }

            let _ = node.for_each_child_view(&mut |child| {
                let reg = child.property_bytes("reg").map_err(WalkError::Dtb)?;
                if reg.is_some() {
                    let mut it = child.reg_iter().map_err(WalkError::Dtb)?;
                    while let Some(r) = it.next() {
                        let (addr, size) = r.map_err(WalkError::Dtb)?;
                        match f(addr, size)? {
                            ControlFlow::Continue(()) => {}
                            ControlFlow::Break(value) => return Ok(ControlFlow::Break(value)),
                        }
                    }
                    return Ok(ControlFlow::Continue(()));
                }

                let size = match child.property_bytes("size").map_err(WalkError::Dtb)? {
                    Some(b) => b,
                    None => return Ok(ControlFlow::Continue(())),
                };
                let cells = child.parent_size_cells().map_err(WalkError::Dtb)?;
                let sc_bytes = (cells as usize) * size_of::<u32>();
                if size.len() != sc_bytes {
                    return Err(WalkError::Dtb("reserved-memory: invalid size length"));
                }

                let alloc_size = super::types::read_regs_from_bytes(size, cells)
                    .map_err(WalkError::Dtb)?
                    .0;

                let alignment = match child.property_bytes("alignment").map_err(WalkError::Dtb)? {
                    Some(b) => Some(
                        super::types::read_regs_from_bytes(b, cells)
                            .map_err(WalkError::Dtb)?
                            .0,
                    ),
                    None => None,
                };

                match child
                    .property_bytes("alloc-ranges")
                    .map_err(WalkError::Dtb)?
                {
                    Some(ar) => {
                        let ac = child.parent_address_cells().map_err(WalkError::Dtb)?;
                        let stride = (ac as usize + cells as usize) * size_of::<u32>();
                        if stride == 0 || ar.len() % stride != 0 {
                            return Err(WalkError::Dtb("reserved-memory: invalid alloc-ranges"));
                        }

                        let mut off = 0usize;
                        while off < ar.len() {
                            let (addr, a_len) = super::types::read_regs_from_bytes(&ar[off..], ac)
                                .map_err(WalkError::Dtb)?;
                            let (len, l_len) =
                                super::types::read_regs_from_bytes(&ar[off + a_len..], cells)
                                    .map_err(WalkError::Dtb)?;
                            off += a_len + l_len;

                            match dynamic(alloc_size, alignment, Some((addr, len)))? {
                                ControlFlow::Continue(()) => {}
                                ControlFlow::Break(value) => return Ok(ControlFlow::Break(value)),
                            }
                        }
                    }
                    None => match dynamic(alloc_size, alignment, None)? {
                        ControlFlow::Continue(()) => {}
                        ControlFlow::Break(value) => return Ok(ControlFlow::Break(value)),
                    },
                }

                Ok(ControlFlow::Continue(()))
            })?;

            Ok(ControlFlow::Break(()))
        })
    }
}
