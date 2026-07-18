#[cfg(target_arch = "aarch64")]
use core::ops::ControlFlow;
#[cfg(target_arch = "aarch64")]
use core::ptr::slice_from_raw_parts;

#[cfg(target_arch = "aarch64")]
use crate::bcm2712::brcmstb::Bcm2712PcieSetup;
#[cfg(target_arch = "aarch64")]
use crate::bcm2712::brcmstb::BrcmStb;
#[cfg(target_arch = "aarch64")]
use crate::bcm2712::brcmstb::PcieDmaWindow;
#[cfg(target_arch = "aarch64")]
use crate::bcm2712::brcmstb::PcieLinkSpeed;
#[cfg(target_arch = "aarch64")]
use crate::bcm2712::brcmstb::PcieStatus;
#[cfg(target_arch = "aarch64")]
use crate::bcm2712::brcmstb::RP1_EXPECTED_DMA_CPU_BASE;
#[cfg(target_arch = "aarch64")]
use crate::bcm2712::brcmstb::RP1_EXPECTED_DMA_PCIE_BASE;
#[cfg(target_arch = "aarch64")]
use dtb::DtbNodeView;
#[cfg(target_arch = "aarch64")]
use dtb::DtbParser;
#[cfg(target_arch = "aarch64")]
use dtb::WalkError;
#[cfg(target_arch = "aarch64")]
use dtb::WalkResult;
#[cfg(target_arch = "aarch64")]
use mutex::RawSpinLock;
#[cfg(target_arch = "aarch64")]
use pci::PciBhlc;
#[cfg(target_arch = "aarch64")]
use pci::PciHeaderKind;
#[cfg(target_arch = "aarch64")]
use pci::PciId;
#[cfg(target_arch = "aarch64")]
use pci::msix::PciMsiXTable;
#[cfg(target_arch = "aarch64")]
use print::println;
#[cfg(target_arch = "aarch64")]
use typestate::Readable;

#[cfg(target_arch = "aarch64")]
pub mod brcmstb;
#[cfg(target_arch = "aarch64")]
pub mod mip;
pub mod pcie_validation;
#[cfg(target_arch = "aarch64")]
pub mod pirq_hook;
#[cfg(target_arch = "aarch64")]
pub mod rp1;
#[cfg(any(target_arch = "aarch64", test))]
pub mod rp1_gem;
#[cfg(target_arch = "aarch64")]
pub mod rp1_interrupt;
pub mod sdhc;
#[cfg(target_arch = "aarch64")]
pub use pirq_hook::PIRQ_LIFECYCLE_HOOK;

#[cfg(target_arch = "aarch64")]
pub(crate) struct MsiXTablePtr {
    base: *const PciMsiXTable,
    len: usize,
}

#[cfg(target_arch = "aarch64")]
unsafe impl Send for MsiXTablePtr {}

#[cfg(target_arch = "aarch64")]
pub(crate) static MSIX_TABLE: RawSpinLock<Option<MsiXTablePtr>> = RawSpinLock::new(None);

#[cfg(target_arch = "aarch64")]
pub(crate) fn get_msi_x_table(table: &MsiXTablePtr) -> &'static [PciMsiXTable] {
    // SAFETY: `MsiXTablePtr` is created from a valid, MMIO-mapped MSI-X table during RP1 init
    // and remains valid for the lifetime of the system; `len` is the table entry count.
    unsafe { &*slice_from_raw_parts(table.base, table.len) }
}

#[cfg(target_arch = "aarch64")]
pub struct Rp1Config {
    pub peripheral_addr: Option<(u64, u64)>,
    pub shared_sram_addr: Option<(u64, u64)>,
    pub dma_window: Option<PcieDmaWindow>,
    pub msi_x_table_addr: Option<(u64, u64)>,
    pub pcie_base: Option<(u64, u64)>,
}

/// Selects whether RP1 uses firmware-provided PCIe state or a local RC reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rp1InitMode {
    FirmwareAssisted,
    FullPcieInit,
    Auto,
    AuditOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rp1InitOptions {
    pub mode: Rp1InitMode,
    pub strict: bool,
}

#[cfg(target_arch = "aarch64")]
pub fn init_rp1(dtb: &DtbParser) -> Result<Rp1Config, Bcm2712Error> {
    init_rp1_with_options(
        dtb,
        Rp1InitOptions {
            mode: Rp1InitMode::FirmwareAssisted,
            strict: true,
        },
    )
}

/// Emit read-only RC, bridge, endpoint-header, window, and AER diagnostics
/// after a post-enumeration BAR/MMIO access failure.
#[cfg(target_arch = "aarch64")]
pub fn dump_rp1_pcie_diagnostics(config: &Rp1Config) {
    let Some((base, _)) = config.pcie_base else {
        println!("PCIE: diag controller base unavailable");
        return;
    };
    let Ok(base) = usize::try_from(base) else {
        println!("PCIE: diag controller base does not fit usize");
        return;
    };
    // SAFETY: `base` comes from the validated BCM2712 PCIe DTB `reg` range;
    // dump_diagnostics performs only volatile reads except config-aperture BDF
    // selection needed to inspect RP1's header and extended capabilities.
    unsafe { (&*(base as *const BrcmStb)).dump_diagnostics() };
}

/// Initialise or audit the BCM2712/RP1 PCIe path according to an explicit
/// policy.  The compatibility `init_rp1` wrapper intentionally remains strict
/// firmware-assisted so existing hypervisor boot callers never reset PCIe.
#[cfg(target_arch = "aarch64")]
pub fn init_rp1_with_options(
    dtb: &DtbParser,
    options: Rp1InitOptions,
) -> Result<Rp1Config, Bcm2712Error> {
    println!(
        "PCIE: RP1 init mode: {:?} strict={}",
        options.mode, options.strict
    );
    let result = dtb.find_nodes_by_compatible_view("brcm,bcm2712-pcie", &mut |view, _name| {
        search_rp1(view, options)
    });
    match result {
        Ok(ControlFlow::Break(config)) => {
            let base = config.peripheral_addr.map(|(addr, _)| addr as usize);
            // SAFETY: RP1 base configuration is only mutated during initialization.
            unsafe { pirq_hook::set_rp1_peripheral_base(base) };
            Ok(config)
        }
        Ok(ControlFlow::Continue(())) => Err(Bcm2712Error::DtbDeviceNotFound),
        Err(WalkError::Dtb(err)) => Err(Bcm2712Error::DtbParseError(err)),
        Err(WalkError::User(err)) => Err(err),
    }
}

#[cfg(target_arch = "aarch64")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bcm2712Error {
    DtbParseError(&'static str),
    DtbDeviceNotFound,
    PcieIsNotInitialized,
    UnexpectedDevice(&'static str),
    InvalidWindow,
    InvalidPciHeaderType,
    InvalidSettings,
    MdioTimeout,
    LinkTimeout,
    PcieEndpointNotFound,
}

#[cfg(target_arch = "aarch64")]
fn search_rp1(view: &DtbNodeView, options: Rp1InitOptions) -> WalkResult<Rp1Config, Bcm2712Error> {
    let mut has_rp1 = false;
    view.for_each_child_view(&mut |child| {
        if child.name() == "rp1" {
            has_rp1 = true;
            let pcie_reg = view
                .reg_iter()
                .map_err(WalkError::Dtb)?
                .next()
                .ok_or(Bcm2712Error::DtbDeviceNotFound)?
                .map_err(WalkError::Dtb)?;
            println!(
                "PCIE: base_addr=0x{:x}, size=0x{:x}",
                pcie_reg.0, pcie_reg.1
            );

            let brcm_stb = BrcmStb::new(pcie_reg.0);

            let result = view.with_msi_parent_view(&mut |mip, mip_name| {
                if !mip
                    .compatible_contains("brcm,bcm2712-mip")
                    .map_err(WalkError::Dtb)?
                {
                    return Err(Bcm2712Error::UnexpectedDevice(mip_name).into());
                }

                let mip_reg = mip
                    .reg_iter()
                    .map_err(WalkError::Dtb)?
                    .next()
                    .ok_or(Bcm2712Error::DtbDeviceNotFound)?
                    .map_err(WalkError::Dtb)?;
                println!("PCIE: MIP addr: 0x{:x}, size: 0x{:x}", mip_reg.0, mip_reg.1);

                let link_was_up = link_up(brcm_stb);
                let mut full_init_ran = false;
                match options.mode {
                    Rp1InitMode::FirmwareAssisted | Rp1InitMode::AuditOnly if !link_was_up => {
                        println!(
                            "PCIE: link initially down status=0x{:08x}",
                            brcm_stb.link_status_raw()
                        );
                        return Err(Bcm2712Error::PcieIsNotInitialized.into());
                    }
                    Rp1InitMode::FirmwareAssisted | Rp1InitMode::AuditOnly => {
                        println!("PCIE: link initially up; firmware-assisted audit");
                    }
                    Rp1InitMode::FullPcieInit => {
                        println!("PCIE: attempting BCM2712 PCIe full init");
                        let setup = pcie_setup_from_dtb(view).map_err(WalkError::User)?;
                        brcm_stb.init_bcm2712_root_complex(&setup).map_err(WalkError::User)?;
                        full_init_ran = true;
                    }
                    Rp1InitMode::Auto if link_was_up => {
                        println!("PCIE: link initially up; reusing firmware setup");
                    }
                    Rp1InitMode::Auto => {
                        println!("PCIE: link initially down");
                        println!("PCIE: attempting BCM2712 PCIe full init");
                        let setup = pcie_setup_from_dtb(view).map_err(WalkError::User)?;
                        brcm_stb.init_bcm2712_root_complex(&setup).map_err(WalkError::User)?;
                        full_init_ran = true;
                    }
                }

                if !link_up(brcm_stb) {
                    println!("PCIE: link remains down status=0x{:08x}", brcm_stb.link_status_raw());
                    return Err(Bcm2712Error::PcieIsNotInitialized.into());
                }

                if full_init_ran {
                    let outbound = brcm_stb
                        .read_outbound_window(1)
                        .map_err(WalkError::User)?
                        .ok_or(Bcm2712Error::InvalidWindow)
                        .map_err(WalkError::User)?;
                    brcm_stb
                        .configure_rp1_endpoint(outbound)
                        .map_err(WalkError::User)?;
                }

                // check bar address
                let config = brcm_stb.set_config_window()?;
                println!(
                    "PCIE: RP1 Vender ID: 0x{:x}",
                    config.id.read().get(PciId::vendor_id)
                );
                println!(
                    "PCIE: RP1 Device ID: 0x{:x}",
                    config.id.read().get(PciId::device_id)
                );
                if config
                    .bhlc
                    .read()
                    .get_enum(PciBhlc::header_kind)
                    .is_none_or(|t: PciHeaderKind| t != PciHeaderKind::Standard)
                {
                    return Err(Bcm2712Error::InvalidPciHeaderType.into());
                }

                // SAFETY: `set_config_window` above selected RP1 function 0, and this scan
                // only reads its standard PCI capability list.
                let msi_x = unsafe { brcm_stb.get_msi_x_capability() }?;
                // SAFETY: `msi_x` points into the currently selected RP1 config space and
                // remains valid until the next config-window change in this initialization.
                let (msi_x_bar, msi_x_entries) = unsafe { brcm_stb.msi_x_table_bar_addr(msi_x)? };
                let msi_x_len = (msi_x_entries as u64)
                    .checked_mul(size_of::<PciMsiXTable>() as u64)
                    .ok_or(Bcm2712Error::InvalidWindow)?;
                let msi_x_end = msi_x_bar
                    .checked_add(msi_x_len)
                    .ok_or(Bcm2712Error::InvalidWindow)?;
                // SAFETY: the RP1 config window is selected and BAR access reads function 0.
                let bar1 = unsafe { brcm_stb.read_rp1_bar1_address() }?;
                // SAFETY: the RP1 config window is selected and BAR access reads function 0.
                let bar2 = unsafe { brcm_stb.read_bar_address(2) }?;
                println!("PCIE: RP1 Pheripheral BAR addr: 0x{:x}", bar1);
                println!("PCIE: RP1 Shared SRAM BAR addr: 0x{:x}", bar2);

                let mut msi_x_table_addr = None;
                let mut peripheral_addr = None;
                let mut shared_sram_addr = None;

                // check outbound windows
                for i in 1..=4 {
                    if let Ok(Some(bar)) = brcm_stb.read_outbound_window(i) {
                        println!(
                            "PCIE: outbound window {}: pcie_base: 0x{:x} cpu_base: 0x{:x} size: 0x{:x}",
                            i, bar.pcie_base, bar.cpu_base, bar.size
                        );
                        let Some(bar_end) = bar.pcie_base.checked_add(bar.size) else {
                            return Err(Bcm2712Error::InvalidWindow.into());
                        };
                        if bar.pcie_base <= msi_x_bar && msi_x_end <= bar_end {
                            let offset = msi_x_bar
                                .checked_sub(bar.pcie_base)
                                .ok_or(Bcm2712Error::InvalidWindow)?;
                            msi_x_table_addr = Some(
                                bar.cpu_base
                                    .checked_add(offset)
                                    .ok_or(Bcm2712Error::InvalidWindow)?,
                            );
                        }
                        if (bar.pcie_base..bar_end).contains(&bar1) {
                            let peripheral_end = bar1
                                .checked_add(0x40_0000)
                                .ok_or(Bcm2712Error::InvalidWindow)?;
                            if peripheral_end > bar_end {
                                return Err(Bcm2712Error::InvalidWindow.into());
                            }
                            let offset = bar1
                                .checked_sub(bar.pcie_base)
                                .ok_or(Bcm2712Error::InvalidWindow)?;
                            peripheral_addr = Some((
                                bar.cpu_base
                                    .checked_add(offset)
                                    .ok_or(Bcm2712Error::InvalidWindow)?,
                                0x40_0000,
                            ));
                        }
                        if (bar.pcie_base..bar_end).contains(&bar2) {
                            let shared_sram_end = bar2
                                .checked_add(0x1_0000)
                                .ok_or(Bcm2712Error::InvalidWindow)?;
                            if shared_sram_end > bar_end {
                                return Err(Bcm2712Error::InvalidWindow.into());
                            }
                            let offset = bar2
                                .checked_sub(bar.pcie_base)
                                .ok_or(Bcm2712Error::InvalidWindow)?;
                            shared_sram_addr = Some((
                                bar.cpu_base
                                    .checked_add(offset)
                                    .ok_or(Bcm2712Error::InvalidWindow)?,
                                0x1_0000,
                            ));
                        }
                    }
                }
                let Some(msi_x_table_addr) = msi_x_table_addr else {
                    println!("PCIE: msi x window are not set");
                    return Err(Bcm2712Error::InvalidWindow.into());
                };
                println!(
                    "PCIE: MSI-X table pcie=0x{:x} cpu=0x{:x} len=0x{:x}",
                    msi_x_bar, msi_x_table_addr, msi_x_len
                );
                let mut table = MSIX_TABLE.lock();
                // SAFETY: the selected outbound window maps the complete MSI-X table range
                // calculated above, and the config window has not changed since `msi_x`.
                *table = Some(unsafe {
                    brcm_stb.init_rp1_msi_x_settings(msi_x, msi_x_table_addr, 0xff_ffff_f000)
                }?);

                let dma_window = match brcm_stb.find_dma_window(
                    RP1_EXPECTED_DMA_PCIE_BASE,
                    RP1_EXPECTED_DMA_CPU_BASE,
                    0x1_0000_0000,
                )? {
                    Some(window) => window,
                    None => brcm_stb.ensure_dma_window(None, PcieDmaWindow::expected_rp1())?,
                };

                rp1_interrupt::init_rp1_interrupt(brcm_stb, mip_reg.0 as u64)?;

                Ok(ControlFlow::Break(Rp1Config {
                    peripheral_addr,
                    shared_sram_addr,
                    dma_window: Some(dma_window),
                    msi_x_table_addr: Some((msi_x_table_addr, msi_x_len)),
                    pcie_base: Some((pcie_reg.0 as u64, pcie_reg.1 as u64)),
                }))
            });

            match result {
                Ok(ControlFlow::Break(config)) => Ok(ControlFlow::Break(config)),
                Ok(ControlFlow::Continue(())) => Err(Bcm2712Error::DtbDeviceNotFound.into()),
                Err(err) => {
                    brcm_stb.dump_diagnostics();
                    Err(err)
                }
            }
        } else {
            Ok(ControlFlow::Continue(()))
        }
    })
}

#[cfg(target_arch = "aarch64")]
fn read_be32(bytes: &[u8]) -> Result<u32, Bcm2712Error> {
    let array: [u8; 4] = bytes
        .try_into()
        .map_err(|_| Bcm2712Error::DtbParseError("PCIE: truncated ranges cell"))?;
    Ok(u32::from_be_bytes(array))
}

/// Parse only the standard PCI memory entry from a BCM2712 controller's
/// `ranges`: child PCI address (three cells), CPU address (two cells), size
/// (two cells).  This avoids widening the generic DTB API solely for PCI's
/// flag-bearing three-cell child address.
#[cfg(target_arch = "aarch64")]
fn pcie_setup_from_dtb(view: &DtbNodeView) -> Result<Bcm2712PcieSetup, Bcm2712Error> {
    let ranges = view
        .property_bytes("ranges")
        .map_err(Bcm2712Error::DtbParseError)?
        .ok_or(Bcm2712Error::DtbParseError("PCIE: missing ranges"))?;
    if ranges.len() % 28 != 0 {
        return Err(Bcm2712Error::DtbParseError("PCIE: malformed PCI ranges"));
    }

    let mut selected = None;
    for entry in ranges.chunks_exact(28) {
        let flags = read_be32(&entry[0..4])?;
        // PCI space code 0x02 denotes non-prefetchable memory; prefer it for
        // the RP1 BAR aperture, while retaining the exact DTB addresses.
        if flags & 0x0300_0000 != 0x0200_0000 {
            continue;
        }
        let pcie_base =
            ((read_be32(&entry[4..8])? as u64) << 32) | read_be32(&entry[8..12])? as u64;
        let cpu_base =
            ((read_be32(&entry[12..16])? as u64) << 32) | read_be32(&entry[16..20])? as u64;
        let size = ((read_be32(&entry[20..24])? as u64) << 32) | read_be32(&entry[24..28])? as u64;
        // BRCM STB outbound windows are 1 MiB granular.  Some firmware DTBs
        // advertise a PCI aperture with a small trailing fragment; retain the
        // DTB bases and use its largest representable prefix rather than
        // inventing an aperture or silently changing either base.
        let usable_size = size & !0x000f_ffff;
        if usable_size != 0 && cpu_base & 0x000f_ffff == 0 && pcie_base & 0x000f_ffff == 0 {
            if usable_size != size {
                println!(
                    "PCIE: DTB ranges size 0x{:x} trimmed to 1 MiB granularity 0x{:x}",
                    size, usable_size
                );
            }
            selected = Some((cpu_base, pcie_base, usable_size));
            break;
        }
    }
    let (outbound_cpu_base, outbound_pcie_base, outbound_size) = selected.ok_or(
        Bcm2712Error::DtbParseError("PCIE: no 1 MiB-aligned memory ranges entry"),
    )?;
    let target_link_speed = match view
        .property_u32_be("max-link-speed")
        .map_err(Bcm2712Error::DtbParseError)?
    {
        Some(1) => Some(PcieLinkSpeed::Gen1),
        Some(2) => Some(PcieLinkSpeed::Gen2),
        Some(3) => Some(PcieLinkSpeed::Gen3),
        Some(_) => None,
        None => Some(PcieLinkSpeed::Gen2),
    };
    println!(
        "PCIE: DTB outbound cpu=0x{:x} pcie=0x{:x} size=0x{:x}",
        outbound_cpu_base, outbound_pcie_base, outbound_size
    );
    Ok(Bcm2712PcieSetup {
        outbound_cpu_base,
        outbound_pcie_base,
        outbound_size,
        inbound_dma_window: PcieDmaWindow::expected_rp1(),
        target_link_speed,
    })
}

#[cfg(target_arch = "aarch64")]
fn link_up(brcm_stb: &BrcmStb) -> bool {
    let status = brcm_stb.pcie_status.read();
    status.get(PcieStatus::phy_linkup) != 0 && status.get(PcieStatus::dl_active) != 0
}
