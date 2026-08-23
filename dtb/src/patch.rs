//! Reusable DTB patch helpers for platform-specific boot-time edits.

use alloc::format;
use alloc::vec;
use common::IrqSense;

use crate::ast::DeviceTree;
use crate::ast::DeviceTreeEditExt;
use crate::ast::DeviceTreeQueryExt;
use crate::ast::NameRef;
use crate::ast::NodeEditExt;
use crate::ast::NodeQueryExt;
use crate::ast::Owned;
use crate::ast::ValueRef;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pl011Spec {
    pub base: u64,
    pub size: u64,
    pub uartclk_hz: u32,
    pub pintid: u32,
    pub irq_sense: IrqSense,
}

const fn gic_dt_irq_flags_from_sense(sense: IrqSense) -> u32 {
    match sense {
        IrqSense::Edge => 1,
        IrqSense::Level => 4,
    }
}

pub fn detach_by_compatible(
    tree: &mut DeviceTree<'_, Owned>,
    compat: &str,
) -> Result<usize, &'static str> {
    let mut detached = 0usize;
    let nodes_len = tree.nodes.len();
    for id in 0..nodes_len {
        let should_detach = tree
            .node(id)
            .map(|node| compatible_contains(node, compat))
            .unwrap_or(false);
        if should_detach {
            tree.detach_node(id)?;
            detached += 1;
        }
    }
    Ok(detached)
}

pub fn detach_by_node_name(
    tree: &mut DeviceTree<'_, Owned>,
    name: &str,
) -> Result<usize, &'static str> {
    let mut detached = 0usize;
    let nodes_len = tree.nodes.len();
    for id in 0..nodes_len {
        let should_detach = tree
            .node(id)
            .map(|node| node.name.as_str() == name)
            .unwrap_or(false);
        if should_detach {
            tree.detach_node(id)?;
            detached += 1;
        }
    }
    Ok(detached)
}

pub fn inject_standalone_pl011(
    tree: &mut DeviceTree<'_, Owned>,
    spec: Pl011Spec,
) -> Result<(), &'static str> {
    if spec.size == 0 {
        return Err("dtb: pl011 size must be non-zero");
    }
    if spec.pintid < 32 {
        return Err("dtb: pl011 pintid must be >= 32");
    }

    let root = tree.root;
    let addr_cells: usize = property_u32(tree, root, "#address-cells")?
        .unwrap_or(2)
        .try_into()
        .map_err(|_| "dtb: #address-cells overflow")?;
    let size_cells: usize = property_u32(tree, root, "#size-cells")?
        .unwrap_or(2)
        .try_into()
        .map_err(|_| "dtb: #size-cells overflow")?;

    if addr_cells == 0 {
        return Err("dtb: #address-cells must be non-zero");
    }
    if size_cells == 0 {
        return Err("dtb: #size-cells must be non-zero");
    }

    let uart_path = format!("/uart@{:x}", spec.base);
    let uart_id = tree.get_or_create_node_by_path(&uart_path)?;

    let reg_cells = addr_cells
        .checked_add(size_cells)
        .ok_or("dtb: reg cells overflow")?;
    let reg_len = reg_cells
        .checked_mul(4)
        .ok_or("dtb: reg byte length overflow")?;
    let mut reg = vec![0u8; reg_len];
    write_be_u32s(&mut reg, 0, addr_cells, spec.base)?;
    write_be_u32s(&mut reg, addr_cells * 4, size_cells, spec.size)?;

    let spi = spec
        .pintid
        .checked_sub(32)
        .ok_or("dtb: pl011 pintid must be >= 32")?;
    let mut interrupts = vec![0u8; 12];
    write_be_u32(&mut interrupts, 0, 0)?;
    write_be_u32(&mut interrupts, 4, spi)?;
    write_be_u32(
        &mut interrupts,
        8,
        gic_dt_irq_flags_from_sense(spec.irq_sense),
    )?;

    let clock_path = if tree.find_node_by_path("/clocks").is_some() {
        "/clocks/hv-pl011-uartclk"
    } else {
        "/hv-pl011-uartclk"
    };
    let clock_id = tree.get_or_create_node_by_path(clock_path)?;
    let clock_phandle = allocate_phandle(tree)?;
    let phandle_bytes = clock_phandle.to_be_bytes().to_vec();
    let mut clock_frequency = vec![0u8; 4];
    write_be_u32(&mut clock_frequency, 0, spec.uartclk_hz)?;
    let mut zero_u32 = vec![0u8; 4];
    write_be_u32(&mut zero_u32, 0, 0)?;

    {
        let clock = tree.node_mut(clock_id).ok_or("dtb: missing clock node")?;
        clock.set_property(
            NameRef::Borrowed("compatible"),
            ValueRef::Owned(b"fixed-clock\0".to_vec()),
        );
        clock.set_property(NameRef::Borrowed("#clock-cells"), ValueRef::Owned(zero_u32));
        clock.set_property(
            NameRef::Borrowed("clock-frequency"),
            ValueRef::Owned(clock_frequency),
        );
        clock.set_property(
            NameRef::Borrowed("phandle"),
            ValueRef::Owned(phandle_bytes.clone()),
        );
        clock.set_property(
            NameRef::Borrowed("linux,phandle"),
            ValueRef::Owned(phandle_bytes),
        );
    }

    let mut clocks = vec![0u8; 8];
    write_be_u32(&mut clocks, 0, clock_phandle)?;
    write_be_u32(&mut clocks, 4, clock_phandle)?;

    {
        let uart = tree.node_mut(uart_id).ok_or("dtb: missing uart node")?;
        uart.set_property(
            NameRef::Borrowed("compatible"),
            ValueRef::Owned(b"arm,pl011\0arm,primecell\0".to_vec()),
        );
        uart.set_property(NameRef::Borrowed("reg"), ValueRef::Owned(reg));
        uart.set_property(NameRef::Borrowed("interrupts"), ValueRef::Owned(interrupts));
        uart.set_property(
            NameRef::Borrowed("status"),
            ValueRef::Owned(b"okay\0".to_vec()),
        );
        uart.set_property(NameRef::Borrowed("clocks"), ValueRef::Owned(clocks));
        uart.set_property(
            NameRef::Borrowed("clock-names"),
            ValueRef::Owned(b"uartclk\0apb_pclk\0".to_vec()),
        );
    }

    let mut alias_target = uart_path.into_bytes();
    alias_target.push(0);
    let aliases_id = tree.get_or_create_node_by_path("/aliases")?;
    let aliases = tree
        .node_mut(aliases_id)
        .ok_or("dtb: missing aliases node")?;
    aliases.set_property(
        NameRef::Borrowed("serial0"),
        ValueRef::Owned(alias_target.clone()),
    );
    aliases.set_property(NameRef::Borrowed("uart0"), ValueRef::Owned(alias_target));

    Ok(())
}

fn compatible_contains(node: &crate::ast::Node<'_>, compat: &str) -> bool {
    let Some(prop) = node.property("compatible") else {
        return false;
    };
    string_list_contains(prop.value.as_slice(), compat)
}

fn string_list_contains(bytes: &[u8], needle: &str) -> bool {
    bytes.split(|&byte| byte == 0).any(|entry| {
        !entry.is_empty() && core::str::from_utf8(entry).is_ok_and(|entry| entry == needle)
    })
}

fn property_u32(
    tree: &DeviceTree<'_, Owned>,
    node_id: usize,
    key: &str,
) -> Result<Option<u32>, &'static str> {
    let node = tree.node(node_id).ok_or("dtb: invalid node id")?;
    let Some(prop) = node.property(key) else {
        return Ok(None);
    };
    let bytes = prop.value.as_slice();
    if bytes.len() != 4 {
        return Err("dtb: u32 property must be 4 bytes");
    }
    Ok(Some(read_be_u32(bytes)?))
}

fn read_be_u32(bytes: &[u8]) -> Result<u32, &'static str> {
    if bytes.len() != 4 {
        return Err("dtb: read_be_u32 expects 4 bytes");
    }
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn write_be_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), &'static str> {
    let end = offset.checked_add(4).ok_or("dtb: write_be_u32 overflow")?;
    let slot = bytes
        .get_mut(offset..end)
        .ok_or("dtb: write_be_u32 out of bounds")?;
    slot.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn write_be_u32s(
    bytes: &mut [u8],
    offset: usize,
    cells: usize,
    value: u64,
) -> Result<(), &'static str> {
    for idx in 0..cells {
        let shift = (cells - 1 - idx)
            .checked_mul(32)
            .ok_or("dtb: write_be_u32s shift overflow")?;
        let cell = if shift >= 64 {
            0
        } else {
            ((value >> shift) & 0xffff_ffff) as u32
        };
        let cell_offset = offset
            .checked_add(
                idx.checked_mul(4)
                    .ok_or("dtb: write_be_u32s offset overflow")?,
            )
            .ok_or("dtb: write_be_u32s offset overflow")?;
        write_be_u32(bytes, cell_offset, cell)?;
    }
    Ok(())
}

fn allocate_phandle(tree: &DeviceTree<'_, Owned>) -> Result<u32, &'static str> {
    let mut max = 0u32;
    for node in &tree.nodes {
        for prop_name in ["phandle", "linux,phandle"] {
            let Some(prop) = node.property(prop_name) else {
                continue;
            };
            let value = prop.value.as_slice();
            if value.len() != 4 {
                return Err("dtb: phandle property must be 4 bytes");
            }
            let phandle = read_be_u32(value)?;
            if phandle > max {
                max = phandle;
            }
        }
    }
    let next = max.checked_add(1).ok_or("dtb: phandle overflow")?;
    if next == 0 {
        return Err("dtb: phandle overflow");
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::DeviceTree;
    use crate::ast::DeviceTreeEditExt;
    use crate::ast::DeviceTreeQueryExt;
    use crate::ast::NameRef;
    use crate::ast::NodeEditExt;
    use crate::ast::NodeQueryExt;
    use crate::ast::ValueRef;
    use alloc::vec::Vec;

    fn parse_be_u32_cells(bytes: &[u8]) -> Result<Vec<u32>, &'static str> {
        if bytes.len() % 4 != 0 {
            return Err("dtb: u32 cells must be 4-byte aligned");
        }
        let mut out = Vec::with_capacity(bytes.len() / 4);
        let mut offset = 0usize;
        while offset < bytes.len() {
            let end = offset + 4;
            out.push(u32::from_be_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]));
            offset = end;
        }
        Ok(out)
    }

    fn set_u32_property(tree: &mut DeviceTree<'_, Owned>, node: usize, key: &str, value: u32) {
        tree.node_mut(node).unwrap().set_property(
            NameRef::Owned(key.to_string()),
            ValueRef::Owned(value.to_be_bytes().to_vec()),
        );
    }

    #[test]
    fn string_lists_skip_empty_and_invalid_entries() {
        let entries = b"\0\xff\0arm,pl011";
        assert!(string_list_contains(entries, "arm,pl011"));
        assert!(!string_list_contains(entries, ""));
    }

    #[test]
    fn detach_removes_nodes_from_serialized_dtb() {
        let mut tree = DeviceTree::with_root(NameRef::Borrowed("/"));
        let pcie = tree
            .add_child(tree.root, NameRef::Borrowed("pcie@1c10000000"))
            .unwrap();
        tree.node_mut(pcie).unwrap().set_property(
            NameRef::Borrowed("compatible"),
            ValueRef::Borrowed(b"brcm,bcm2712-pcie\0"),
        );
        let _rp1 = tree.add_child(pcie, NameRef::Borrowed("rp1")).unwrap();

        let detached = detach_by_compatible(&mut tree, "brcm,bcm2712-pcie").unwrap();
        assert_eq!(detached, 1);

        let dtb = tree.into_dtb_box().unwrap();
        let reparsed = DeviceTree::from_dtb(&dtb).unwrap();
        assert!(reparsed.find_node_by_path("/pcie@1c10000000").is_none());
    }

    #[test]
    fn inject_standalone_pl011_creates_probeable_node() {
        let mut tree = DeviceTree::with_root(NameRef::Borrowed("/"));
        let root = tree.root;
        set_u32_property(&mut tree, root, "#address-cells", 2);
        set_u32_property(&mut tree, root, "#size-cells", 2);

        inject_standalone_pl011(
            &mut tree,
            Pl011Spec {
                base: 0x1c00030000,
                size: 0x1000,
                uartclk_hz: 48_000_000,
                pintid: 185,
                irq_sense: IrqSense::Edge,
            },
        )
        .unwrap();

        let dtb = tree.into_dtb_box().unwrap();
        let reparsed = DeviceTree::from_dtb(&dtb).unwrap();

        let uart_id = reparsed
            .find_node_by_path("/uart@1c00030000")
            .expect("missing injected uart node");
        let uart = reparsed.node(uart_id).unwrap();
        let compatible = uart.property("compatible").expect("missing compatible");
        assert!(
            string_list_contains(compatible.value.as_slice(), "arm,pl011"),
            "compatible list missing arm,pl011"
        );

        let interrupts = uart.property("interrupts").expect("missing interrupts");
        let ints = parse_be_u32_cells(interrupts.value.as_slice()).unwrap();
        assert_eq!(ints.as_slice(), &[0, 153, 1]);

        let aliases_id = reparsed
            .find_node_by_path("/aliases")
            .expect("missing aliases node");
        let aliases = reparsed.node(aliases_id).unwrap();
        assert_eq!(
            aliases.property("serial0").unwrap().value.as_slice(),
            b"/uart@1c00030000\0"
        );
        assert_eq!(
            aliases.property("uart0").unwrap().value.as_slice(),
            b"/uart@1c00030000\0"
        );

        let clock_id = reparsed
            .find_node_by_path("/hv-pl011-uartclk")
            .expect("missing fixed-clock node");
        let clock = reparsed.node(clock_id).unwrap();
        assert!(
            string_list_contains(
                clock.property("compatible").unwrap().value.as_slice(),
                "fixed-clock"
            ),
            "fixed-clock node has unexpected compatible"
        );

        let freq = parse_be_u32_cells(clock.property("clock-frequency").unwrap().value.as_slice())
            .unwrap();
        assert_eq!(freq.as_slice(), &[48_000_000]);

        let phandle = parse_be_u32_cells(clock.property("phandle").unwrap().value.as_slice())
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let linux_phandle =
            parse_be_u32_cells(clock.property("linux,phandle").unwrap().value.as_slice())
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
        assert_eq!(phandle, linux_phandle);

        let clocks = parse_be_u32_cells(uart.property("clocks").unwrap().value.as_slice()).unwrap();
        assert_eq!(clocks.as_slice(), &[phandle, phandle]);
        assert_eq!(
            uart.property("clock-names").unwrap().value.as_slice(),
            b"uartclk\0apb_pclk\0"
        );
    }
}
