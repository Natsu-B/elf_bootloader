extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use allocator::AlignedSliceBox;
use arch_hal::gic::IrqSense;
use arch_hal::gic::MmioRegion;
use arch_hal::soc::bcm2712;
use core::convert::TryInto;
use core::ops::ControlFlow;

use ::dtb::DeviceTree;
use ::dtb::DeviceTreeBorrowed;
use ::dtb::DeviceTreeEditExt;
use ::dtb::DeviceTreeQueryExt;
use ::dtb::DtbNodeView;
use ::dtb::DtbParser;
use ::dtb::MemReserve;
use ::dtb::NameRef;
use ::dtb::NodeEditExt;
use ::dtb::NodeId;
use ::dtb::NodeQueryExt;
use ::dtb::Owned;
use ::dtb::ValueRef;
use ::dtb::WalkError;
use ::dtb::copy_node_properties;
use ::dtb::copy_subtree_to_path;
use ::dtb::encode_gic_spi_interrupts_with_mapper;
use ::dtb::encode_reg_entries;
use ::dtb::node_path;

use crate::GUEST_PL011_UART0_ADDR;
use crate::Gicv2Info;
use crate::RP1_BASE;
use crate::vgic;
use crate::virtio_blk;

type SourceTree<'dtb> = DeviceTreeBorrowed<'dtb>;
type TargetTree<'dtb> = DeviceTree<'dtb, Owned>;

const SOURCE_SOC_PATHS: [&str; 2] = ["/soc@107c000000", "/soc"];
const SOURCE_CLOCKS_PATH: &str = "/clocks";
const SOURCE_AXI_PATH: &str = "/axi";

const TARGET_SOC_PATH: &str = "/soc@107c000000";
const TARGET_UART_PATH: &str = "/soc@107c000000/serial@1c00030000";
const TARGET_GPIO_PATH: &str = "/soc@107c000000/gpio@1c000d0000";
const TARGET_I2C0_PATH: &str = "/soc@107c000000/i2c@1c00070000";
const TARGET_I2C10_PATH: &str = "/soc@107c000000/i2c@1c00088000";
const TARGET_RP1_CLOCKS_PATH: &str = "/soc@107c000000/clocks@1c00018000";
const TARGET_CSI0_PATH: &str = "/soc@107c000000/csi@1c0110000";
const TARGET_CSI1_PATH: &str = "/soc@107c000000/csi@1c0128000";
const TARGET_MAILBOX_PATH: &str = "/soc@107c000000/mailbox@7c013880";
const GUEST_ROOT_TOKEN: &str = "root=/dev/vda2";
const GUEST_SYSTEMD_MASK_TOKENS: [&str; 13] = [
    "systemd.mask=boot-firmware.mount",
    "systemd.mask=cloud-config.service",
    "systemd.mask=cloud-final.service",
    "systemd.mask=cloud-init-local.service",
    "systemd.mask=cloud-init-network.service",
    "systemd.mask=cloud-init-main.service",
    "systemd.mask=dev-zram0.swap",
    "systemd.mask=plymouth-quit-wait.service",
    "systemd.mask=plymouth-quit.service",
    "systemd.mask=plymouth-start.service",
    "systemd.mask=proc-sys-fs-binfmt_misc.mount",
    "systemd.mask=systemd-binfmt.service",
    "systemd.mask=systemd-zram-setup@zram0.service",
];
const GUEST_SYSTEMD_BOOT_TOKENS: [&str; 2] = [
    "systemd.unit=multi-user.target",
    "systemd.wants=serial-getty@ttyAMA0.service",
];

const SOURCE_UART0_PATH: &str = "/axi/pcie@1000120000/rp1/serial@30000";
const SOURCE_GPIO_PATH: &str = "/axi/pcie@1000120000/rp1/gpio@d0000";
const SOURCE_I2C0_PATH: &str = "/axi/pcie@1000120000/rp1/i2c@70000";
const SOURCE_I2C10_PATH: &str = "/axi/pcie@1000120000/rp1/i2c@88000";
const SOURCE_RP1_CLOCKS_PATH: &str = "/axi/pcie@1000120000/rp1/clocks@18000";
const SOURCE_CSI0_PATH: &str = "/axi/pcie@1000120000/rp1/csi@110000";
const SOURCE_CSI1_PATH: &str = "/axi/pcie@1000120000/rp1/csi@128000";

const SOURCE_CLK_XOSC_PATH: &str = "/clocks/clk_xosc";
const SOURCE_IOMMU5_PATH: &str = "/axi/iommu@5280";
const SOURCE_IOMMUC_PATH: &str = "/axi/iommuc@5b00";
const SOURCE_CAM0_CLK_PATH: &str = "/cam0_clk";
const SOURCE_CAM1_CLK_PATH: &str = "/cam1_clk";
const SOURCE_CAM0_REG_PATH: &str = "/cam0_reg";
const SOURCE_CAM1_REG_PATH: &str = "/cam1_reg";
const SOURCE_CAM_DUMMY_REG_PATH: &str = "/cam_dummy_reg";
const SOURCE_I2C0IF_PATH: &str = "/i2c0if";
const SOURCE_I2C0MUX_PATH: &str = "/i2c0mux";

const ROOT_PROPERTY_NAMES: [&str; 5] = [
    "#address-cells",
    "#size-cells",
    "model",
    "compatible",
    "interrupt-parent",
];
const SOC_PROPERTY_NAMES: [&str; 2] = ["compatible", "interrupt-parent"];
const CPU_KEEP_NAMES: [&str; 3] = ["cpu@0", "cpu@1", "cpu@2"];

const SOC_PHYS_BASE: u64 = 0x10_0000_0000;
const SOC_PHYS_SIZE: u64 = 0x8000_0000;
const RP1_SOC_WINDOW_SIZE: u64 = 0x40_0000;

#[derive(Clone, Copy)]
enum ProjectionInterrupts {
    None,
    Rp1Msix,
    Uart0,
}

#[derive(Clone, Copy)]
struct Rp1Projection {
    source: &'static str,
    target: &'static str,
    interrupts: ProjectionInterrupts,
    force_status_ok: bool,
}

const RP1_PROJECTIONS: [Rp1Projection; 7] = [
    Rp1Projection {
        source: SOURCE_UART0_PATH,
        target: TARGET_UART_PATH,
        interrupts: ProjectionInterrupts::Uart0,
        force_status_ok: true,
    },
    Rp1Projection {
        source: SOURCE_GPIO_PATH,
        target: TARGET_GPIO_PATH,
        interrupts: ProjectionInterrupts::Rp1Msix,
        force_status_ok: false,
    },
    Rp1Projection {
        source: SOURCE_I2C0_PATH,
        target: TARGET_I2C0_PATH,
        interrupts: ProjectionInterrupts::Rp1Msix,
        force_status_ok: false,
    },
    Rp1Projection {
        source: SOURCE_I2C10_PATH,
        target: TARGET_I2C10_PATH,
        interrupts: ProjectionInterrupts::Rp1Msix,
        force_status_ok: false,
    },
    Rp1Projection {
        source: SOURCE_RP1_CLOCKS_PATH,
        target: TARGET_RP1_CLOCKS_PATH,
        interrupts: ProjectionInterrupts::None,
        force_status_ok: false,
    },
    Rp1Projection {
        source: SOURCE_CSI0_PATH,
        target: TARGET_CSI0_PATH,
        interrupts: ProjectionInterrupts::Rp1Msix,
        force_status_ok: false,
    },
    Rp1Projection {
        source: SOURCE_CSI1_PATH,
        target: TARGET_CSI1_PATH,
        interrupts: ProjectionInterrupts::Rp1Msix,
        force_status_ok: false,
    },
];

pub(crate) fn build_guest_dtb(
    source: &DtbParser,
    reserved_memory: &[(usize, usize)],
    gic_info: &Gicv2Info,
    uart_irq: vgic::UartIrq,
) -> Result<AlignedSliceBox<u8>, &'static str> {
    let source_tree = DeviceTreeBorrowed::from_parser(source)?;
    let mut target = DeviceTree::with_root(NameRef::Borrowed("/"));
    target.header = source_tree.header.clone();
    target.mem_reserve = source_tree.mem_reserve.clone();

    copy_root_properties(&source_tree, &mut target)?;
    let chosen_id = copy_chosen(&source_tree, &mut target)?;
    let initrd_range = remove_initrd(&mut target, chosen_id);
    remove_initrd_memreserve(&mut target, initrd_range);

    copy_memory_nodes(&source_tree, &mut target)?;
    copy_cpus(&source_tree, &mut target)?;
    copy_optional_same_path_subtree(&source_tree, &mut target, "/psci")?;
    copy_optional_same_path_subtree(&source_tree, &mut target, "/timer")?;

    init_required_same_path_node(
        &source_tree,
        &mut target,
        SOURCE_CLOCKS_PATH,
        "dtb: missing /clocks node",
    )?;
    copy_required_same_path_subtree(
        &source_tree,
        &mut target,
        SOURCE_CLK_XOSC_PATH,
        "dtb: missing clk_xosc node",
    )?;

    init_required_same_path_node(
        &source_tree,
        &mut target,
        SOURCE_AXI_PATH,
        "dtb: missing /axi node",
    )?;
    copy_required_same_path_subtree(
        &source_tree,
        &mut target,
        SOURCE_IOMMU5_PATH,
        "dtb: missing iommu5 node",
    )?;
    copy_required_same_path_subtree(
        &source_tree,
        &mut target,
        SOURCE_IOMMUC_PATH,
        "dtb: missing iommuc node",
    )?;

    init_soc_node(&source_tree, &mut target)?;
    copy_required_soc_child(
        &source_tree,
        source,
        &mut target,
        "interrupt-controller@7fff9000",
        "dtb: missing GIC node",
    )?;
    copy_required_soc_child(
        &source_tree,
        source,
        &mut target,
        "mailbox@7c013880",
        "dtb: missing mailbox node",
    )?;
    copy_required_same_path_subtree(
        &source_tree,
        &mut target,
        SOURCE_CAM0_CLK_PATH,
        "dtb: missing cam0_clk node",
    )?;
    copy_required_same_path_subtree(
        &source_tree,
        &mut target,
        SOURCE_CAM1_CLK_PATH,
        "dtb: missing cam1_clk node",
    )?;
    copy_required_same_path_subtree(
        &source_tree,
        &mut target,
        SOURCE_CAM0_REG_PATH,
        "dtb: missing cam0_reg node",
    )?;
    copy_optional_same_path_subtree(&source_tree, &mut target, SOURCE_CAM1_REG_PATH)?;
    copy_required_same_path_subtree(
        &source_tree,
        &mut target,
        SOURCE_CAM_DUMMY_REG_PATH,
        "dtb: missing cam_dummy_reg node",
    )?;
    copy_required_same_path_subtree(
        &source_tree,
        &mut target,
        SOURCE_I2C0IF_PATH,
        "dtb: missing i2c0if node",
    )?;
    copy_required_same_path_subtree(
        &source_tree,
        &mut target,
        SOURCE_I2C0MUX_PATH,
        "dtb: missing i2c0mux node",
    )?;

    let gic_phandle = find_source_gic_phandle(&source_tree)?;
    for projection in RP1_PROJECTIONS {
        project_rp1_subtree(
            &source_tree,
            source,
            &mut target,
            projection,
            gic_phandle,
            uart_irq,
        )?;
    }
    add_virtio_mmio_blk_node(&mut target, gic_phandle)?;
    copy_reserved_memory(&source_tree, &mut target)?;
    rebuild_aliases(&source_tree, &mut target)?;
    configure_uart_console(&mut target, chosen_id, GUEST_PL011_UART0_ADDR.0)?;
    append_reserved_memory(&mut target, reserved_memory);

    let gicv = gic_info
        .gicv
        .ok_or("gic: missing GICV region for DT update")?;
    update_gicv2_cpu_interface_reg(&mut target, gicv)?;

    target.into_dtb_box()
}

fn add_virtio_mmio_blk_node(
    target: &mut TargetTree<'_>,
    gic_phandle: u32,
) -> Result<NodeId, &'static str> {
    let base = virtio_blk::VIRTIO_BLK_MMIO_BASE as u64;
    let size = virtio_blk::VIRTIO_BLK_MMIO_SIZE as u64;
    let path = format!("/virtio_mmio@{base:x}");
    let node_id = target.get_or_create_node_by_path(&path)?;
    let node = target
        .node_mut(node_id)
        .ok_or("dtb: missing virtio-blk target node")?;
    node.set_property(
        NameRef::Borrowed("compatible"),
        ValueRef::Owned(b"virtio,mmio\0".to_vec()),
    );
    node.set_property(
        NameRef::Borrowed("dma-coherent"),
        ValueRef::Owned(Vec::new()),
    );
    node.set_property(
        NameRef::Borrowed("reg"),
        ValueRef::Owned(encode_reg_entries(&[(base, size)], 2, 1)?),
    );
    node.set_property(
        NameRef::Borrowed("interrupt-parent"),
        ValueRef::Owned(gic_phandle.to_be_bytes().to_vec()),
    );
    let irq_flags = gic_dt_irq_flags_from_sense(IrqSense::Edge);
    let interrupts = encode_gic_spi_interrupts_with_mapper(
        &[(virtio_blk::VIRTIO_BLK_IRQ_INTID, irq_flags)],
        |intid| Ok(intid),
    )?;
    node.set_property(NameRef::Borrowed("interrupts"), ValueRef::Owned(interrupts));
    node.set_property(
        NameRef::Borrowed("status"),
        ValueRef::Owned(b"okay\0".to_vec()),
    );
    Ok(node_id)
}

fn copy_root_properties(
    source: &SourceTree<'_>,
    target: &mut TargetTree<'_>,
) -> Result<(), &'static str> {
    copy_selected_properties(
        source,
        source.root,
        target,
        target.root,
        &ROOT_PROPERTY_NAMES,
    )
}

fn init_required_same_path_node(
    source: &SourceTree<'_>,
    target: &mut TargetTree<'_>,
    path: &str,
    error: &'static str,
) -> Result<NodeId, &'static str> {
    let source_id = source.find_node_by_path(path).ok_or(error)?;
    let target_id = target.get_or_create_node_by_path(path)?;
    copy_node_properties(source, source_id, target, target_id)?;
    Ok(target_id)
}

fn init_soc_node(
    source: &SourceTree<'_>,
    target: &mut TargetTree<'_>,
) -> Result<NodeId, &'static str> {
    let source_path = source_soc_path(source)?;
    let source_id = source
        .find_node_by_path(source_path)
        .ok_or("dtb: missing /soc node")?;
    let target_id = target.get_or_create_node_by_path(TARGET_SOC_PATH)?;
    copy_selected_properties(source, source_id, target, target_id, &SOC_PROPERTY_NAMES)?;

    let target_node = target
        .node_mut(target_id)
        .ok_or("dtb: missing target /soc node")?;
    target_node.set_property(
        NameRef::Borrowed("#address-cells"),
        ValueRef::Owned(2u32.to_be_bytes().to_vec()),
    );
    target_node.set_property(
        NameRef::Borrowed("#size-cells"),
        ValueRef::Owned(1u32.to_be_bytes().to_vec()),
    );
    target_node.set_property(
        NameRef::Borrowed("ranges"),
        ValueRef::Owned(encode_target_soc_ranges()?),
    );
    Ok(target_id)
}

fn copy_chosen(
    source: &SourceTree<'_>,
    target: &mut TargetTree<'_>,
) -> Result<NodeId, &'static str> {
    let target_id = target.get_or_create_node_by_path("/chosen")?;
    if let Some(source_id) = source.find_node_by_path("/chosen") {
        copy_node_properties(source, source_id, target, target_id)?;
    }
    Ok(target_id)
}

fn copy_memory_nodes(
    source: &SourceTree<'_>,
    target: &mut TargetTree<'_>,
) -> Result<(), &'static str> {
    let root_children = source
        .node(source.root)
        .ok_or("dtb: missing source root")?
        .children
        .clone();
    let mut copied_any = false;
    for child_id in root_children {
        if is_memory_node(source, child_id)? {
            let path = node_path(source, child_id)?;
            copy_subtree_to_path(source, target, child_id, &path)?;
            copied_any = true;
        }
    }
    if copied_any {
        Ok(())
    } else {
        Err("dtb: missing memory node")
    }
}

fn copy_cpus(source: &SourceTree<'_>, target: &mut TargetTree<'_>) -> Result<(), &'static str> {
    let cpus_id = source
        .find_node_by_path("/cpus")
        .ok_or("dtb: missing /cpus node")?;
    let target_id = target.get_or_create_node_by_path("/cpus")?;
    copy_node_properties(source, cpus_id, target, target_id)?;

    let children = source
        .node(cpus_id)
        .ok_or("dtb: invalid /cpus node")?
        .children
        .clone();
    for child_id in children {
        let child = source.node(child_id).ok_or("dtb: invalid /cpus child")?;
        let name = child.name.as_str();
        let keep_cpu = CPU_KEEP_NAMES.iter().any(|candidate| *candidate == name);
        let keep_shared_cache =
            !name.starts_with("cpu@") && property_string_equals(child, "compatible", "cache");
        if !(keep_cpu || keep_shared_cache) {
            continue;
        }
        let path = format!("/cpus/{name}");
        copy_subtree_to_path(source, target, child_id, &path)?;
    }
    Ok(())
}

fn copy_reserved_memory(
    source: &SourceTree<'_>,
    target: &mut TargetTree<'_>,
) -> Result<(), &'static str> {
    let Some(source_id) = source.find_node_by_path("/reserved-memory") else {
        return Ok(());
    };
    let target_id = target.get_or_create_node_by_path("/reserved-memory")?;
    copy_node_properties(source, source_id, target, target_id)?;

    let children = source
        .node(source_id)
        .ok_or("dtb: invalid reserved-memory node")?
        .children
        .clone();
    for child_id in children {
        if !is_linux_cma_node(source, child_id)? {
            continue;
        }
        let child_name = source
            .node(child_id)
            .ok_or("dtb: invalid reserved-memory child")?
            .name
            .as_str();
        let path = format!("/reserved-memory/{child_name}");
        copy_subtree_to_path(source, target, child_id, &path)?;
    }
    Ok(())
}

fn rebuild_aliases(
    source: &SourceTree<'_>,
    target: &mut TargetTree<'_>,
) -> Result<(), &'static str> {
    let target_id = target.get_or_create_node_by_path("/aliases")?;
    if let Some(source_id) = source.find_node_by_path("/aliases") {
        let source_node = source.node(source_id).ok_or("dtb: invalid aliases node")?;
        for prop in &source_node.properties {
            let Some(path) = first_cstr(prop.value.as_slice()) else {
                continue;
            };
            let remapped = remap_alias_path(path);
            if target.find_node_by_path(&remapped).is_none() {
                continue;
            }
            target
                .node_mut(target_id)
                .ok_or("dtb: invalid aliases target node")?
                .set_property(
                    NameRef::Owned(prop.name.as_str().into()),
                    ValueRef::Owned(path_property_bytes(&remapped)),
                );
        }
    }

    for (name, path) in [
        ("serial0", TARGET_UART_PATH),
        ("uart0", TARGET_UART_PATH),
        ("i2c0", TARGET_I2C0_PATH),
        ("i2c10", TARGET_I2C10_PATH),
        ("mailbox", TARGET_MAILBOX_PATH),
    ] {
        if target.find_node_by_path(path).is_none() {
            continue;
        }
        target
            .node_mut(target_id)
            .ok_or("dtb: invalid aliases target node")?
            .set_property(
                NameRef::Borrowed(name),
                ValueRef::Owned(path_property_bytes(path)),
            );
    }
    Ok(())
}

fn remap_alias_path(path: &str) -> String {
    match path {
        SOURCE_UART0_PATH => return TARGET_UART_PATH.into(),
        SOURCE_GPIO_PATH => return TARGET_GPIO_PATH.into(),
        SOURCE_I2C0_PATH => return TARGET_I2C0_PATH.into(),
        SOURCE_I2C10_PATH => return TARGET_I2C10_PATH.into(),
        _ => {}
    }
    if let Some(suffix) = path.strip_prefix("/soc/") {
        return format!("{TARGET_SOC_PATH}/{suffix}");
    }
    path.into()
}

fn copy_required_same_path_subtree(
    source: &SourceTree<'_>,
    target: &mut TargetTree<'_>,
    path: &str,
    error: &'static str,
) -> Result<NodeId, &'static str> {
    copy_optional_same_path_subtree(source, target, path)?.ok_or(error)
}

fn copy_optional_same_path_subtree(
    source: &SourceTree<'_>,
    target: &mut TargetTree<'_>,
    path: &str,
) -> Result<Option<NodeId>, &'static str> {
    let Some(source_id) = source.find_node_by_path(path) else {
        return Ok(None);
    };
    Ok(Some(copy_subtree_to_path(source, target, source_id, path)?))
}

fn copy_required_soc_child(
    source: &SourceTree<'_>,
    source_parser: &DtbParser,
    target: &mut TargetTree<'_>,
    child_name: &str,
    error: &'static str,
) -> Result<NodeId, &'static str> {
    copy_optional_soc_child(source, source_parser, target, child_name)?.ok_or(error)
}

fn copy_optional_soc_child(
    source: &SourceTree<'_>,
    source_parser: &DtbParser,
    target: &mut TargetTree<'_>,
    child_name: &str,
) -> Result<Option<NodeId>, &'static str> {
    let source_path = source_soc_child_path(source, child_name)?;
    let Some(source_id) = source.find_node_by_path(&source_path) else {
        return Ok(None);
    };
    let target_path = format!("{TARGET_SOC_PATH}/{child_name}");
    let target_id = copy_subtree_to_path(source, target, source_id, &target_path)?;
    rewrite_soc_child_reg_from_source(source_parser, &source_path, target, target_id)?;
    Ok(Some(target_id))
}

fn project_rp1_subtree(
    source: &SourceTree<'_>,
    source_parser: &DtbParser,
    target: &mut TargetTree<'_>,
    projection: Rp1Projection,
    gic_phandle: u32,
    uart_irq: vgic::UartIrq,
) -> Result<NodeId, &'static str> {
    let source_id = source
        .find_node_by_path(projection.source)
        .ok_or("dtb: missing projected RP1 node")?;
    let target_id = copy_subtree_to_path(source, target, source_id, projection.target)?;
    rewrite_projected_rp1_reg_from_source(
        source_parser,
        projection.source,
        projection.target,
        target,
        target_id,
    )?;

    {
        let node = target
            .node_mut(target_id)
            .ok_or("dtb: missing projected RP1 target node")?;
        node.remove_property("ranges");
        node.remove_property("dma-ranges");
        node.remove_property("msi-parent");
        node.remove_property("interrupt-parent");
        if matches!(projection.interrupts, ProjectionInterrupts::Uart0) {
            sanitize_guest_uart0_node(node);
        }
        if projection.force_status_ok {
            node.set_property(
                NameRef::Borrowed("status"),
                ValueRef::Owned(b"okay\0".to_vec()),
            );
        }
    }

    match projection.interrupts {
        ProjectionInterrupts::None => {}
        ProjectionInterrupts::Rp1Msix => {
            rewrite_rp1_interrupts_from_source(
                source_parser,
                projection.source,
                target,
                target_id,
                gic_phandle,
                None,
            )?;
        }
        ProjectionInterrupts::Uart0 => {
            rewrite_rp1_interrupts_from_source(
                source_parser,
                projection.source,
                target,
                target_id,
                gic_phandle,
                Some(uart_irq),
            )?;
        }
    }
    Ok(target_id)
}

fn sanitize_guest_uart0_node(node: &mut ::dtb::ast::Node<'_>) {
    node.remove_property("cts-event-workaround");
    node.remove_property("skip-init");
    node.remove_property("uart-has-rtscts");
}

fn rewrite_soc_child_reg_from_source(
    source: &DtbParser,
    source_path: &str,
    target: &mut TargetTree<'_>,
    target_id: NodeId,
) -> Result<(), &'static str> {
    let regs = with_source_node_view(source, source_path, &mut |view| {
        let mut values = Vec::new();
        let mut iter = view.reg_raw_iter()?;
        while let Some(entry) = iter.next() {
            let (base, size) = entry?;
            values.push((base as u64, size as u64));
        }
        Ok(values)
    })?
    .ok_or("dtb: missing source node view")?;
    if regs.is_empty() {
        return Ok(());
    }
    target
        .node_mut(target_id)
        .ok_or("dtb: missing target node for reg rewrite")?
        .set_property(
            NameRef::Borrowed("reg"),
            ValueRef::Owned(encode_reg_entries(&regs, 2, 1)?),
        );
    Ok(())
}

fn rewrite_projected_rp1_reg_from_source(
    source: &DtbParser,
    source_path: &str,
    target_path: &str,
    target: &mut TargetTree<'_>,
    target_id: NodeId,
) -> Result<(), &'static str> {
    let source_regs = with_source_node_view(source, source_path, &mut |view| {
        let mut values = Vec::new();
        let mut iter = view.reg_iter()?;
        while let Some(entry) = iter.next() {
            let (phys, size) = entry?;
            values.push((phys as u64, size as u64));
        }
        Ok(values)
    })?
    .ok_or("dtb: missing source node view")?;
    if source_regs.is_empty() {
        return Ok(());
    }

    let target_base = target_node_unit_address(target_path)?;
    let source_base = source_regs[0].0;
    let regs = source_regs
        .into_iter()
        .map(|(phys, size)| {
            let offset = phys
                .checked_sub(source_base)
                .ok_or("dtb: RP1 reg entries are not monotonic")?;
            let guest = target_base
                .checked_add(offset)
                .ok_or("dtb: RP1 guest reg overflow")?;
            Ok((guest, size))
        })
        .collect::<Result<Vec<_>, &'static str>>()?;

    target
        .node_mut(target_id)
        .ok_or("dtb: missing target node for reg rewrite")?
        .set_property(
            NameRef::Borrowed("reg"),
            ValueRef::Owned(encode_reg_entries(&regs, 2, 1)?),
        );
    Ok(())
}

fn rewrite_rp1_interrupts_from_source(
    source: &DtbParser,
    source_path: &str,
    target: &mut TargetTree<'_>,
    target_id: NodeId,
    gic_phandle: u32,
    uart_irq: Option<vgic::UartIrq>,
) -> Result<(), &'static str> {
    let interrupts = if let Some(uart_irq) = uart_irq {
        encode_uart_passthrough_interrupts(uart_irq)?
    } else {
        let specifiers = with_source_node_view(source, source_path, &mut |view| {
            let Some(iter) = view.interrupts_iter::<2>()? else {
                return Ok(Vec::new());
            };
            let mut values = Vec::new();
            for entry in iter {
                let [cell0, flags] = entry?;
                values.push((cell0, flags));
            }
            Ok(values)
        })?
        .ok_or("dtb: missing source node view")?;
        if specifiers.is_empty() {
            return Ok(());
        }
        encode_gic_spi_interrupts_with_mapper(&specifiers, rp1_msix_index_to_intid)?
    };
    if interrupts.is_empty() {
        return Ok(());
    }

    let node = target
        .node_mut(target_id)
        .ok_or("dtb: missing target node for interrupt rewrite")?;
    node.set_property(
        NameRef::Borrowed("interrupt-parent"),
        ValueRef::Owned(gic_phandle.to_be_bytes().to_vec()),
    );
    node.set_property(NameRef::Borrowed("interrupts"), ValueRef::Owned(interrupts));
    Ok(())
}

fn encode_uart_passthrough_interrupts(uart_irq: vgic::UartIrq) -> Result<Vec<u8>, &'static str> {
    let specifiers = [(uart_irq.pintid, gic_dt_irq_flags_from_sense(uart_irq.sense))];
    encode_gic_spi_interrupts_with_mapper(&specifiers, |intid| Ok(intid))
}

fn with_source_node_view<R>(
    parser: &DtbParser,
    path: &str,
    f: &mut impl for<'a, 's> FnMut(DtbNodeView<'a, 's>) -> Result<R, &'static str>,
) -> Result<Option<R>, &'static str> {
    fn descend<R>(
        node: DtbNodeView<'_, '_>,
        segments: &[&str],
        f: &mut impl for<'a, 's> FnMut(DtbNodeView<'a, 's>) -> Result<R, &'static str>,
    ) -> Result<Option<R>, &'static str> {
        if segments.is_empty() {
            return Ok(Some(f(node)?));
        }

        let mut found = None;
        match node.for_each_child_view(&mut |child| {
            if child.name() != segments[0] {
                return Ok(ControlFlow::Continue(()));
            }
            found = descend(child, &segments[1..], f).map_err(WalkError::User)?;
            Ok(ControlFlow::Break(()))
        }) {
            Ok(ControlFlow::Continue(())) | Ok(ControlFlow::Break(())) => Ok(found),
            Err(WalkError::Dtb(err)) => Err(err),
            Err(WalkError::User(err)) => Err(err),
        }
    }

    if path == "/" {
        return Ok(Some(f(parser.root_node_view()?)?));
    }
    if !path.starts_with('/') {
        return Err("dtb: path must start with '/'");
    }

    let root = parser.root_node_view()?;
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    descend(root, &segments, f)
}

fn source_soc_path(source: &SourceTree<'_>) -> Result<&'static str, &'static str> {
    SOURCE_SOC_PATHS
        .into_iter()
        .find(|path| source.find_node_by_path(path).is_some())
        .ok_or("dtb: missing /soc node")
}

fn source_soc_child_path(
    source: &SourceTree<'_>,
    child_name: &str,
) -> Result<String, &'static str> {
    Ok(format!("{}/{}", source_soc_path(source)?, child_name))
}

fn find_source_gic_phandle(source: &SourceTree<'_>) -> Result<u32, &'static str> {
    let gic_path = source_soc_child_path(source, "interrupt-controller@7fff9000")?;
    let node_id = source
        .find_node_by_path(&gic_path)
        .ok_or("dtb: missing GIC node")?;
    node_phandle(source, node_id).ok_or("dtb: missing GIC phandle")
}

fn copy_selected_properties(
    source: &SourceTree<'_>,
    source_id: NodeId,
    target: &mut TargetTree<'_>,
    target_id: NodeId,
    names: &[&'static str],
) -> Result<(), &'static str> {
    let source_node = source
        .node(source_id)
        .ok_or("dtb: invalid source node id")?;
    let target_node = target
        .node_mut(target_id)
        .ok_or("dtb: invalid target node id")?;
    for &name in names {
        let Some(prop) = source_node.property(name) else {
            continue;
        };
        target_node.set_property(
            NameRef::Borrowed(name),
            ValueRef::Owned(prop.value.as_slice().to_vec()),
        );
    }
    Ok(())
}

fn node_phandle(tree: &SourceTree<'_>, node_id: NodeId) -> Option<u32> {
    property_u32(tree, node_id, "phandle")
        .ok()
        .flatten()
        .or_else(|| property_u32(tree, node_id, "linux,phandle").ok().flatten())
}

fn find_node_by_phandle_owned(tree: &TargetTree<'_>, phandle: u32) -> Option<NodeId> {
    tree.nodes.iter().enumerate().find_map(|(id, node)| {
        let matches = node
            .property("phandle")
            .and_then(|prop| {
                let bytes = prop.value.as_slice();
                if bytes.len() != 4 {
                    return None;
                }
                Some(u32::from_be_bytes(bytes.try_into().ok()?))
            })
            .or_else(|| {
                node.property("linux,phandle").and_then(|prop| {
                    let bytes = prop.value.as_slice();
                    if bytes.len() != 4 {
                        return None;
                    }
                    Some(u32::from_be_bytes(bytes.try_into().ok()?))
                })
            });
        (matches == Some(phandle)).then_some(id)
    })
}

const fn gic_dt_irq_flags_from_sense(sense: IrqSense) -> u32 {
    match sense {
        IrqSense::Edge => 1,
        IrqSense::Level => 4,
    }
}

fn rp1_msix_index_to_intid(msix_index: u32) -> Result<u32, &'static str> {
    let index = usize::try_from(msix_index).map_err(|_| "dtb: RP1 MSI-X index overflow")?;
    if !bcm2712::pirq_hook::GUEST_RP1_PASSTHROUGH_MSIX_INDICES.contains(&index) {
        return Err("dtb: RP1 MSI-X index is not guest-pass-through");
    }
    Ok(bcm2712::pirq_hook::RP1_MSIX_SPI_START + msix_index)
}

fn target_node_unit_address(path: &str) -> Result<u64, &'static str> {
    let name = path.rsplit('/').next().ok_or("dtb: invalid node path")?;
    let (_, unit_address) = name
        .rsplit_once('@')
        .ok_or("dtb: node path missing unit address")?;
    u64::from_str_radix(unit_address, 16).map_err(|_| "dtb: invalid node unit address")
}

fn soc_bus_address_from_phys(address: u64) -> Result<u64, &'static str> {
    let soc_end = SOC_PHYS_BASE
        .checked_add(SOC_PHYS_SIZE)
        .ok_or("dtb: /soc range overflow")?;
    if (SOC_PHYS_BASE..soc_end).contains(&address) {
        return Ok(address - SOC_PHYS_BASE);
    }

    let rp1_base = RP1_BASE as u64;
    let rp1_end = rp1_base
        .checked_add(RP1_SOC_WINDOW_SIZE)
        .ok_or("dtb: RP1 /soc range overflow")?;
    if (rp1_base..rp1_end).contains(&address) {
        return Ok(address);
    }

    Err("dtb: address is outside guest /soc ranges")
}

fn encode_target_soc_ranges() -> Result<Vec<u8>, &'static str> {
    const ENTRY_CELLS: usize = 5;
    let mut bytes = vec![0u8; ENTRY_CELLS * 4 * 2];

    write_be_u32s(&mut bytes, 0, 2, 0)?;
    write_be_u32s(&mut bytes, 8, 2, SOC_PHYS_BASE)?;
    write_be_u32s(&mut bytes, 16, 1, SOC_PHYS_SIZE)?;

    write_be_u32s(&mut bytes, 20, 2, RP1_BASE as u64)?;
    write_be_u32s(&mut bytes, 28, 2, RP1_BASE as u64)?;
    write_be_u32s(&mut bytes, 36, 1, RP1_SOC_WINDOW_SIZE)?;
    Ok(bytes)
}

fn path_property_bytes(path: &str) -> Vec<u8> {
    let mut bytes = path.as_bytes().to_vec();
    if !bytes.ends_with(&[0]) {
        bytes.push(0);
    }
    bytes
}

fn be_bytes_to_u64(bytes: &[u8]) -> Option<u64> {
    match bytes.len() {
        4 => Some(u32::from_be_bytes(bytes.try_into().ok()?) as u64),
        8 => Some(u64::from_be_bytes(bytes.try_into().ok()?)),
        _ => None,
    }
}

fn remove_initrd(tree: &mut DeviceTree<'_>, chosen: NodeId) -> Option<(u64, u64)> {
    let start = tree
        .node(chosen)
        .and_then(|node| node.property("linux,initrd-start"))
        .and_then(|p| be_bytes_to_u64(p.value.as_slice()));
    let end = tree
        .node(chosen)
        .and_then(|node| node.property("linux,initrd-end"))
        .and_then(|p| be_bytes_to_u64(p.value.as_slice()));

    if let Some(node) = tree.node_mut(chosen) {
        node.remove_property("linux,initrd-start");
        node.remove_property("linux,initrd-end");
    }

    if let (Some(start), Some(end)) = (start, end)
        && end > start
    {
        return Some((start, end - start));
    }
    None
}

fn remove_initrd_memreserve(tree: &mut DeviceTree<'_>, initrd: Option<(u64, u64)>) {
    if let Some((addr, size)) = initrd {
        tree.mem_reserve
            .retain(|entry| !(entry.address == addr && entry.size == size));
    }
}

fn append_reserved_memory(tree: &mut DeviceTree<'_>, reserved_memory: &[(usize, usize)]) {
    for &(addr, size) in reserved_memory {
        if size == 0 {
            continue;
        }
        let entry = MemReserve {
            address: addr as u64,
            size: size as u64,
        };
        if tree
            .mem_reserve
            .iter()
            .any(|existing| existing.address == entry.address && existing.size == entry.size)
        {
            continue;
        }
        tree.mem_reserve.push(entry);
    }
}

fn configure_uart_console(
    tree: &mut DeviceTree<'_>,
    chosen: NodeId,
    pl011_uart_addr: usize,
) -> Result<(), &'static str> {
    let alias = pick_uart_alias(tree);
    let stdout_value = format!("{alias}:115200\0").into_bytes();
    let node = tree.node_mut(chosen).ok_or("chosen node missing")?;
    node.set_property(
        NameRef::Borrowed("stdout-path"),
        ValueRef::Owned(stdout_value.clone()),
    );
    node.set_property(
        NameRef::Borrowed("linux,stdout-path"),
        ValueRef::Owned(stdout_value),
    );

    update_bootargs(tree, chosen, pl011_uart_addr)
}

fn pick_uart_alias(tree: &DeviceTree<'_>) -> &'static str {
    if let Some(alias_id) = tree.find_node_by_path("/aliases")
        && let Some(node) = tree.node(alias_id)
    {
        if node.property("uart0").is_some() {
            return "uart0";
        }
        if node.property("serial0").is_some() {
            return "serial0";
        }
    }
    "uart0"
}

fn update_bootargs(
    tree: &mut DeviceTree<'_>,
    chosen: NodeId,
    pl011_uart_addr: usize,
) -> Result<(), &'static str> {
    let existing = tree
        .node(chosen)
        .and_then(|node| node.property("bootargs"))
        .map(|prop| prop.value.as_slice())
        .and_then(|bytes| bytes.split(|byte| *byte == 0).next())
        .and_then(|raw| core::str::from_utf8(raw).ok());
    let mut bytes = rewrite_bootargs(existing, pl011_uart_addr).into_bytes();
    if !bytes.ends_with(&[0]) {
        bytes.push(0);
    }

    tree.node_mut(chosen)
        .ok_or("chosen node missing")?
        .set_property(NameRef::Borrowed("bootargs"), ValueRef::Owned(bytes));
    Ok(())
}

fn rewrite_bootargs(existing: Option<&str>, pl011_uart_addr: usize) -> String {
    let mut args = String::new();
    let mut saw_rootwait = false;

    if let Some(existing) = existing {
        for token in existing.split_whitespace() {
            if token.starts_with("console=")
                || token.starts_with("earlycon=")
                || token.starts_with("root=")
                || token.starts_with("systemd.unit=")
                || token.starts_with("systemd.wants=")
                || token.starts_with("virtio_mmio.device=")
                || GUEST_SYSTEMD_MASK_TOKENS.contains(&token)
                || GUEST_SYSTEMD_BOOT_TOKENS.contains(&token)
                || token == "plymouth.ignore-serial-console"
            {
                continue;
            }
            if token == "rootwait" {
                if saw_rootwait {
                    continue;
                }
                saw_rootwait = true;
            }
            if !args.is_empty() {
                args.push(' ');
            }
            args.push_str(token);
        }
    }

    let earlycon = format!("earlycon=pl011,0x{pl011_uart_addr:x}");
    for token in [
        GUEST_ROOT_TOKEN,
        if saw_rootwait { "" } else { "rootwait" },
        earlycon.as_str(),
        "console=ttyAMA0,115200",
    ] {
        append_bootarg_token(&mut args, token);
    }
    for token in GUEST_SYSTEMD_BOOT_TOKENS {
        append_bootarg_token(&mut args, token);
    }
    for token in GUEST_SYSTEMD_MASK_TOKENS {
        append_bootarg_token(&mut args, token);
    }

    args
}

fn append_bootarg_token(args: &mut String, token: &str) {
    if token.is_empty() {
        return;
    }
    if !args.is_empty() {
        args.push(' ');
    }
    args.push_str(token);
}

#[cfg(test)]
fn bootarg_token_count(text: &str, needle: &str) -> usize {
    text.split_whitespace()
        .filter(|token| *token == needle)
        .count()
}

#[cfg(test)]
fn bootargs_contains_guest_systemd_masks(text: &str) -> bool {
    GUEST_SYSTEMD_MASK_TOKENS
        .iter()
        .all(|token| bootarg_token_count(text, token) == 1)
}

#[cfg(test)]
fn bootargs_contains_guest_systemd_boot_tokens(text: &str) -> bool {
    GUEST_SYSTEMD_BOOT_TOKENS
        .iter()
        .all(|token| bootarg_token_count(text, token) == 1)
}

fn update_gicv2_cpu_interface_reg(
    tree: &mut DeviceTree<'_>,
    gicv: MmioRegion,
) -> Result<(), &'static str> {
    const COMPATS: [&str; 2] = ["arm,gic-400", "arm,cortex-a15-gic"];
    let mut gic_node = None;
    for node_id in 0..tree.nodes.len() {
        for compat in COMPATS {
            if node_compatible_contains(tree, node_id, compat)? {
                gic_node = Some(node_id);
                break;
            }
        }
        if gic_node.is_some() {
            break;
        }
    }
    let Some(node_id) = gic_node else {
        return Ok(());
    };

    let parent = tree
        .node(node_id)
        .and_then(|node| node.parent)
        .unwrap_or(tree.root);
    let addr_cells = property_u32(tree, parent, "#address-cells")?.unwrap_or(2) as usize;
    let size_cells = property_u32(tree, parent, "#size-cells")?.unwrap_or(1) as usize;
    let stride = (addr_cells + size_cells) * 4;
    let Some(node) = tree.node(node_id) else {
        return Ok(());
    };
    let Some(reg) = node.property("reg") else {
        return Ok(());
    };
    let mut bytes = reg.value.as_slice().to_vec();
    if bytes.len() < stride * 2 {
        return Err("gic: reg property too short");
    }

    let parent_path = node_path(tree, parent)?;
    let cpu_if_base = if parent_path == TARGET_SOC_PATH {
        soc_bus_address_from_phys(gicv.base as u64)?
    } else {
        gicv.base as u64
    };

    let base_offset = stride;
    write_be_u32s(&mut bytes, base_offset, addr_cells, cpu_if_base)?;
    write_be_u32s(
        &mut bytes,
        base_offset + addr_cells * 4,
        size_cells,
        gicv.size as u64,
    )?;

    if let Some(node) = tree.node_mut(node_id) {
        node.set_property(NameRef::Borrowed("reg"), ValueRef::Owned(bytes));
    }
    Ok(())
}

fn node_compatible_contains(
    tree: &DeviceTree<'_>,
    node_id: NodeId,
    needle: &str,
) -> Result<bool, &'static str> {
    let Some(node) = tree.node(node_id) else {
        return Ok(false);
    };
    let Some(prop) = node.property("compatible") else {
        return Ok(false);
    };
    let bytes = prop.value.as_slice();
    let mut start = 0usize;
    while start < bytes.len() {
        let end = bytes[start..]
            .iter()
            .position(|&byte| byte == 0)
            .map(|offset| start + offset)
            .unwrap_or(bytes.len());
        if let Ok(entry) = core::str::from_utf8(&bytes[start..end])
            && entry == needle
        {
            return Ok(true);
        }
        start = end + 1;
    }
    Ok(false)
}

fn is_memory_node(source: &SourceTree<'_>, node_id: NodeId) -> Result<bool, &'static str> {
    let node = source.node(node_id).ok_or("dtb: invalid memory node")?;
    if node.name.as_str().starts_with("memory@") {
        return Ok(true);
    }
    Ok(property_string_equals(node, "device_type", "memory"))
}

fn is_linux_cma_node(source: &SourceTree<'_>, node_id: NodeId) -> Result<bool, &'static str> {
    let node = source
        .node(node_id)
        .ok_or("dtb: invalid reserved-memory child")?;
    if node.name.as_str().starts_with("linux,cma") {
        return Ok(true);
    }
    if node.property("linux,cma-default").is_some() {
        return Ok(true);
    }
    Ok(false)
}

fn property_string_equals(node: &::dtb::ast::Node<'_>, key: &str, expected: &str) -> bool {
    let Some(prop) = node.property(key) else {
        return false;
    };
    first_cstr(prop.value.as_slice()).is_some_and(|text| text == expected)
}

fn first_cstr(bytes: &[u8]) -> Option<&str> {
    let raw = bytes.split(|byte| *byte == 0).next()?;
    if raw.is_empty() {
        return None;
    }
    core::str::from_utf8(raw).ok()
}

fn property_u32<State>(
    tree: &DeviceTree<'_, State>,
    node_id: NodeId,
    key: &str,
) -> Result<Option<u32>, &'static str> {
    let Some(node) = tree.node(node_id) else {
        return Ok(None);
    };
    let Some(prop) = node.property(key) else {
        return Ok(None);
    };
    let bytes = prop.value.as_slice();
    if bytes.len() != 4 {
        return Ok(None);
    }
    Ok(Some(read_be_u32(bytes, 0)?))
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
    let end = offset.checked_add(4).ok_or("dtb: read_be_u32 overflow")?;
    let slice = bytes.get(offset..end).ok_or("dtb: read_be_u32 oob")?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn write_be_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), &'static str> {
    let end = offset.checked_add(4).ok_or("dtb: write_be_u32 overflow")?;
    let slice = bytes.get_mut(offset..end).ok_or("dtb: write_be_u32 oob")?;
    slice.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn write_be_u32s(
    bytes: &mut [u8],
    offset: usize,
    cells: usize,
    value: u64,
) -> Result<(), &'static str> {
    for index in 0..cells {
        let shift = 32 * (cells - 1 - index);
        let cell = ((value >> shift) & 0xffff_ffff) as u32;
        write_be_u32(bytes, offset + index * 4, cell)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::GUEST_ROOT_TOKEN;
    use super::GUEST_SYSTEMD_MASK_TOKENS;
    use super::bootargs_contains_guest_systemd_boot_tokens;
    use super::bootargs_contains_guest_systemd_masks;
    use super::encode_uart_passthrough_interrupts;
    use super::gic_dt_irq_flags_from_sense;
    use super::rewrite_bootargs;
    use crate::vgic::UartIrq;
    use alloc::vec::Vec;
    use arch_hal::gic::IrqSense;
    use arch_hal::gic::dt_irq::decode_gicv2_irq;

    const TEST_UART_ADDR: usize = 0x1c00_030000;
    const TEST_UART_INTID: u32 = 265;

    fn token_count(text: &str, needle: &str) -> usize {
        text.split_whitespace()
            .filter(|token| *token == needle)
            .count()
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn rewrite_bootargs_replaces_root_console_and_earlycon() {
        let rewritten = rewrite_bootargs(
            Some(
                "root=/dev/mmcblk0p2 ro console=ttyS0,115200 earlycon=uart8250,mmio32,0xfe201000 quiet",
            ),
            TEST_UART_ADDR,
        );

        assert!(rewritten.contains("quiet"));
        assert!(rewritten.contains(GUEST_ROOT_TOKEN));
        assert!(rewritten.contains("rootwait"));
        assert!(rewritten.contains("console=ttyAMA0,115200"));
        assert!(rewritten.contains("earlycon=pl011,0x1c00030000"));
        assert!(rewritten.contains("systemd.wants=serial-getty@ttyAMA0.service"));
        assert!(!rewritten.contains("systemd.mask=serial-getty@ttyAMA0.service"));
        assert!(bootargs_contains_guest_systemd_masks(&rewritten));
        assert!(bootargs_contains_guest_systemd_boot_tokens(&rewritten));
        assert!(!rewritten.contains("root=/dev/mmcblk0p2"));
        assert!(!rewritten.contains("console=ttyS0,115200"));
        assert!(!rewritten.contains("earlycon=uart8250"));
        assert!(!rewritten.contains("plymouth.ignore-serial-console"));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn rewrite_bootargs_preserves_single_rootwait_and_drops_virtio_mmio_device() {
        let rewritten = rewrite_bootargs(
            Some("rootwait root=/dev/vda1 virtio_mmio.device=0x200@0x1000:3 splash rootwait"),
            TEST_UART_ADDR,
        );

        assert_eq!(token_count(&rewritten, "rootwait"), 1);
        assert_eq!(token_count(&rewritten, GUEST_ROOT_TOKEN), 1);
        assert!(bootargs_contains_guest_systemd_masks(&rewritten));
        assert!(bootargs_contains_guest_systemd_boot_tokens(&rewritten));
        assert!(!rewritten.contains("virtio_mmio.device="));
        assert!(rewritten.contains("splash"));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn rewrite_bootargs_inserts_required_tokens_when_missing() {
        let rewritten = rewrite_bootargs(None, TEST_UART_ADDR);

        assert!(rewritten.contains(GUEST_ROOT_TOKEN));
        assert_eq!(token_count(&rewritten, "rootwait"), 1);
        assert!(rewritten.contains("console=ttyAMA0,115200"));
        assert!(rewritten.contains("earlycon=pl011,0x1c00030000"));
        assert!(bootargs_contains_guest_systemd_masks(&rewritten));
        assert!(bootargs_contains_guest_systemd_boot_tokens(&rewritten));
        assert!(!rewritten.contains("virtio_mmio.device="));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn rewrite_bootargs_deduplicates_guest_systemd_masks() {
        let existing = "systemd.mask=boot-firmware.mount quiet systemd.mask=dev-zram0.swap";
        let rewritten = rewrite_bootargs(Some(existing), TEST_UART_ADDR);

        assert!(rewritten.contains("quiet"));
        for token in GUEST_SYSTEMD_MASK_TOKENS {
            assert_eq!(token_count(&rewritten, token), 1);
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn rewrite_bootargs_deduplicates_guest_systemd_boot_tokens() {
        let existing = "systemd.unit=graphical.target quiet";
        let rewritten = rewrite_bootargs(Some(existing), TEST_UART_ADDR);

        assert!(rewritten.contains("quiet"));
        assert!(!rewritten.contains("systemd.unit=graphical.target"));
        assert_eq!(token_count(&rewritten, "systemd.unit=multi-user.target"), 1);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn gic_dt_irq_flags_follow_uart_irq_sense() {
        assert_eq!(gic_dt_irq_flags_from_sense(IrqSense::Level), 4);
        assert_eq!(gic_dt_irq_flags_from_sense(IrqSense::Edge), 1);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    #[cfg_attr(not(target_arch = "aarch64"), test)]
    fn encode_uart_passthrough_interrupts_uses_level_triggered_spi_specifier() {
        let interrupts = encode_uart_passthrough_interrupts(UartIrq {
            pintid: TEST_UART_INTID,
            sense: IrqSense::Level,
        })
        .unwrap();
        let cells: Vec<u32> = interrupts
            .chunks_exact(4)
            .map(|chunk| u32::from_be_bytes(chunk.try_into().unwrap()))
            .collect();
        let decoded = decode_gicv2_irq(&cells).unwrap();

        assert_eq!(decoded.intid, TEST_UART_INTID);
        assert_eq!(decoded.flags, 4);
    }
}
