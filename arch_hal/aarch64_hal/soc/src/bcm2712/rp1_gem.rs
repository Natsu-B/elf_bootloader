//! Polling Cadence GEM driver for the RP1 Ethernet controller.
//!
//! RP1 exposes a GEM_GXL (Cadence MACB/GEM compatible) instance through its
//! peripheral BAR.  Descriptor and packet addresses are PCIe DMA addresses,
//! not CPU virtual or BAR addresses; this implementation deliberately uses
//! the extended 16-byte descriptor format and the 64-bit queue base registers.

#[cfg(target_arch = "aarch64")]
use core::arch::asm;
#[cfg(target_arch = "aarch64")]
use core::cell::UnsafeCell;
use core::hint::spin_loop;
#[cfg(target_arch = "aarch64")]
use core::mem::MaybeUninit;
#[cfg(target_arch = "aarch64")]
use core::time::Duration;

#[cfg(target_arch = "aarch64")]
use cpu::clean_dcache_range;
#[cfg(target_arch = "aarch64")]
use cpu::dsb_sy;
#[cfg(target_arch = "aarch64")]
use cpu::invalidate_dcache_range;
#[cfg(target_arch = "aarch64")]
use cpu::va_to_pa_el2_read;
#[cfg(target_arch = "aarch64")]
use io_api::ethernet::EthernetFrameIo;
use io_api::ethernet::MacAddr;
#[cfg(target_arch = "aarch64")]
use print::println;
#[cfg(target_arch = "aarch64")]
use timer::SystemTimer;

#[cfg(target_arch = "aarch64")]
use crate::bcm2712::Bcm2712Error;
#[cfg(target_arch = "aarch64")]
use crate::bcm2712::Rp1Config;
#[cfg(target_arch = "aarch64")]
use crate::bcm2712::brcmstb::PcieDmaWindow;
#[cfg(target_arch = "aarch64")]
use crate::bcm2712::rp1::Rp1GpioBank0;
#[cfg(target_arch = "aarch64")]
use crate::bcm2712::rp1::Rp1PeripheralMap;

const GEM_APERTURE_SIZE: usize = 0x4000;
const GEM_CFG_APERTURE_SIZE: usize = 0x1000;

const NCR: usize = 0x0000;
const NCFGR: usize = 0x0004;
const NSR: usize = 0x0008;
const DMACFG: usize = 0x0010;
const TSR: usize = 0x0014;
const RBQP: usize = 0x0018;
const TBQP: usize = 0x001c;
const RSR: usize = 0x0020;
const ISR: usize = 0x0024;
const IDR: usize = 0x002c;
const IMR: usize = 0x0030;
const MAN: usize = 0x0034;
const GEM_AMP: usize = 0x0054;
const SA1B: usize = 0x0088;
const SA1T: usize = 0x008c;
const USRIO: usize = 0x00c0;
const MID: usize = 0x00fc;
const TBQPH: usize = 0x04c8;
const RBQPH: usize = 0x04d4;

const GEM_CFG_CONTROL: usize = 0x00;
const GEM_CFG_STATUS: usize = 0x04;
const GEM_CFG_CLKGEN: usize = 0x14;

// RP1 GEM configuration block values. `MEM_PD` is active high and bus-error
// reporting is bit 2, as defined by RP1's Ethernet control register.
const GEM_CFG_CTRL_BUS_ERROR_REPORT: u32 = 1 << 2;
const GEM_CFG_CTRL_MEM_PD: u32 = 1 << 3;
const GEM_CFG_CLKGEN_ENABLE: u32 = 1 << 0;

const NCR_RE: u32 = 1 << 2;
const NCR_TE: u32 = 1 << 3;
const NCR_MPE: u32 = 1 << 4;
const NCR_CLRSTAT: u32 = 1 << 5;
const NCR_TSTART: u32 = 1 << 9;

const NCFGR_SPD_100: u32 = 1 << 0;
const NCFGR_FD: u32 = 1 << 1;
const NCFGR_DRFCS: u32 = 1 << 17;
const NCFGR_GBE: u32 = 1 << 10;
const NCFGR_MDC_DIV_64: u32 = 0b100 << 18;
const NCFGR_DBW_128: u32 = 0b010 << 21;

const NSR_IDLE: u32 = 1 << 2;
const TSR_COMPLETE: u32 = 1 << 5;

const DMACFG_BURST_16: u32 = 16;
const DMACFG_RX_FULL_PACKET: u32 = 1 << 8;
const DMACFG_TX_FULL_PACKET: u32 = 1 << 10;
const DMACFG_RX_BUFFER_SHIFT: u32 = 16;
const DMACFG_ADDR64: u32 = 1 << 30;

const RX_DESC_USED: u32 = 1 << 0;
const RX_DESC_WRAP: u32 = 1 << 1;
const RX_STATUS_LEN_MASK: u32 = 0x1fff;
const RX_STATUS_SOF: u32 = 1 << 14;
const RX_STATUS_EOF: u32 = 1 << 15;
const TX_DESC_LEN_MASK: u32 = 0x3fff;
const TX_DESC_LAST: u32 = 1 << 15;
const TX_DESC_WRAP: u32 = 1 << 30;
const TX_DESC_USED: u32 = 1 << 31;

const PHY_RESET_PIN: usize = 32;
const PHY_BMCR: u8 = 0;
const PHY_BMSR: u8 = 1;
const PHY_ID1: u8 = 2;
const PHY_ID2: u8 = 3;
const PHY_ADVERTISE: u8 = 4;
const PHY_LPA: u8 = 5;
const PHY_CTRL1000: u8 = 9;
const PHY_STAT1000: u8 = 10;
const PHY_BMCR_SPEED100: u16 = 1 << 13;
const PHY_BMCR_SPEED1000: u16 = 1 << 6;
const PHY_BMCR_FULLDPLX: u16 = 1 << 8;
const PHY_BMCR_ANENABLE: u16 = 1 << 12;
const PHY_BMCR_ANRESTART: u16 = 1 << 9;
const PHY_BMSR_LINK: u16 = 1 << 2;
const PHY_BMSR_ANEG_COMPLETE: u16 = 1 << 5;
const PHY_ADV_10FULL: u16 = 1 << 6;
const PHY_ADV_100HALF: u16 = 1 << 7;
const PHY_ADV_100FULL: u16 = 1 << 8;
const PHY_CTRL1000_HALF: u16 = 1 << 8;
const PHY_CTRL1000_FULL: u16 = 1 << 9;
const PHY_STAT1000_HALF: u16 = 1 << 10;
const PHY_STAT1000_FULL: u16 = 1 << 11;

const RX_RING_LEN: usize = 64;
const TX_RING_LEN: usize = 16;
const DMA_BUF_LEN: usize = 2048;
const MAX_FRAME_LEN: usize = 1518;
const MDIO_POLL_LIMIT: usize = 20_000;
const TX_POLL_LIMIT: usize = 100_000;
const QUIESCE_POLL_LIMIT: usize = 1_000;
const QUIESCE_NCR_MASK: u32 = NCR_RE | NCR_TE | NCR_TSTART | NCR_MPE;
const LINK_POLL_INTERVAL_US: u64 = 10_000;
const DEFAULT_LINK_WAIT_US: u64 = 5_000_000;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct GemDescriptor {
    addr_lo: u32,
    ctrl_status: u32,
    addr_hi: u32,
    reserved: u32,
}

impl GemDescriptor {
    const ZERO: Self = Self {
        addr_lo: 0,
        ctrl_status: 0,
        addr_hi: 0,
        reserved: 0,
    };

    const fn rx_addr(dma: u64, wrap: bool) -> (u32, u32) {
        (
            dma as u32 | if wrap { RX_DESC_WRAP } else { 0 },
            (dma >> 32) as u32,
        )
    }

    const fn tx_ctrl(len: usize, wrap: bool, used: bool) -> u32 {
        (len as u32 & TX_DESC_LEN_MASK)
            | TX_DESC_LAST
            | if wrap { TX_DESC_WRAP } else { 0 }
            | if used { TX_DESC_USED } else { 0 }
    }
}

#[cfg(target_arch = "aarch64")]
#[repr(C, align(64))]
struct DmaStorage {
    rx_desc: [GemDescriptor; RX_RING_LEN],
    tx_desc: [GemDescriptor; TX_RING_LEN],
    rx_buffers: [[u8; DMA_BUF_LEN]; RX_RING_LEN],
    tx_buffer: [u8; DMA_BUF_LEN],
}

#[cfg(target_arch = "aarch64")]
impl DmaStorage {
    const ZERO: Self = Self {
        rx_desc: [GemDescriptor::ZERO; RX_RING_LEN],
        tx_desc: [GemDescriptor::ZERO; TX_RING_LEN],
        rx_buffers: [[0; DMA_BUF_LEN]; RX_RING_LEN],
        tx_buffer: [0; DMA_BUF_LEN],
    };
}

#[cfg(target_arch = "aarch64")]
struct StaticCell<T>(UnsafeCell<T>);

// SAFETY: access is limited to the bootstrap CPU during RP1 GEM initialization;
// after construction the driver is exposed only through its unique singleton
// mutable reference.
#[cfg(target_arch = "aarch64")]
unsafe impl<T> Sync for StaticCell<T> {}

#[cfg(target_arch = "aarch64")]
static TAKEN: StaticCell<bool> = StaticCell(UnsafeCell::new(false));
#[cfg(target_arch = "aarch64")]
static DMA_STORAGE: StaticCell<DmaStorage> = StaticCell(UnsafeCell::new(DmaStorage::ZERO));
#[cfg(target_arch = "aarch64")]
static DRIVER: StaticCell<MaybeUninit<Rp1Gem>> = StaticCell(UnsafeCell::new(MaybeUninit::uninit()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSpeed {
    Mbps10,
    Mbps100,
    Mbps1000,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkState {
    pub up: bool,
    pub speed: LinkSpeed,
    pub full_duplex: bool,
}

#[cfg(target_arch = "aarch64")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rp1GemConfig {
    pub gem_base: usize,
    pub gem_cfg_base: usize,
    pub dma_window: PcieDmaWindow,
    pub mac_addr: MacAddr,
}

#[cfg(target_arch = "aarch64")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rp1GemOptions {
    /// Explicit Clause-22 PHY address.  `None` scans all 32 addresses.
    pub phy_addr: Option<u8>,
    /// Bounded auto-negotiation timeout.  Zero selects the 5 second default.
    pub link_wait_us: u64,
}

#[cfg(target_arch = "aarch64")]
impl Default for Rp1GemOptions {
    fn default() -> Self {
        Self {
            phy_addr: None,
            link_wait_us: DEFAULT_LINK_WAIT_US,
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rp1GemError {
    AlreadyTaken,
    InvalidMacAddress,
    InvalidMmio,
    MissingDmaWindow,
    InvalidWindow,
    AddressTranslationFailed,
    DmaAddressNotCovered,
    MdioTimeout,
    /// The last NCR still had requested enable/start bits set after bounded polling.
    QuiesceTimeout {
        ncr: u32,
    },
    NoPhy,
    LinkTimeout,
    TxTimeout,
    TxFrameInvalid,
    RxDescriptorError {
        addr_lo: u32,
        status: u32,
    },
    RxFrameTooLarge {
        len: usize,
    },
    Gpio(Bcm2712Error),
}

#[cfg(target_arch = "aarch64")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rp1GemDiagnostic {
    pub gem_base: usize,
    pub gem_cfg_base: usize,
    pub mid: u32,
    pub cfg_control: u32,
    pub cfg_status: u32,
    pub cfg_clkgen: u32,
    pub ncr: u32,
    pub ncfgr: u32,
    pub dmacfg: u32,
    pub tsr: u32,
    pub rsr: u32,
    pub isr: u32,
    pub imr: u32,
    pub nsr: u32,
    pub rx_ring_index: usize,
    pub rx_desc_addr_lo: u32,
    pub rx_desc_ctrl_status: u32,
    pub rx_desc_addr_hi: u32,
}

/// Polling, no-allocation RP1 Cadence GEM instance.
#[cfg(target_arch = "aarch64")]
pub struct Rp1Gem {
    config: Rp1GemConfig,
    phy_addr: u8,
    tx_index: usize,
    rx_index: usize,
    last_error: Option<Rp1GemError>,
}

#[cfg(target_arch = "aarch64")]
impl Rp1Gem {
    pub fn init_from_rp1_config(
        rp1: &Rp1Config,
        mac_addr: MacAddr,
        options: Rp1GemOptions,
    ) -> Result<&'static mut Self, Rp1GemError> {
        println!("[rp1-gem] init enter");
        if is_zero_mac(mac_addr) {
            return Err(Rp1GemError::InvalidMacAddress);
        }
        println!("[rp1-gem] init MAC accepted");
        println!("[rp1-gem] init claim singleton");
        if !claim_singleton() {
            return Err(Rp1GemError::AlreadyTaken);
        }
        println!("[rp1-gem] init singleton claimed");

        let result = (|| {
            println!("[rp1-gem] init map RP1 peripherals");
            let map = Rp1PeripheralMap::from_config(rp1).map_err(|_| Rp1GemError::InvalidWindow)?;
            println!("[rp1-gem] init RP1 map complete");
            let dma_window = rp1.dma_window.ok_or(Rp1GemError::MissingDmaWindow)?;
            let config = Rp1GemConfig {
                gem_base: map.rp1_gem_base().map_err(|_| Rp1GemError::InvalidWindow)?,
                gem_cfg_base: map
                    .rp1_gem_cfg_base()
                    .map_err(|_| Rp1GemError::InvalidWindow)?,
                dma_window,
                mac_addr,
            };
            println!("[rp1-gem] init read MID");
            let mid = read_mmio(config.gem_base, GEM_APERTURE_SIZE, MID);
            if mid == 0 || mid == u32::MAX {
                return Err(Rp1GemError::InvalidMmio);
            }
            println!("[rp1-gem] init MID=0x{:08x}", mid);

            let mut gem = Self {
                config,
                phy_addr: 0,
                tx_index: 0,
                rx_index: 0,
                last_error: None,
            };
            println!("[rp1-gem] init configure CFG");
            gem.configure_cfg();
            println!("[rp1-gem] init CFG complete");
            println!("[rp1-gem] init PHY reset");
            gem.reset_phy(&map)?;
            println!("[rp1-gem] init PHY reset complete");
            gem.program_mac_address();
            println!("[rp1-gem] init MAC programmed");
            gem.enable_mdio();
            println!("[rp1-gem] init MDIO enabled");
            gem.phy_addr = gem.find_phy(options.phy_addr)?;
            println!("[rp1-gem] init PHY found addr={}", gem.phy_addr);
            gem.restart_autoneg_and_wait(options.link_wait_us)?;
            println!("[rp1-gem] init link complete");
            gem.program_dma()?;
            println!("[rp1-gem] init DMA complete");
            gem.enable_rx_tx();
            println!("[rp1-gem] init RX/TX enabled");
            Ok(gem)
        })();

        match result {
            Ok(gem) => {
                // SAFETY: `TAKEN` grants exclusive construction. `DRIVER` is static storage
                // with sufficient size/alignment and is not read before this write completes.
                unsafe {
                    (*DRIVER.0.get()).write(gem);
                    Ok(&mut *(*DRIVER.0.get()).as_mut_ptr())
                }
            }
            Err(err) => {
                release_singleton();
                Err(err)
            }
        }
    }

    pub fn diagnostic_snapshot(&mut self) -> Rp1GemDiagnostic {
        let rx_index = self.rx_index;
        let storage = dma_storage();
        let rx_desc = &storage.rx_desc[rx_index];
        invalidate_dcache_range(
            rx_desc as *const GemDescriptor as *const u8,
            core::mem::size_of::<GemDescriptor>(),
        );
        Rp1GemDiagnostic {
            gem_base: self.config.gem_base,
            gem_cfg_base: self.config.gem_cfg_base,
            mid: self.reg_read(MID),
            cfg_control: self.cfg_read(GEM_CFG_CONTROL),
            cfg_status: self.cfg_read(GEM_CFG_STATUS),
            cfg_clkgen: self.cfg_read(GEM_CFG_CLKGEN),
            ncr: self.reg_read(NCR),
            ncfgr: self.reg_read(NCFGR),
            dmacfg: self.reg_read(DMACFG),
            tsr: self.reg_read(TSR),
            rsr: self.reg_read(RSR),
            isr: self.reg_read(ISR),
            imr: self.reg_read(IMR),
            nsr: self.reg_read(NSR),
            rx_ring_index: rx_index,
            rx_desc_addr_lo: rx_desc.addr_lo,
            rx_desc_ctrl_status: rx_desc.ctrl_status,
            rx_desc_addr_hi: rx_desc.addr_hi,
        }
    }

    pub fn phy_address(&self) -> u8 {
        self.phy_addr
    }

    pub fn phy_id(&mut self) -> Result<(u16, u16), Rp1GemError> {
        Ok((
            self.mdio_read(self.phy_addr, PHY_ID1)?,
            self.mdio_read(self.phy_addr, PHY_ID2)?,
        ))
    }

    pub fn link_state(&mut self) -> Result<LinkState, Rp1GemError> {
        self.read_link_state()
    }

    pub fn take_last_error(&mut self) -> Option<Rp1GemError> {
        self.last_error.take()
    }

    /// Checks the NCR stop request and releases the singleton only on success.
    ///
    /// # Safety
    ///
    /// The caller must stop using this `Rp1Gem` reference immediately after
    /// this call returns `Ok`. On `Err`, ownership and DMA storage stay retained.
    /// The next successful `init_from_rp1_config` after release will
    /// overwrite the same static driver storage and create a fresh unique
    /// mutable reference for the reinitialized RP1 instance.
    pub unsafe fn release_after_quiesce(&mut self) -> Result<u32, Rp1GemError> {
        let ncr = self.quiesce()?;
        release_singleton();
        Ok(ncr)
    }

    /// Quiesces the device for Linux and releases the singleton claim.
    ///
    /// # Safety
    ///
    /// The caller must stop using this `Rp1Gem` reference immediately after
    /// this call returns `Ok`. On `Err`, the singleton remains owned; do not
    /// jump to Linux. This is intended for the terminal Linux handoff path.
    pub unsafe fn release_after_linux_handoff(&mut self) -> Result<u32, Rp1GemError> {
        let ncr = self.handoff_to_linux()?;
        release_singleton();
        Ok(ncr)
    }

    /// Checks NCR enable/start bits, then clears bootstrap queue configuration.
    ///
    /// This deliberately does not reset RP1 or power down the GEM block, because
    /// Linux still needs the freshly loaded RP1 firmware and will reprogram the
    /// MAC/DMA state from its own driver.
    pub fn handoff_to_linux(&mut self) -> Result<u32, Rp1GemError> {
        let ncr = self.quiesce()?;

        // Linux will install its own DMA rings.  After RX/TX are stopped, clear
        // queue base registers and DMA configuration so no stale bootstrap
        // descriptors remain advertised to the MAC.
        self.reg_write(RBQP, 0);
        self.reg_write(RBQPH, 0);
        self.reg_write(TBQP, 0);
        self.reg_write(TBQPH, 0);
        self.reg_write(DMACFG, 0);

        self.reg_write(TSR, u32::MAX);
        self.reg_write(RSR, u32::MAX);
        self.reg_write(ISR, u32::MAX);
        dsb_sy();
        Ok(ncr)
    }

    /// Requests RX/TX/management stop and returns the observed disabled NCR.
    ///
    /// A bounded poll timeout is an error, not permission to reload RP1 or hand
    /// it off. Storage stays allocated on either result. NCR readback alone
    /// does not prove that all in-flight DMA transactions have completed.
    pub fn quiesce(&mut self) -> Result<u32, Rp1GemError> {
        // Disable every interrupt source before changing DMA ownership.  IDR is
        // a set-disable register, so writing all ones is idempotent.
        self.reg_write(IDR, u32::MAX);

        // Clear receive/transmit enable and the transmit-start command while
        // retaining no management-port enable: a handoff does not need MDIO,
        // and verify those requested control bits below.
        let ncr = self.reg_read(NCR);
        self.reg_write(NCR, (ncr & !QUIESCE_NCR_MASK) | NCR_CLRSTAT);
        let ncr = poll_ncr_stopped(|| self.reg_read(NCR))
            .map_err(|ncr| Rp1GemError::QuiesceTimeout { ncr })?;

        // These status registers are write-one-to-clear.  A final ISR read is
        // intentionally avoided because it can have device-specific effects.
        self.reg_write(TSR, u32::MAX);
        self.reg_write(RSR, u32::MAX);
        self.reg_write(ISR, u32::MAX);
        dsb_sy();
        Ok(ncr)
    }

    pub fn send_test_broadcast(&mut self) -> Result<(), Rp1GemError> {
        let mut frame = [0u8; 60];
        frame[..6].fill(0xff);
        frame[6..12].copy_from_slice(&self.config.mac_addr.0);
        frame[12] = 0x88;
        frame[13] = 0xb5;
        frame[14..].fill(0xa5);
        self.send_frame(&frame)
    }

    pub fn mdio_read(&mut self, phy: u8, reg: u8) -> Result<u16, Rp1GemError> {
        self.wait_mdio_idle()?;
        self.reg_write(MAN, mdio_command(0b10, phy, reg, 0));
        self.wait_mdio_idle()?;
        Ok((self.reg_read(MAN) & 0xffff) as u16)
    }

    pub fn mdio_write(&mut self, phy: u8, reg: u8, value: u16) -> Result<(), Rp1GemError> {
        self.wait_mdio_idle()?;
        self.reg_write(MAN, mdio_command(0b01, phy, reg, value));
        self.wait_mdio_idle()
    }

    fn configure_cfg(&self) {
        let control = self.cfg_read(GEM_CFG_CONTROL);
        self.cfg_write(
            GEM_CFG_CONTROL,
            (control | GEM_CFG_CTRL_BUS_ERROR_REPORT) & !GEM_CFG_CTRL_MEM_PD,
        );
        self.cfg_write(
            GEM_CFG_CLKGEN,
            self.cfg_read(GEM_CFG_CLKGEN) | GEM_CFG_CLKGEN_ENABLE,
        );
        dsb_sy();
    }

    fn reset_phy(&self, map: &Rp1PeripheralMap) -> Result<(), Rp1GemError> {
        println!("[rp1-gem] PHY reset: resolve GPIO32");
        let gpio = Rp1GpioBank0::from_map(map).map_err(Rp1GemError::Gpio)?;
        println!("[rp1-gem] PHY reset: configure GPIO32 output");
        gpio.configure_gpio_output(PHY_RESET_PIN)
            .map_err(Rp1GemError::Gpio)?;
        println!("[rp1-gem] PHY reset: GPIO32 output configured");
        gpio.set_gpio_output(PHY_RESET_PIN, false)
            .map_err(Rp1GemError::Gpio)?;
        println!("[rp1-gem] PHY reset: GPIO32 low");
        delay_us(5_000);
        println!("[rp1-gem] PHY reset: low delay complete");
        gpio.set_gpio_output(PHY_RESET_PIN, true)
            .map_err(Rp1GemError::Gpio)?;
        println!("[rp1-gem] PHY reset: GPIO32 high");
        delay_us(150_000);
        println!("[rp1-gem] PHY reset: settle delay complete");
        Ok(())
    }

    fn enable_mdio(&self) {
        self.reg_write(NCR, NCR_MPE | NCR_CLRSTAT);
        self.reg_write(NCFGR, NCFGR_MDC_DIV_64 | NCFGR_DRFCS | NCFGR_DBW_128);
        // GEM USRIO selects RGMII on bit 0 and enables its clock on bit 1.
        self.reg_write(USRIO, self.reg_read(USRIO) | 0b11);
    }

    fn find_phy(&mut self, requested: Option<u8>) -> Result<u8, Rp1GemError> {
        let start = requested.unwrap_or(0);
        let end = requested.map_or(31, |phy| phy.min(31));
        for phy in start..=end {
            let id1 = self.mdio_read(phy, PHY_ID1)?;
            let id2 = self.mdio_read(phy, PHY_ID2)?;
            if phy_id_is_valid(id1, id2) {
                return Ok(phy);
            }
        }
        Err(Rp1GemError::NoPhy)
    }

    fn restart_autoneg_and_wait(&mut self, requested_wait_us: u64) -> Result<(), Rp1GemError> {
        let bmcr = self.mdio_read(self.phy_addr, PHY_BMCR)?;
        self.mdio_write(
            self.phy_addr,
            PHY_BMCR,
            bmcr | PHY_BMCR_ANENABLE | PHY_BMCR_ANRESTART,
        )?;
        let wait_us = if requested_wait_us == 0 {
            DEFAULT_LINK_WAIT_US
        } else {
            requested_wait_us
        };
        let attempts = (wait_us / LINK_POLL_INTERVAL_US).max(1);
        for _ in 0..attempts {
            let _ = self.mdio_read(self.phy_addr, PHY_BMSR)?; // BMSR link is latched low.
            let bmsr = self.mdio_read(self.phy_addr, PHY_BMSR)?;
            if (bmsr & (PHY_BMSR_LINK | PHY_BMSR_ANEG_COMPLETE))
                == (PHY_BMSR_LINK | PHY_BMSR_ANEG_COMPLETE)
            {
                let link = self.read_link_state()?;
                self.configure_link(link);
                return Ok(());
            }
            delay_us(LINK_POLL_INTERVAL_US);
        }
        Err(Rp1GemError::LinkTimeout)
    }

    fn read_link_state(&mut self) -> Result<LinkState, Rp1GemError> {
        let _ = self.mdio_read(self.phy_addr, PHY_BMSR)?;
        let bmsr = self.mdio_read(self.phy_addr, PHY_BMSR)?;
        if bmsr & PHY_BMSR_LINK == 0 {
            return Err(Rp1GemError::LinkTimeout);
        }
        let bmcr = self.mdio_read(self.phy_addr, PHY_BMCR)?;
        if bmcr & PHY_BMCR_ANENABLE == 0 {
            return Ok(LinkState {
                up: true,
                speed: decode_forced_speed(bmcr),
                full_duplex: bmcr & PHY_BMCR_FULLDPLX != 0,
            });
        }
        let common = self.mdio_read(self.phy_addr, PHY_ADVERTISE)?
            & self.mdio_read(self.phy_addr, PHY_LPA)?;
        let ctrl1000 = self.mdio_read(self.phy_addr, PHY_CTRL1000)?;
        let stat1000 = self.mdio_read(self.phy_addr, PHY_STAT1000)?;
        Ok(LinkState {
            up: true,
            speed: decode_link_speed(common, ctrl1000, stat1000),
            full_duplex: if ctrl1000 & PHY_CTRL1000_FULL != 0 && stat1000 & PHY_STAT1000_FULL != 0 {
                true
            } else {
                common & (PHY_ADV_10FULL | PHY_ADV_100FULL) != 0
            },
        })
    }

    fn configure_link(&self, link: LinkState) {
        let mut ncfgr = NCFGR_MDC_DIV_64 | NCFGR_DRFCS | NCFGR_DBW_128;
        match link.speed {
            LinkSpeed::Mbps10 => {}
            LinkSpeed::Mbps100 => ncfgr |= NCFGR_SPD_100,
            LinkSpeed::Mbps1000 => ncfgr |= NCFGR_GBE,
        }
        if link.full_duplex {
            ncfgr |= NCFGR_FD;
        }
        self.reg_write(NCFGR, ncfgr);
    }

    fn program_mac_address(&self) {
        let mac = self.config.mac_addr.0;
        self.reg_write(
            SA1B,
            u32::from(mac[0])
                | (u32::from(mac[1]) << 8)
                | (u32::from(mac[2]) << 16)
                | (u32::from(mac[3]) << 24),
        );
        self.reg_write(SA1T, u32::from(mac[4]) | (u32::from(mac[5]) << 8));
    }

    fn program_dma(&mut self) -> Result<(), Rp1GemError> {
        self.reg_write(IDR, u32::MAX);
        self.reg_write(NCR, NCR_MPE);

        let storage = dma_storage();
        for index in 0..RX_RING_LEN {
            let buffer_dma = self.va_to_dma(storage.rx_buffers[index].as_ptr(), DMA_BUF_LEN)?;
            let (lo, hi) = GemDescriptor::rx_addr(buffer_dma, index + 1 == RX_RING_LEN);
            storage.rx_desc[index] = GemDescriptor {
                addr_lo: lo,
                ctrl_status: 0,
                addr_hi: hi,
                reserved: 0,
            };
        }
        for index in 0..TX_RING_LEN {
            storage.tx_desc[index] = GemDescriptor {
                addr_lo: 0,
                ctrl_status: GemDescriptor::tx_ctrl(0, index + 1 == TX_RING_LEN, true),
                addr_hi: 0,
                reserved: 0,
            };
        }

        let rx_ring_dma = self.va_to_dma(
            storage.rx_desc.as_ptr() as *const u8,
            core::mem::size_of_val(&storage.rx_desc),
        )?;
        let tx_ring_dma = self.va_to_dma(
            storage.tx_desc.as_ptr() as *const u8,
            core::mem::size_of_val(&storage.tx_desc),
        )?;
        clean_dcache_range(
            storage.rx_desc.as_ptr() as *const u8,
            core::mem::size_of_val(&storage.rx_desc),
        );
        clean_dcache_range(
            storage.tx_desc.as_ptr() as *const u8,
            core::mem::size_of_val(&storage.tx_desc),
        );
        dma_write_barrier();

        self.reg_write(RBQP, rx_ring_dma as u32);
        self.reg_write(RBQPH, (rx_ring_dma >> 32) as u32);
        self.reg_write(TBQP, tx_ring_dma as u32);
        self.reg_write(TBQPH, (tx_ring_dma >> 32) as u32);
        self.reg_write(
            DMACFG,
            DMACFG_BURST_16
                | DMACFG_RX_FULL_PACKET
                | DMACFG_TX_FULL_PACKET
                | ((DMA_BUF_LEN as u32 / 64) << DMACFG_RX_BUFFER_SHIFT)
                | DMACFG_ADDR64,
        );
        self.reg_write(GEM_AMP, gem_amp_value(8, 8, true));
        self.tx_index = 0;
        self.rx_index = 0;
        Ok(())
    }

    fn enable_rx_tx(&self) {
        self.reg_write(ISR, u32::MAX);
        self.reg_write(NCR, NCR_MPE | NCR_RE | NCR_TE | NCR_CLRSTAT);
        dsb_sy();
    }

    fn send_frame(&mut self, frame: &[u8]) -> Result<(), Rp1GemError> {
        if frame.len() < 14 || frame.len() > MAX_FRAME_LEN || frame.len() > DMA_BUF_LEN {
            return Err(Rp1GemError::TxFrameInvalid);
        }
        let storage = dma_storage();
        let index = self.tx_index;
        invalidate_dcache_range(
            &storage.tx_desc[index] as *const GemDescriptor as *const u8,
            core::mem::size_of::<GemDescriptor>(),
        );
        if storage.tx_desc[index].ctrl_status & TX_DESC_USED == 0 {
            return Err(Rp1GemError::TxTimeout);
        }
        storage.tx_buffer[..frame.len()].copy_from_slice(frame);
        let buffer_dma = self.va_to_dma(storage.tx_buffer.as_ptr(), frame.len())?;
        clean_dcache_range(storage.tx_buffer.as_ptr(), frame.len());
        let wrap = index + 1 == TX_RING_LEN;
        storage.tx_desc[index] = GemDescriptor {
            addr_lo: buffer_dma as u32,
            ctrl_status: GemDescriptor::tx_ctrl(frame.len(), wrap, false),
            addr_hi: (buffer_dma >> 32) as u32,
            reserved: 0,
        };
        clean_dcache_range(
            &storage.tx_desc[index] as *const GemDescriptor as *const u8,
            core::mem::size_of::<GemDescriptor>(),
        );
        dma_write_barrier();
        self.reg_write(NCR, self.reg_read(NCR) | NCR_TSTART);

        for _ in 0..TX_POLL_LIMIT {
            invalidate_dcache_range(
                &storage.tx_desc[index] as *const GemDescriptor as *const u8,
                core::mem::size_of::<GemDescriptor>(),
            );
            if storage.tx_desc[index].ctrl_status & TX_DESC_USED != 0 {
                self.tx_index = (index + 1) % TX_RING_LEN;
                return Ok(());
            }
            spin_loop();
        }
        let _ = self.reg_read(TSR) & TSR_COMPLETE;
        self.recover_tx()?;
        Err(Rp1GemError::TxTimeout)
    }

    fn recover_tx(&mut self) -> Result<(), Rp1GemError> {
        self.program_dma()?;
        self.enable_rx_tx();
        Ok(())
    }

    fn recv_frame(&mut self, buffer: &mut [u8]) -> Result<Option<usize>, Rp1GemError> {
        let storage = dma_storage();
        let index = self.rx_index;
        let desc = &mut storage.rx_desc[index];
        invalidate_dcache_range(
            desc as *const GemDescriptor as *const u8,
            core::mem::size_of::<GemDescriptor>(),
        );
        if desc.addr_lo & RX_DESC_USED == 0 {
            return Ok(None);
        }
        let addr_lo = desc.addr_lo;
        let status = desc.ctrl_status;
        let length = (status & RX_STATUS_LEN_MASK) as usize;
        let result = if status & (RX_STATUS_SOF | RX_STATUS_EOF) != (RX_STATUS_SOF | RX_STATUS_EOF)
            || length < 14
        {
            Err(Rp1GemError::RxDescriptorError { addr_lo, status })
        } else if length > MAX_FRAME_LEN || length > buffer.len() {
            Err(Rp1GemError::RxFrameTooLarge { len: length })
        } else {
            invalidate_dcache_range(storage.rx_buffers[index].as_ptr(), length);
            buffer[..length].copy_from_slice(&storage.rx_buffers[index][..length]);
            Ok(Some(length))
        };
        desc.addr_lo &= !(RX_DESC_USED | RX_DESC_WRAP);
        if index + 1 == RX_RING_LEN {
            desc.addr_lo |= RX_DESC_WRAP;
        }
        desc.ctrl_status = 0;
        clean_dcache_range(
            desc as *const GemDescriptor as *const u8,
            core::mem::size_of::<GemDescriptor>(),
        );
        dma_write_barrier();
        self.rx_index = (index + 1) % RX_RING_LEN;
        result
    }

    fn wait_mdio_idle(&self) -> Result<(), Rp1GemError> {
        for _ in 0..MDIO_POLL_LIMIT {
            if self.reg_read(NSR) & NSR_IDLE != 0 {
                return Ok(());
            }
            spin_loop();
        }
        Err(Rp1GemError::MdioTimeout)
    }

    fn va_to_dma(&self, va: *const u8, len: usize) -> Result<u64, Rp1GemError> {
        let phys = va_to_pa_el2_read(va as u64).ok_or(Rp1GemError::AddressTranslationFailed)?;
        self.config
            .dma_window
            .cpu_phys_to_dma(phys, len as u64)
            .map_err(|_| Rp1GemError::DmaAddressNotCovered)
    }

    fn reg_read(&self, offset: usize) -> u32 {
        read_mmio(self.config.gem_base, GEM_APERTURE_SIZE, offset)
    }

    fn reg_write(&self, offset: usize, value: u32) {
        write_mmio(self.config.gem_base, GEM_APERTURE_SIZE, offset, value)
    }

    fn cfg_read(&self, offset: usize) -> u32 {
        read_mmio(self.config.gem_cfg_base, GEM_CFG_APERTURE_SIZE, offset)
    }

    fn cfg_write(&self, offset: usize, value: u32) {
        write_mmio(
            self.config.gem_cfg_base,
            GEM_CFG_APERTURE_SIZE,
            offset,
            value,
        )
    }
}

#[cfg(target_arch = "aarch64")]
impl EthernetFrameIo for Rp1Gem {
    fn max_frame_len(&self) -> usize {
        MAX_FRAME_LEN
    }

    fn mac_addr(&self) -> MacAddr {
        self.config.mac_addr
    }

    fn try_recv_frame(&mut self, buffer: &mut [u8]) -> Option<usize> {
        match self.recv_frame(buffer) {
            Ok(frame) => frame,
            Err(err) => {
                self.last_error = Some(err);
                None
            }
        }
    }

    fn try_send_frame(&mut self, frame: &[u8]) -> bool {
        match self.send_frame(frame) {
            Ok(()) => true,
            Err(err) => {
                self.last_error = Some(err);
                false
            }
        }
    }
}

#[cfg(target_arch = "aarch64")]
fn dma_storage() -> &'static mut DmaStorage {
    // SAFETY: `TAKEN` permits exactly one `Rp1Gem`; all calls through it are
    // serialized by its exclusive `&mut self`, so the static DMA storage has
    // no simultaneous mutable aliases.
    unsafe { &mut *DMA_STORAGE.0.get() }
}

#[cfg(target_arch = "aarch64")]
fn claim_singleton() -> bool {
    // SAFETY: initialization is performed by the single bootstrap CPU before
    // this polling driver is shared. This flag is only touched here and by
    // `release_singleton` during that serialized initialization interval.
    unsafe {
        let taken = &mut *TAKEN.0.get();
        if *taken {
            false
        } else {
            *taken = true;
            true
        }
    }
}

#[cfg(target_arch = "aarch64")]
fn release_singleton() {
    // SAFETY: this function is called only by the bootstrap CPU after it
    // successfully claimed the singleton and a later initialization step failed.
    unsafe { *TAKEN.0.get() = false }
}

#[cfg(target_arch = "aarch64")]
fn delay_us(us: u64) {
    let mut timer = SystemTimer::new();
    timer.init();
    timer.wait(Duration::from_micros(us));
}

#[cfg(target_arch = "aarch64")]
fn read_mmio(base: usize, size: usize, offset: usize) -> u32 {
    debug_assert!(offset <= size.saturating_sub(core::mem::size_of::<u32>()));
    debug_assert_eq!(offset & 3, 0);
    // SAFETY: base is validated from the RP1 peripheral BAR, offsets are
    // bounded to the documented GEM aperture, and the registers are u32 aligned.
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

#[cfg(target_arch = "aarch64")]
fn write_mmio(base: usize, size: usize, offset: usize, value: u32) {
    debug_assert!(offset <= size.saturating_sub(core::mem::size_of::<u32>()));
    debug_assert_eq!(offset & 3, 0);
    // SAFETY: base is validated from the RP1 peripheral BAR, offsets are
    // bounded to the documented GEM aperture, and the registers are u32 aligned.
    unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) }
}

#[cfg(target_arch = "aarch64")]
fn dma_write_barrier() {
    // SAFETY: dmb oshst orders cache-cleaned descriptor and buffer writes before
    // the following MMIO ownership/queue-base publication to the RP1 DMA master.
    unsafe { asm!("dmb oshst", options(nostack, preserves_flags)) }
}

#[cfg(target_arch = "aarch64")]
const fn mdio_command(op: u32, phy: u8, reg: u8, value: u16) -> u32 {
    (0b01 << 30)
        | ((op & 0b11) << 28)
        | ((phy as u32 & 0x1f) << 23)
        | ((reg as u32 & 0x1f) << 18)
        | (0b10 << 16)
        | value as u32
}

const fn gem_amp_value(ar2r_max_pipe: u8, aw2w_max_pipe: u8, aw2b_fill: bool) -> u32 {
    (ar2r_max_pipe as u32) | ((aw2w_max_pipe as u32) << 8) | ((aw2b_fill as u32) << 16)
}

// Readback of control bits only: this is not an AXI/PCIe drain test.
fn poll_ncr_stopped(mut read: impl FnMut() -> u32) -> Result<u32, u32> {
    let mut ncr = u32::MAX;
    for _ in 0..QUIESCE_POLL_LIMIT {
        ncr = read();
        if ncr & QUIESCE_NCR_MASK == 0 {
            return Ok(ncr);
        }
        spin_loop();
    }
    Err(ncr)
}

fn is_zero_mac(mac: MacAddr) -> bool {
    mac.0 == [0; 6]
}

const fn phy_id_is_valid(id1: u16, id2: u16) -> bool {
    !(id1 == 0 || id1 == u16::MAX || id2 == 0 || id2 == u16::MAX)
}

const fn decode_forced_speed(bmcr: u16) -> LinkSpeed {
    if bmcr & PHY_BMCR_SPEED1000 != 0 {
        LinkSpeed::Mbps1000
    } else if bmcr & PHY_BMCR_SPEED100 != 0 {
        LinkSpeed::Mbps100
    } else {
        LinkSpeed::Mbps10
    }
}

const fn decode_link_speed(common: u16, ctrl1000: u16, stat1000: u16) -> LinkSpeed {
    if (ctrl1000 & PHY_CTRL1000_FULL != 0 && stat1000 & PHY_STAT1000_FULL != 0)
        || (ctrl1000 & PHY_CTRL1000_HALF != 0 && stat1000 & PHY_STAT1000_HALF != 0)
    {
        LinkSpeed::Mbps1000
    } else if common & (PHY_ADV_100FULL | PHY_ADV_100HALF) != 0 {
        LinkSpeed::Mbps100
    } else {
        LinkSpeed::Mbps10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_ncr_is_required_with_bounded_polling() {
        for ready_at in [1, 2, QUIESCE_POLL_LIMIT] {
            let mut reads = 0;
            let result = poll_ncr_stopped(|| {
                reads += 1;
                if reads == ready_at {
                    NCR_CLRSTAT
                } else {
                    QUIESCE_NCR_MASK
                }
            });
            assert_eq!(result, Ok(NCR_CLRSTAT));
            assert_eq!(reads, ready_at);
        }
        for held in [NCR_RE, NCR_TE, NCR_TSTART, NCR_MPE, u32::MAX] {
            let mut reads = 0;
            assert_eq!(
                poll_ncr_stopped(|| {
                    reads += 1;
                    held
                }),
                Err(held)
            );
            assert_eq!(reads, QUIESCE_POLL_LIMIT);
        }
    }

    #[test]
    fn rx_descriptor_ownership_and_wrap_encoding() {
        let (lo, hi) = GemDescriptor::rx_addr(0x1234_5678_9abc_def0, true);
        assert_eq!(lo, 0x9abc_def2);
        assert_eq!(hi, 0x1234_5678);
        assert_eq!(lo & RX_DESC_USED, 0);
    }

    #[test]
    fn tx_descriptor_encoding() {
        let ctrl = GemDescriptor::tx_ctrl(1518, true, true);
        assert_eq!(ctrl & TX_DESC_LEN_MASK, 1518);
        assert_ne!(ctrl & TX_DESC_LAST, 0);
        assert_ne!(ctrl & TX_DESC_WRAP, 0);
        assert_ne!(ctrl & TX_DESC_USED, 0);
    }

    #[test]
    fn dma_address_split_join_is_64_bit() {
        let dma = 0x0010_1234_5678_9000u64;
        let (lo, hi) = GemDescriptor::rx_addr(dma, false);
        assert_eq!((u64::from(hi) << 32) | u64::from(lo & !RX_DESC_WRAP), dma);
    }

    #[test]
    fn zero_mac_is_rejected() {
        assert!(is_zero_mac(MacAddr([0; 6])));
        assert!(!is_zero_mac(MacAddr([2, 0, 0, 0, 0, 5])));
    }

    #[test]
    fn invalid_phy_ids_are_rejected() {
        assert!(!phy_id_is_valid(0, 1));
        assert!(!phy_id_is_valid(1, u16::MAX));
        assert!(phy_id_is_valid(0x2000, 0x5c90));
    }

    #[test]
    fn link_speed_decoding_prefers_gigabit() {
        assert_eq!(
            decode_link_speed(PHY_ADV_100FULL, PHY_CTRL1000_FULL, PHY_STAT1000_FULL),
            LinkSpeed::Mbps1000
        );
        assert_eq!(decode_link_speed(PHY_ADV_100FULL, 0, 0), LinkSpeed::Mbps100);
        assert_eq!(decode_link_speed(PHY_ADV_10FULL, 0, 0), LinkSpeed::Mbps10);
    }

    #[test]
    fn gem_amp_uses_linux_dt_values() {
        assert_eq!(gem_amp_value(8, 8, true), 0x0001_0808);
    }
}
