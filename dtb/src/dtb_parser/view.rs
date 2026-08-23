use core::convert::TryFrom;
use core::mem::size_of;
use core::ops::ControlFlow;

use super::iters::InterruptCellsIter;
use super::iters::RangesIter;
use super::iters::RangesIterWide;
use super::iters::RegIter;
use super::iters::RegRawIter;
use super::parser::DtbParser;
use super::types::NodeScope;
use super::types::Validated;
use super::types::WalkError;
use super::types::WalkResult;
use super::types::read_u32_be;

#[derive(Clone, Copy)]
pub struct DtbNodeView<'dtb, 's> {
    pub(crate) parser: &'dtb DtbParser<Validated>,
    pub(crate) begin: usize,
    pub(crate) end: usize,
    pub(crate) name: &'static str,
    pub(crate) ancestors: &'s [NodeScope],
}

impl<'dtb, 's> DtbNodeView<'dtb, 's> {
    const PROP_ADDRESS_CELLS: &'static str = "#address-cells";
    const PROP_SIZE_CELLS: &'static str = "#size-cells";
    const PROP_RANGES: &'static str = "ranges";
    const PROP_DMA_RANGES: &'static str = "dma-ranges";
    const PROP_COMPATIBLE: &'static str = "compatible";
    const PROP_INTERRUPTS: &'static str = "interrupts";
    const PROP_INTERRUPT_CELLS: &'static str = "#interrupt-cells";
    const PROP_INTERRUPT_PARENT: &'static str = "interrupt-parent";
    const PROP_MSI_PARENT: &'static str = "msi-parent";
    const PROP_PHANDLE: &'static str = "phandle";
    const PROP_LINUX_PHANDLE: &'static str = "linux,phandle";
    const MAX_INTERRUPT_CELLS: usize = 4;

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn struct_range(&self) -> (usize, usize) {
        (self.begin, self.end)
    }

    pub fn property_bytes(&self, key: &str) -> Result<Option<&[u8]>, &'static str> {
        self.parser.node_property_bytes(self.begin, self.end, key)
    }

    pub fn property_u32_be(&self, key: &str) -> Result<Option<u32>, &'static str> {
        let Some(bytes) = self.property_bytes(key)? else {
            return Ok(None);
        };
        if bytes.len() != size_of::<u32>() {
            return Err("property length is not 4 bytes");
        }
        Ok(Some(read_u32_be(bytes)?))
    }

    pub fn compatible_contains(&self, needle: &str) -> Result<bool, &'static str> {
        let Some(bytes) = self.property_bytes(Self::PROP_COMPATIBLE)? else {
            return Ok(false);
        };

        let mut start = 0usize;
        while start < bytes.len() {
            let nul = bytes[start..]
                .iter()
                .position(|&b| b == 0)
                .ok_or("compatible: missing nul")?;
            let end = start + nul;
            let entry =
                core::str::from_utf8(&bytes[start..end]).map_err(|_| "compatible: invalid utf8")?;
            if entry == needle {
                return Ok(true);
            }
            start = end + 1;
        }
        Ok(false)
    }

    pub fn reg_iter(&self) -> Result<RegIter<'_, 'dtb, 's>, &'static str> {
        RegIter::new(self)
    }

    pub fn reg_raw_iter(&self) -> Result<RegRawIter<'_, 'dtb, 's>, &'static str> {
        RegRawIter::new(self)
    }

    pub fn interrupts_iter<const CELLS: usize>(
        &self,
    ) -> Result<Option<InterruptCellsIter<'_, CELLS>>, &'static str> {
        match self.property_bytes(Self::PROP_INTERRUPTS)? {
            Some(prop) => InterruptCellsIter::new(prop).map(Some),
            None => Ok(None),
        }
    }

    /// Note: returned view has empty ancestors; use with_interrupt_parent_view for ancestors.
    pub fn interrupt_parent(&self) -> Result<Option<DtbNodeView<'dtb, 'static>>, &'static str> {
        let Some(phandle) = self.interrupt_parent_phandle()? else {
            return Ok(None);
        };
        let controller = self
            .parser
            .find_node_view_by_phandle(phandle)?
            .ok_or("interrupt-parent: controller not found")?;
        Ok(Some(controller))
    }

    pub fn interrupt_parent_phandle(&self) -> Result<Option<u32>, &'static str> {
        self.inherited_u32_be(Self::PROP_INTERRUPT_PARENT)
    }

    pub fn with_interrupt_parent_view<F, R>(&self, f: &mut F) -> Result<Option<R>, &'static str>
    where
        F: for<'a, 'cs> FnMut(DtbNodeView<'a, 'cs>) -> Result<R, &'static str>,
    {
        let Some(phandle) = self.interrupt_parent_phandle()? else {
            return Ok(None);
        };
        match self.parser.with_node_view_by_phandle(phandle, f)? {
            Some(value) => Ok(Some(value)),
            None => Err("interrupt-parent: controller not found"),
        }
    }

    pub fn msi_parent_phandle(&self) -> Result<Option<u32>, &'static str> {
        self.inherited_u32_be(Self::PROP_MSI_PARENT)
    }

    /// Visit the MSI parent controller with a view that preserves ancestors.
    ///
    /// - If `msi-parent` is missing, returns `Ok(ControlFlow::Continue(()))`.
    /// - If the controller node is missing, returns
    ///   `Err(WalkError::Dtb("msi-parent: controller not found"))`.
    /// - If found, calls `f(&controller, controller.name())` exactly once, stops scanning,
    ///   and returns its result.
    pub fn with_msi_parent_view<T, E>(
        &self,
        f: &mut impl FnMut(&DtbNodeView<'_, '_>, &'static str) -> WalkResult<T, E>,
    ) -> WalkResult<T, E> {
        let Some(phandle) = self.msi_parent_phandle().map_err(WalkError::Dtb)? else {
            return Ok(ControlFlow::Continue(()));
        };

        let result = self.parser.for_each_node_view(&mut |node| {
            let primary = node
                .property_u32_be(Self::PROP_PHANDLE)
                .map_err(WalkError::Dtb)?;
            let secondary = node
                .property_u32_be(Self::PROP_LINUX_PHANDLE)
                .map_err(WalkError::Dtb)?;
            if primary == Some(phandle) || secondary == Some(phandle) {
                let result = f(&node, node.name())?;
                return Ok(ControlFlow::Break(result));
            }
            Ok(ControlFlow::Continue(()))
        });

        match result {
            Ok(ControlFlow::Break(value)) => Ok(value),
            Ok(ControlFlow::Continue(())) => {
                Err(WalkError::Dtb("msi-parent: controller not found"))
            }
            Err(err) => Err(err),
        }
    }

    pub fn phandle(&self) -> Option<u32> {
        self.property_u32_be(Self::PROP_PHANDLE)
            .ok()
            .flatten()
            .or_else(|| {
                self.property_u32_be(Self::PROP_LINUX_PHANDLE)
                    .ok()
                    .flatten()
            })
    }

    pub fn interrupt_cells(&self) -> Result<Option<u32>, &'static str> {
        let result = self.with_interrupt_parent_view(&mut |controller| {
            controller.property_u32_be(Self::PROP_INTERRUPT_CELLS)
        })?;
        Ok(result.flatten())
    }

    pub fn for_each_interrupt_specifier<T, E, F>(&self, f: &mut F) -> WalkResult<T, E>
    where
        F: FnMut(&[u32]) -> WalkResult<T, E>,
    {
        let Some(data) = self
            .property_bytes(Self::PROP_INTERRUPTS)
            .map_err(WalkError::Dtb)?
        else {
            return Ok(ControlFlow::Continue(()));
        };
        let cells = self
            .interrupt_cells()
            .map_err(WalkError::Dtb)?
            .ok_or(WalkError::Dtb("interrupts: missing #interrupt-cells"))?;
        let cells = usize::try_from(cells)
            .map_err(|_| WalkError::Dtb("interrupts: cell count overflow"))?;
        if cells == 0 || cells > Self::MAX_INTERRUPT_CELLS {
            return Err(WalkError::Dtb("interrupts: invalid cell count"));
        }

        let stride = cells
            .checked_mul(size_of::<u32>())
            .ok_or(WalkError::Dtb("interrupts: stride overflow"))?;
        if data.len() % stride != 0 {
            return Err(WalkError::Dtb(
                "interrupts: length not multiple of cell count",
            ));
        }

        let mut offset = 0usize;
        let mut decoded = [0u32; Self::MAX_INTERRUPT_CELLS];
        while offset < data.len() {
            for i in 0..cells {
                let base = offset + i * size_of::<u32>();
                let chunk = &data[base..base + size_of::<u32>()];
                decoded[i] = read_u32_be(chunk).map_err(WalkError::Dtb)?;
            }
            match f(&decoded[..cells])? {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(value) => return Ok(ControlFlow::Break(value)),
            }
            offset += stride;
        }
        Ok(ControlFlow::Continue(()))
    }

    pub fn has_ranges(&self) -> bool {
        self.property_bytes(Self::PROP_RANGES)
            .map(|p| p.is_some())
            .unwrap_or(false)
    }

    pub fn parent_view(&self) -> Result<Option<DtbNodeView<'dtb, 's>>, &'static str> {
        let Some((parent_scope, parent_ancestors)) = self.ancestors.split_last() else {
            return Ok(None);
        };
        let parent = self
            .parser
            .node_view_from_scope(*parent_scope, parent_ancestors)?;
        Ok(Some(parent))
    }

    pub fn ranges_iter(&self) -> Result<Option<RangesIter<'_>>, &'static str> {
        self.mapping_ranges_iter(Self::PROP_RANGES, "ranges: missing parent")
    }

    pub fn dma_ranges_iter(&self) -> Result<Option<RangesIter<'_>>, &'static str> {
        self.mapping_ranges_iter(Self::PROP_DMA_RANGES, "dma-ranges: missing parent")
    }

    // Build an iterator for a non-empty address mapping property.
    fn mapping_ranges_iter(
        &self,
        property: &str,
        missing_parent: &'static str,
    ) -> Result<Option<RangesIter<'_>>, &'static str> {
        // Missing and empty mapping properties both denote identity.
        let Some(prop) = self
            .property_bytes(property)?
            .filter(|prop| !prop.is_empty())
        else {
            return Ok(None);
        };
        let parent_address_cells =
            self.parent_cells(Self::PROP_ADDRESS_CELLS, 2, missing_parent)?;
        let child_address_cells = self.address_cells_result()?;
        let child_size_cells = self.size_cells_result()?;

        RangesIter::new(
            child_address_cells,
            parent_address_cells,
            child_size_cells,
            prop,
        )
        .map(Some)
    }

    pub fn address_cells(&self) -> u32 {
        self.address_cells_result().unwrap_or(2)
    }

    pub fn size_cells(&self) -> u32 {
        self.size_cells_result().unwrap_or(1)
    }

    pub fn for_each_child_view<T, E, F>(&self, f: &mut F) -> WalkResult<T, E>
    where
        F: for<'cs> FnMut(DtbNodeView<'dtb, 'cs>) -> WalkResult<T, E>,
    {
        let (_, mut cursor) = self
            .parser
            .scan_node_properties(self.begin, self.end, None)
            .map_err(WalkError::Dtb)?;
        const MAX_DEPTH: usize = 32;

        while cursor < self.end {
            let token = self
                .parser
                .read_token(cursor, self.end)
                .map_err(WalkError::Dtb)?;
            if token == DtbParser::FDT_NOP {
                cursor += DtbParser::TOKEN_SIZE;
                continue;
            }
            if token == DtbParser::FDT_BEGIN_NODE {
                let child_begin = cursor;
                let mut child_end = child_begin;
                self.parser
                    .skip_node(&mut child_end, self.end)
                    .map_err(WalkError::Dtb)?;
                let child_name = self
                    .parser
                    .node_name(child_begin, child_end)
                    .map_err(WalkError::Dtb)?;

                let depth = self.ancestors.len();
                if depth + 1 > MAX_DEPTH {
                    return Err(WalkError::Dtb("node depth exceeded"));
                }
                let mut scopes = [NodeScope { begin: 0, end: 0 }; MAX_DEPTH];
                scopes[..depth].copy_from_slice(self.ancestors);
                scopes[depth] = NodeScope {
                    begin: self.begin,
                    end: self.end,
                };

                let view = DtbNodeView {
                    parser: self.parser,
                    begin: child_begin,
                    end: child_end,
                    name: child_name,
                    ancestors: &scopes[..depth + 1],
                };
                match f(view)? {
                    ControlFlow::Continue(()) => {}
                    ControlFlow::Break(value) => return Ok(ControlFlow::Break(value)),
                };
                cursor = child_end;
                continue;
            }
            if token == DtbParser::FDT_END_NODE {
                return Ok(ControlFlow::Continue(()));
            }
            return Err(WalkError::Dtb("child traversal: unexpected token"));
        }
        Err(WalkError::Dtb("child traversal: unexpected end"))
    }

    pub(crate) fn parent_address_cells(&self) -> Result<u32, &'static str> {
        self.parent_cells(Self::PROP_ADDRESS_CELLS, 2, "'reg' at root is invalid")
    }

    pub(crate) fn parent_size_cells(&self) -> Result<u32, &'static str> {
        self.parent_cells(Self::PROP_SIZE_CELLS, 1, "'reg' at root is invalid")
    }

    fn inherited_u32_be(&self, key: &str) -> Result<Option<u32>, &'static str> {
        if let Some(v) = self.property_u32_be(key)? {
            return Ok(Some(v));
        }
        self.ancestor_u32_be(key)
    }

    fn ancestor_u32_be(&self, key: &str) -> Result<Option<u32>, &'static str> {
        for scope in self.ancestors.iter().rev() {
            if let Some(v) = self
                .parser
                .node_property_u32_be(scope.begin, scope.end, key)?
            {
                return Ok(Some(v));
            }
        }
        Ok(None)
    }

    fn parent_cells(
        &self,
        key: &str,
        default: u32,
        missing: &'static str,
    ) -> Result<u32, &'static str> {
        if self.ancestors.is_empty() {
            return Err(missing);
        }
        Ok(self.ancestor_u32_be(key)?.unwrap_or(default))
    }

    fn address_cells_result(&self) -> Result<u32, &'static str> {
        Ok(self
            .inherited_u32_be(Self::PROP_ADDRESS_CELLS)?
            .unwrap_or(2))
    }

    fn size_cells_result(&self) -> Result<u32, &'static str> {
        Ok(self.inherited_u32_be(Self::PROP_SIZE_CELLS)?.unwrap_or(1))
    }

    fn translate_one_level_wide(&self, child: (u128, u128)) -> Result<(u128, u128), &'static str> {
        let ranges = match self.property_bytes(Self::PROP_RANGES)? {
            Some(r) => r,
            None => return Ok(child),
        };
        if ranges.is_empty() {
            return Ok(child); // empty => identity
        }

        let parent_address_cells =
            self.parent_cells(Self::PROP_ADDRESS_CELLS, 2, "ranges: missing parent")?;
        let child_address_cells = self.address_cells_result()?;
        let child_size_cells = self.size_cells_result()?;

        let child_end = child
            .0
            .checked_add(child.1)
            .ok_or("ranges: child overflow")?;
        let mut iter = RangesIterWide::new(
            child_address_cells,
            parent_address_cells,
            child_size_cells,
            ranges,
        )?;
        for entry in &mut iter {
            let entry = entry?;
            let entry_end = entry
                .child_base
                .checked_add(entry.len)
                .ok_or("ranges: entry overflow")?;
            if child.0 >= entry.child_base && child_end <= entry_end {
                let off = child
                    .0
                    .checked_sub(entry.child_base)
                    .ok_or("ranges: underflow")?;
                let parent_base = entry
                    .parent_base
                    .checked_add(off)
                    .ok_or("ranges: overflow")?;
                return Ok((parent_base, child.1));
            }
        }
        Err("ranges: address not covered")
    }

    fn translate_address_internal_wide(
        &self,
        child: (u128, u128),
    ) -> Result<(u128, u128), &'static str> {
        let mapped = self.translate_one_level_wide(child)?;
        let Some((parent_scope, parent_ancestors)) = self.ancestors.split_last() else {
            return Ok(mapped);
        };
        let parent_view = self
            .parser
            .node_view_from_scope(*parent_scope, parent_ancestors)?;
        parent_view.translate_address_internal_wide(mapped)
    }

    pub(crate) fn translate_reg_address_internal(
        &self,
        parent_addr: (usize, usize),
    ) -> Result<(usize, usize), &'static str> {
        let Some((parent_scope, parent_ancestors)) = self.ancestors.split_last() else {
            return Ok(parent_addr);
        };
        let parent_view = self
            .parser
            .node_view_from_scope(*parent_scope, parent_ancestors)?;
        parent_view.translate_address_internal(parent_addr)
    }

    pub(crate) fn translate_address_internal(
        &self,
        child: (usize, usize),
    ) -> Result<(usize, usize), &'static str> {
        let mapped = self.translate_address_internal_wide((child.0 as u128, child.1 as u128))?;
        let addr =
            usize::try_from(mapped.0).map_err(|_| "ranges: mapped address overflow usize")?;
        let len = usize::try_from(mapped.1).map_err(|_| "ranges: mapped size overflow usize")?;
        Ok((addr, len))
    }
}
