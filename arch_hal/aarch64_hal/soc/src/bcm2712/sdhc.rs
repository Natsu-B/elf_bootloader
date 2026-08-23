use core::cell::SyncUnsafeCell;
use core::mem::MaybeUninit;
use core::mem::size_of;
use core::sync::atomic::Ordering;
use core::time::Duration;

#[cfg(target_arch = "aarch64")]
use super::pirq_hook;
use dtb::DtbParser;
use dtb::WalkError;
use io_api::block_device::BlockDevice;
use io_api::block_device::IoError;
use io_api::block_device::Lba;
use mutex::SpinLock;
use mutex::pod::RawAtomicPod;
use print::println;

#[cfg(test)]
extern crate std;

#[cfg(target_arch = "aarch64")]
use timer::SystemTimer;
#[cfg(target_arch = "aarch64")]
use timer::read_counter;

#[cfg(not(target_arch = "aarch64"))]
use self::timer_compat::SystemTimer;
#[cfg(not(target_arch = "aarch64"))]
use self::timer_compat::read_counter;

#[cfg(not(target_arch = "aarch64"))]
mod timer_compat {
    use core::num::NonZeroU64;
    use core::sync::atomic::AtomicU64;
    use core::sync::atomic::Ordering;
    use core::time::Duration;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub struct SystemTimer {
        counter_frequency: Option<NonZeroU64>,
    }

    impl SystemTimer {
        pub const fn new() -> Self {
            Self {
                counter_frequency: None,
            }
        }

        pub fn init(&mut self) {
            self.counter_frequency = NonZeroU64::new(1_000_000);
        }

        pub fn counter_frequency_hz(&self) -> NonZeroU64 {
            self.counter_frequency
                .expect("before calling wait function call init")
        }

        pub fn wait(&self, duration: Duration) {
            let ticks = duration.as_micros() as u64;
            COUNTER.fetch_add(ticks.max(1), Ordering::Relaxed);
        }
    }

    pub fn read_counter() -> u64 {
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }
}

const DT_COMPAT_BCM2712_SDHCI: &str = "brcm,bcm2712-sdhci";
const DT_COMPAT_BRCMSTB_SDHCI: &str = "brcm,sdhci-brcmstb";
const SDHC_DEFAULT_MIN_CLOCK_HZ: u32 = 400_000;
const SDHC_DEFAULT_MAX_CLOCK_HZ: u32 = 100_000_000;
const SDHC_CONSERVATIVE_TRANSFER_CLOCK_HZ: u32 = 12_500_000;
const SDHC_BLOCK_SIZE: usize = 512;
const SDHC_MAX_TRANSFER_BLOCKS_PER_CMD: usize = 1024;
const SDHC_CMD_TIMEOUT_US: u64 = 100_000;
const SDHC_DATA_TIMEOUT_US: u64 = 1_000_000;
const SDHC_CLOCK_TIMEOUT_US: u64 = 20_000;
#[cfg(target_arch = "aarch64")]
const BCM2712_SDIO1_PREINIT_DELAY_US: u64 = 100;
const SDHC_RESET_TIMEOUT_US: u64 = 100_000;
const SDHC_ACMD41_RETRY_MAX: usize = 1000;
const SDHC_ACMD41_POLL_DELAY_US: u64 = 10;
const SDHC_POST_CLOCK_SETTLE_DELAY_US: u64 = 50;
const SDHC_CARD_DETECT_SETTLE_DELAY_US: u64 = 50;
const SDHC_MAX_DT_CANDIDATES: usize = 8;
const SDHC_MAX_REG_ENTRIES: usize = 8;
const SDHC_MAX_CLOCK_DIVIDER: u16 = 0x03ff;
const SDHC_3V3_MICROVOLTS: u32 = 3_300_000;
const BCM2712_SD_SLOT_HOST_BASE: usize = 0x10_00ff_f000;

const SDHCI_BLOCK_SIZE: usize = 0x04;
const SDHCI_BLOCK_COUNT: usize = 0x06;
const SDHCI_ARGUMENT: usize = 0x08;
const SDHCI_TRANSFER_MODE: usize = 0x0c;
const SDHCI_COMMAND: usize = 0x0e;
const SDHCI_RESPONSE: usize = 0x10;
const SDHCI_BUFFER: usize = 0x20;
const SDHCI_PRESENT_STATE: usize = 0x24;
const SDHCI_HOST_CONTROL: usize = 0x28;
const SDHCI_POWER_CONTROL: usize = 0x29;
const SDHCI_CLOCK_CONTROL: usize = 0x2c;
const SDHCI_TIMEOUT_CONTROL: usize = 0x2e;
const SDHCI_SOFTWARE_RESET: usize = 0x2f;
const SDHCI_INT_STATUS: usize = 0x30;
const SDHCI_INT_ENABLE: usize = 0x34;
const SDHCI_SIGNAL_ENABLE: usize = 0x38;

const SDHCI_MAKE_BLKSZ: u16 = 0x7000;
const SDHCI_TRNS_BLK_CNT_EN: u16 = 1 << 1;
const SDHCI_TRNS_READ: u16 = 1 << 4;
const SDHCI_TRNS_MULTI: u16 = 1 << 5;
const SDHCI_CMD_RESP_NONE: u16 = 0x00;
const SDHCI_CMD_RESP_LONG: u16 = 0x01;
const SDHCI_CMD_RESP_SHORT: u16 = 0x02;
const SDHCI_CMD_RESP_SHORT_BUSY: u16 = 0x03;
const SDHCI_CMD_CRC: u16 = 0x08;
const SDHCI_CMD_INDEX: u16 = 0x10;
const SDHCI_CMD_DATA: u16 = 0x20;
const SDHCI_CMD_INHIBIT: u32 = 1 << 0;
const SDHCI_DATA_INHIBIT: u32 = 1 << 1;
const SDHCI_DOING_WRITE: u32 = 1 << 8;
const SDHCI_DOING_READ: u32 = 1 << 9;
const SDHCI_SPACE_AVAILABLE: u32 = 1 << 10;
const SDHCI_DATA_AVAILABLE: u32 = 1 << 11;
const SDHCI_CARD_PRESENT: u32 = 1 << 16;
const SDHCI_CARD_STATE_STABLE: u32 = 1 << 17;
const SDHCI_WRITE_PROTECT: u32 = 1 << 19;
const SDHCI_CTRL_4BITBUS: u8 = 0x02;
const SDHCI_CTRL_8BITBUS: u8 = 0x20;
const SDHCI_POWER_ON: u8 = 0x01;
const SDHCI_POWER_330: u8 = 0x0e;
const SDHCI_CLOCK_CARD_EN: u16 = 1 << 2;
const SDHCI_CLOCK_INT_STABLE: u16 = 1 << 1;
const SDHCI_CLOCK_INT_EN: u16 = 1 << 0;
const SDHCI_DIVIDER_SHIFT: u16 = 8;
const SDHCI_DIVIDER_HI_SHIFT: u16 = 6;
const SDHCI_DIV_MASK: u16 = 0x00ff;
const SDHCI_DIV_HI_MASK: u16 = 0x0300;
const SDHCI_RESET_ALL: u8 = 0x01;
const SDHCI_RESET_CMD: u8 = 0x02;
const SDHCI_RESET_DATA: u8 = 0x04;
const SDHCI_INT_RESPONSE: u32 = 1 << 0;
const SDHCI_INT_DATA_END: u32 = 1 << 1;
const SDHCI_INT_DMA_END: u32 = 1 << 3;
const SDHCI_INT_SPACE_AVAIL: u32 = 1 << 4;
const SDHCI_INT_DATA_AVAIL: u32 = 1 << 5;
const SDHCI_INT_ERROR: u32 = 1 << 15;
const SDHCI_INT_TIMEOUT: u32 = 1 << 16;
const SDHCI_INT_DATA_TIMEOUT: u32 = 1 << 20;
const SDHCI_INT_CMD_MASK: u32 =
    SDHCI_INT_RESPONSE | SDHCI_INT_TIMEOUT | (1 << 17) | (1 << 18) | (1 << 19);
const SDHCI_INT_DATA_MASK: u32 = SDHCI_INT_DATA_END
    | SDHCI_INT_DMA_END
    | SDHCI_INT_SPACE_AVAIL
    | SDHCI_INT_DATA_AVAIL
    | SDHCI_INT_DATA_TIMEOUT
    | (1 << 21)
    | (1 << 22)
    | (1 << 25);
const SDHCI_INT_ALL_MASK: u32 = u32::MAX;

const SDIO_CFG_CTRL: usize = 0x00;
const SDIO_CFG_CTRL_SDCD_N_TEST_EN: u32 = 1 << 31;
const SDIO_CFG_CTRL_SDCD_N_TEST_LEV: u32 = 1 << 30;
const SDIO_CFG_SD_PIN_SEL: usize = 0x44;
const SDIO_CFG_SD_PIN_SEL_MASK: u32 = 0x3;
const SDIO_CFG_SD_PIN_SEL_CARD: u32 = 1 << 1;
const SDIO_CFG_CQ_CAPABILITY: usize = 0x4c;
const SDIO_CFG_CQ_CAPABILITY_BASE_CLOCK_MASK: u32 = 0x00ff;
const SDIO_CFG_CQ_CAPABILITY_FMUL_MASK: u32 = 0b11 << 12;
const SDIO_CFG_CQ_CAPABILITY_FMUL_DEFAULT: u32 = SDIO_CFG_CQ_CAPABILITY_FMUL_MASK;
const OF_GPIO_ACTIVE_LOW: u32 = 1 << 0;
const BRCMSTB_GIO_BANK_SIZE: usize = 0x20;
#[cfg(target_arch = "aarch64")]
const BRCMSTB_GIO_ODEN: usize = 0x00;
const BRCMSTB_GIO_DATA: usize = 0x04;
#[cfg(target_arch = "aarch64")]
const BRCMSTB_GIO_IODIR: usize = 0x08;

#[cfg(any(test, target_arch = "aarch64"))]
const RP1_GPIO_CTRL_REG_OFFSET: usize = 0x0004;
#[cfg(any(test, target_arch = "aarch64"))]
const RP1_GPIO_CTRL_STRIDE: usize = size_of::<u32>() * 2;
#[cfg(target_arch = "aarch64")]
const RP1_GPIO_CTRL_FUNCSEL_MASK: u32 = 0x1f;
#[cfg(target_arch = "aarch64")]
const RP1_GPIO_CTRL_OUTOVER_MASK: u32 = 0b11 << 12;
#[cfg(target_arch = "aarch64")]
const RP1_GPIO_CTRL_OEOVER_MASK: u32 = 0b11 << 14;
#[cfg(target_arch = "aarch64")]
const RP1_GPIO_FUNCSEL_ALT0: u32 = 0x00;
#[cfg(any(test, target_arch = "aarch64"))]
const RP1_PAD_SLEWFAST: u32 = 1 << 0;
#[cfg(any(test, target_arch = "aarch64"))]
const RP1_PAD_SCHMITT: u32 = 1 << 1;
#[cfg(any(test, target_arch = "aarch64"))]
const RP1_PAD_PULL_SHIFT: u32 = 2;
#[cfg(target_arch = "aarch64")]
const RP1_PAD_PULL_MASK: u32 = 0b11 << RP1_PAD_PULL_SHIFT;
#[cfg(any(test, target_arch = "aarch64"))]
const RP1_PAD_PULL_NONE: u32 = 0 << RP1_PAD_PULL_SHIFT;
#[cfg(any(test, target_arch = "aarch64"))]
const RP1_PAD_PULL_UP: u32 = 2 << RP1_PAD_PULL_SHIFT;
#[cfg(any(test, target_arch = "aarch64"))]
const RP1_PAD_DRIVE_SHIFT: u32 = 4;
#[cfg(target_arch = "aarch64")]
const RP1_PAD_DRIVE_MASK: u32 = 0b11 << RP1_PAD_DRIVE_SHIFT;
#[cfg(any(test, target_arch = "aarch64"))]
const RP1_PAD_DRIVE_12MA: u32 = 0b11 << RP1_PAD_DRIVE_SHIFT;
#[cfg(any(test, target_arch = "aarch64"))]
const RP1_PAD_INPUT_ENABLE: u32 = 1 << 6;
#[cfg(target_arch = "aarch64")]
const RP1_PAD_OUTPUT_DISABLE: u32 = 1 << 7;
#[cfg(target_arch = "aarch64")]
const RP1_PAD_CONFIG_MASK: u32 = RP1_PAD_SLEWFAST
    | RP1_PAD_SCHMITT
    | RP1_PAD_PULL_MASK
    | RP1_PAD_DRIVE_MASK
    | RP1_PAD_INPUT_ENABLE
    | RP1_PAD_OUTPUT_DISABLE;
#[cfg(target_arch = "aarch64")]
const RP1_SDIO1_PINS: [usize; 6] = [28, 29, 30, 31, 32, 33];

#[cfg(any(test, target_arch = "aarch64"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rp1IoBankDesc {
    min_gpio: usize,
    num_gpios: usize,
    gpio_offset: usize,
    pads_offset: usize,
}

#[cfg(any(test, target_arch = "aarch64"))]
const RP1_IO_BANKS: [Rp1IoBankDesc; 3] = [
    Rp1IoBankDesc {
        min_gpio: 0,
        num_gpios: 28,
        gpio_offset: 0x0000,
        pads_offset: 0x0004,
    },
    Rp1IoBankDesc {
        min_gpio: 28,
        num_gpios: 6,
        gpio_offset: 0x4000,
        pads_offset: 0x4004,
    },
    Rp1IoBankDesc {
        min_gpio: 34,
        num_gpios: 20,
        gpio_offset: 0x8000,
        pads_offset: 0x8004,
    },
];

#[cfg(any(test, target_arch = "aarch64"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rp1PinPrep {
    ctrl_offset: usize,
    pad_offset: usize,
    pad_value: u32,
}

const MMC_CMD_GO_IDLE_STATE: u8 = 0;
const MMC_CMD_ALL_SEND_CID: u8 = 2;
const MMC_CMD_SET_RELATIVE_ADDR: u8 = 3;
const MMC_CMD_SELECT_CARD: u8 = 7;
const MMC_CMD_SEND_IF_COND: u8 = 8;
const MMC_CMD_SEND_CSD: u8 = 9;
const MMC_CMD_SEND_STATUS: u8 = 13;
const MMC_CMD_STOP_TRANSMISSION: u8 = 12;
const MMC_CMD_SET_BLOCKLEN: u8 = 16;
const MMC_CMD_READ_SINGLE_BLOCK: u8 = 17;
const MMC_CMD_WRITE_SINGLE_BLOCK: u8 = 24;
const MMC_CMD_APP_CMD: u8 = 55;
const SD_CMD_APP_SET_BUS_WIDTH: u8 = 6;
const SD_CMD_APP_SEND_OP_COND: u8 = 41;
const SD_BUS_WIDTH_1BIT_ARG: u32 = 0;
const SD_BUS_WIDTH_4BIT_ARG: u32 = 2;

const OCR_BUSY: u32 = 1 << 31;
const OCR_HCS: u32 = 1 << 30;
const OCR_3V2_3V4: u32 = 0x0030_0000;
const OCR_3V3_3V4: u32 = 0x0020_0000;
const OCR_3V2_3V3: u32 = 0x0010_0000;
const OCR_REQUEST: u32 = OCR_HCS | OCR_3V2_3V4 | OCR_3V3_3V4 | OCR_3V2_3V3;
const SD_STATUS_READY_FOR_DATA: u32 = 1 << 8;
const SD_STATUS_CURRENT_STATE_SHIFT: u32 = 9;
const SD_STATUS_CURRENT_STATE_MASK: u32 = 0x0f << SD_STATUS_CURRENT_STATE_SHIFT;
const SD_STATUS_TRANSFER_STATE: u8 = 4;
const SDHC_CARD_STATUS_TIMEOUT_US: u64 = 1_000_000;
const SDHC_CARD_STATUS_POLL_DELAY_US: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdhcError {
    DtbParse(&'static str),
    DtbNotFound,
    DtbInvalid(&'static str),
    InvalidParam,
    InvalidClock,
    InvalidState,
    Timeout,
    NotReady,
    NoCard,
    ReadOnly,
    Unsupported,
    Io,
    OutOfRange,
    Align,
}

impl From<SdhcError> for IoError {
    fn from(value: SdhcError) -> Self {
        match value {
            SdhcError::InvalidParam | SdhcError::InvalidClock | SdhcError::DtbInvalid(_) => {
                IoError::InvalidParam
            }
            SdhcError::Timeout => IoError::Timeout,
            SdhcError::NotReady => IoError::NotReady,
            SdhcError::NoCard => IoError::NotReady,
            SdhcError::ReadOnly => IoError::ReadOnly,
            SdhcError::Unsupported => IoError::Unsupported,
            SdhcError::OutOfRange => IoError::OutOfRange,
            SdhcError::Align => IoError::Align,
            SdhcError::Io => IoError::Io,
            SdhcError::DtbParse(_) | SdhcError::DtbNotFound | SdhcError::InvalidState => {
                IoError::NotReady
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HostRegion {
    base: usize,
    size: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GpioLineConfig {
    base: usize,
    pin: u8,
    active_low: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GpioOutputConfig {
    line: GpioLineConfig,
    logical_high: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SdhcPowerConfig {
    vqmmc_select: Option<GpioOutputConfig>,
    vqmmc_settle_us: u32,
    vmmc_enable: Option<GpioOutputConfig>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SdhcConfig {
    host: HostRegion,
    cfg: HostRegion,
    max_clock_hz: u32,
    power: SdhcPowerConfig,
    card_detect: Option<GpioLineConfig>,
    bus_width: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct NamedRegions {
    host: Option<HostRegion>,
    cfg: Option<HostRegion>,
    busisol: Option<HostRegion>,
    lcpll: Option<HostRegion>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SdhcRegName {
    Host,
    Cfg,
    BusIsol,
    LcPll,
    #[default]
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SdhcDtCandidate {
    host: Option<HostRegion>,
    cfg: Option<HostRegion>,
    busisol: Option<HostRegion>,
    lcpll: Option<HostRegion>,
    max_clock_hz: u32,
    power: SdhcPowerConfig,
    card_detect: Option<GpioLineConfig>,
    broken_cd: bool,
    has_brcmstb_compat: bool,
    status_okay: bool,
    bus_width: u32,
    non_removable: bool,
    has_wifi_child: bool,
}

impl SdhcDtCandidate {
    fn is_supported_shape(&self) -> bool {
        self.status_okay && self.host.is_some() && self.cfg.is_some()
    }

    fn host_base(&self) -> Option<usize> {
        self.host.map(|region| region.base)
    }

    fn is_viable_sd_slot(&self) -> bool {
        self.host_base() == Some(BCM2712_SD_SLOT_HOST_BASE)
            || (!self.non_removable && !self.has_wifi_child)
    }

    fn into_config(self) -> Result<SdhcConfig, SdhcError> {
        let host = self
            .host
            .ok_or(SdhcError::DtbInvalid("sdhc: missing host reg"))?;
        let cfg = self
            .cfg
            .ok_or(SdhcError::DtbInvalid("sdhc: missing cfg reg"))?;
        let runtime_bus_width = resolve_sd_slot_bus_width(self.bus_width, self.broken_cd)?;
        if self.card_detect.is_none() && !self.broken_cd {
            return Err(SdhcError::DtbInvalid("sdhc: missing cd-gpios for SD slot"));
        }
        Ok(SdhcConfig {
            host,
            cfg,
            max_clock_hz: self.max_clock_hz,
            power: self.power,
            card_detect: self.card_detect,
            bus_width: runtime_bus_width,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct CardState {
    high_capacity: bool,
    capacity_blocks: u64,
    read_only: bool,
}

pub struct Bcm2712Sdhc {
    host: *mut u8,
    cfg: *mut u8,
    max_clock_hz: u32,
    min_clock_hz: u32,
    card_detect: Option<GpioLineConfig>,
    bus_width: u32,
    card: SpinLock<Option<CardState>>,
}

// SAFETY: The controller is accessed through volatile MMIO and serialized state transitions.
unsafe impl Send for Bcm2712Sdhc {}
// SAFETY: Methods synchronize mutable shared state with `SpinLock`.
unsafe impl Sync for Bcm2712Sdhc {}

// SAFETY: `bool` has no invalid bit patterns; `false` is canonical for `RawAtomicPod<bool>`.
static TAKEN: RawAtomicPod<bool> = unsafe { RawAtomicPod::new_raw_unchecked(false) };
// SAFETY: `bool` has no invalid bit patterns; `false` is canonical for `RawAtomicPod<bool>`.
static READY: RawAtomicPod<bool> = unsafe { RawAtomicPod::new_raw_unchecked(false) };
static STATE: SyncUnsafeCell<MaybeUninit<Bcm2712Sdhc>> = SyncUnsafeCell::new(MaybeUninit::uninit());

fn mmio_read_u32(base: *mut u8, offset: usize) -> u32 {
    // SAFETY: The caller provides a valid MMIO base, and `offset` targets a 32-bit register in
    // that mapped window.
    unsafe { core::ptr::read_volatile(base.wrapping_add(offset) as *const u32) }
}

fn mmio_write_u32(base: *mut u8, offset: usize, value: u32) {
    // SAFETY: The caller provides a valid MMIO base, and `offset` targets a 32-bit register in
    // that mapped window.
    unsafe { core::ptr::write_volatile(base.wrapping_add(offset) as *mut u32, value) };
}

fn log_init_stage(stage: &str, err: SdhcError) -> SdhcError {
    if cfg!(debug_assertions) {
        println!("sdhc: init stage {} failed: {:?}", stage, err);
    }
    err
}

fn brcmstb_gpio_bank(pin: u8) -> (usize, u32) {
    let bank = usize::from(pin / 32);
    let bit = 1u32 << u32::from(pin % 32);
    (bank, bit)
}

fn brcmstb_gpio_reg_offset(bank: usize, reg: usize) -> Result<usize, SdhcError> {
    bank.checked_mul(BRCMSTB_GIO_BANK_SIZE)
        .and_then(|base| base.checked_add(reg))
        .ok_or(SdhcError::InvalidParam)
}

fn read_brcmstb_gpio_input(config: GpioLineConfig) -> Result<bool, SdhcError> {
    if config.base == 0 {
        return Err(SdhcError::InvalidParam);
    }

    let base = config.base as *mut u8;
    let (bank, bit) = brcmstb_gpio_bank(config.pin);
    let data_offset = brcmstb_gpio_reg_offset(bank, BRCMSTB_GIO_DATA)?;
    let raw_high = (mmio_read_u32(base, data_offset) & bit) != 0;
    Ok(if config.active_low {
        !raw_high
    } else {
        raw_high
    })
}

#[cfg(target_arch = "aarch64")]
fn configure_brcmstb_gpio_output(config: GpioOutputConfig) -> Result<(), SdhcError> {
    let base = config.line.base as *mut u8;
    let (bank, bit) = brcmstb_gpio_bank(config.line.pin);
    let data_offset = brcmstb_gpio_reg_offset(bank, BRCMSTB_GIO_DATA)?;
    let oden_offset = brcmstb_gpio_reg_offset(bank, BRCMSTB_GIO_ODEN)?;
    let iodir_offset = brcmstb_gpio_reg_offset(bank, BRCMSTB_GIO_IODIR)?;
    let raw_high = if config.line.active_low {
        !config.logical_high
    } else {
        config.logical_high
    };

    let mut data = mmio_read_u32(base, data_offset);
    if raw_high {
        data |= bit;
    } else {
        data &= !bit;
    }
    mmio_write_u32(base, data_offset, data);

    let oden = mmio_read_u32(base, oden_offset) & !bit;
    mmio_write_u32(base, oden_offset, oden);

    let iodir = mmio_read_u32(base, iodir_offset) & !bit;
    mmio_write_u32(base, iodir_offset, iodir);
    Ok(())
}

fn wait_micros(delay_us: u64) {
    if delay_us == 0 {
        return;
    }
    let mut timer = SystemTimer::new();
    timer.init();
    timer.wait(Duration::from_micros(delay_us));
}

#[cfg(any(test, target_arch = "aarch64"))]
fn rp1_io_bank(pin: usize) -> Option<(Rp1IoBankDesc, usize)> {
    RP1_IO_BANKS.iter().copied().find_map(|bank| {
        let end = bank.min_gpio.checked_add(bank.num_gpios)?;
        (bank.min_gpio..end)
            .contains(&pin)
            .then_some((bank, pin - bank.min_gpio))
    })
}

#[cfg(any(test, target_arch = "aarch64"))]
fn rp1_sdio1_pad_value(pin: usize) -> Option<u32> {
    let pull = match pin {
        28 => RP1_PAD_PULL_NONE,
        29..=33 => RP1_PAD_PULL_UP,
        _ => return None,
    };
    Some(RP1_PAD_SLEWFAST | RP1_PAD_SCHMITT | RP1_PAD_DRIVE_12MA | RP1_PAD_INPUT_ENABLE | pull)
}

#[cfg(any(test, target_arch = "aarch64"))]
fn rp1_sdio1_pin_prep(pin: usize) -> Option<Rp1PinPrep> {
    let (bank, pin_offset) = rp1_io_bank(pin)?;
    Some(Rp1PinPrep {
        ctrl_offset: bank
            .gpio_offset
            .checked_add(pin_offset.checked_mul(RP1_GPIO_CTRL_STRIDE)?)?
            .checked_add(RP1_GPIO_CTRL_REG_OFFSET)?,
        pad_offset: bank
            .pads_offset
            .checked_add(pin_offset.checked_mul(size_of::<u32>())?)?,
        pad_value: rp1_sdio1_pad_value(pin)?,
    })
}

fn configure_bcm2712_sdio1_clock_regs(cfg: *mut u8, max_clock_hz: u32) -> Result<(), SdhcError> {
    // The Raspberry Pi 5 SD-slot path exposes its base host clock through the DT clock
    // provider, and the standard SDHCI clock-control register programs the actual card clock.
    // The STB-specific local-clock-running handshake does not assert on this path, so we must
    // not gate bring-up on those CFG status bits.
    let mut reg = mmio_read_u32(cfg, SDIO_CFG_CQ_CAPABILITY);
    reg &= !(SDIO_CFG_CQ_CAPABILITY_FMUL_MASK | SDIO_CFG_CQ_CAPABILITY_BASE_CLOCK_MASK);
    reg |= bcm2712_cq_capability_value(max_clock_hz);
    mmio_write_u32(cfg, SDIO_CFG_CQ_CAPABILITY, reg);
    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn configure_bcm2712_sd_power(power: &SdhcPowerConfig) -> Result<(), SdhcError> {
    if let Some(vqmmc_select) = power.vqmmc_select {
        configure_brcmstb_gpio_output(vqmmc_select)?;
    }
    if let Some(vmmc_enable) = power.vmmc_enable {
        configure_brcmstb_gpio_output(vmmc_enable)?;
    }
    wait_micros(u64::from(power.vqmmc_settle_us));
    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn bcm2712_sdio1_preinit(cfg: &SdhcConfig) -> Result<(), SdhcError> {
    let rp1_base = pirq_hook::rp1_peripheral_base()
        .ok_or(SdhcError::InvalidState)
        .map_err(|err| log_init_stage("preinit-rp1-base", err))? as *mut u8;
    configure_bcm2712_sd_power(&cfg.power).map_err(|err| log_init_stage("preinit-power", err))?;
    configure_bcm2712_sdio1_clock_regs(cfg.cfg.base as *mut u8, cfg.max_clock_hz)?;

    for pin in RP1_SDIO1_PINS {
        let prep = rp1_sdio1_pin_prep(pin).ok_or(SdhcError::InvalidParam)?;
        let ctrl = mmio_read_u32(rp1_base, prep.ctrl_offset)
            & !(RP1_GPIO_CTRL_FUNCSEL_MASK
                | RP1_GPIO_CTRL_OUTOVER_MASK
                | RP1_GPIO_CTRL_OEOVER_MASK);
        mmio_write_u32(rp1_base, prep.ctrl_offset, ctrl | RP1_GPIO_FUNCSEL_ALT0);

        let pad = mmio_read_u32(rp1_base, prep.pad_offset) & !RP1_PAD_CONFIG_MASK;
        mmio_write_u32(rp1_base, prep.pad_offset, pad | prep.pad_value);
    }

    wait_micros(BCM2712_SDIO1_PREINIT_DELAY_US);
    Ok(())
}

#[cfg(not(target_arch = "aarch64"))]
fn bcm2712_sdio1_preinit(_cfg: &SdhcConfig) -> Result<(), SdhcError> {
    Ok(())
}

impl Bcm2712Sdhc {
    pub fn init_from_dtb(dtb: &DtbParser) -> Result<&'static Self, SdhcError> {
        if TAKEN
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SdhcError::InvalidState);
        }
        let result = Self::init_from_dtb_inner(dtb);
        if result.is_err() {
            READY.store(false, Ordering::Release);
            TAKEN.store(false, Ordering::Release);
        }
        result
    }

    fn init_from_dtb_inner(dtb: &DtbParser) -> Result<&'static Self, SdhcError> {
        let cfg = parse_from_dtb(dtb)?;
        if cfg.host.base == 0 || cfg.host.size < 0x100 {
            return Err(SdhcError::DtbInvalid("sdhc: invalid host region"));
        }
        if cfg.cfg.base == 0 || cfg.cfg.size < 0x48 {
            return Err(SdhcError::DtbInvalid("sdhc: invalid cfg region"));
        }
        if cfg.max_clock_hz == 0 {
            return Err(SdhcError::InvalidClock);
        }
        bcm2712_sdio1_preinit(&cfg).map_err(|err| log_init_stage("preinit", err))?;
        let instance = Bcm2712Sdhc {
            host: cfg.host.base as *mut u8,
            cfg: cfg.cfg.base as *mut u8,
            max_clock_hz: cfg.max_clock_hz.min(SDHC_DEFAULT_MAX_CLOCK_HZ),
            min_clock_hz: SDHC_DEFAULT_MIN_CLOCK_HZ,
            card_detect: cfg.card_detect,
            bus_width: cfg.bus_width,
            card: SpinLock::new(None),
        };
        instance
            .reset(SDHCI_RESET_ALL)
            .map_err(|err| log_init_stage("reset", err))?;
        instance
            .configure_card_detect()
            .map_err(|err| log_init_stage("card-detect", err))?;
        instance
            .power_on()
            .map_err(|err| log_init_stage("power-on", err))?;
        instance
            .set_clock(instance.min_clock_hz)
            .map_err(|err| log_init_stage("set-clock", err))?;
        instance.enable_interrupt_masks();
        instance
            .card_init_sequence()
            .map_err(|err| log_init_stage("card-init", err))?;

        // SAFETY: guarded by `TAKEN`; this is one-time initialization.
        unsafe {
            (*STATE.get()).write(instance);
        }
        READY.store(true, Ordering::Release);
        // SAFETY: `READY` is published only after full initialization.
        let state = unsafe { (&*STATE.get()).assume_init_ref() };
        Ok(state)
    }

    fn now_ticks() -> u64 {
        read_counter()
    }

    fn elapsed_us(timer: &SystemTimer, start: u64) -> u64 {
        let now = Self::now_ticks();
        let ticks = now.wrapping_sub(start) as u128;
        let hz = timer.counter_frequency_hz().get() as u128;
        ((ticks * 1_000_000u128) / hz) as u64
    }

    fn wait_until_static(
        timeout_us: u64,
        mut condition: impl FnMut() -> bool,
    ) -> Result<(), SdhcError> {
        let mut timer = SystemTimer::new();
        timer.init();
        let start = Self::now_ticks();
        while !condition() {
            if Self::elapsed_us(&timer, start) >= timeout_us {
                return Err(SdhcError::Timeout);
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    fn wait_until(
        &self,
        timeout_us: u64,
        mut condition: impl FnMut(&Self) -> bool,
    ) -> Result<(), SdhcError> {
        Self::wait_until_static(timeout_us, || condition(self))
    }

    fn reg_read_u8(&self, offset: usize) -> u8 {
        // SAFETY: `self.host` points to mapped SDHCI MMIO base for the lifetime of this driver.
        unsafe { core::ptr::read_volatile(self.host.wrapping_add(offset) as *const u8) }
    }

    fn reg_read_u16(&self, offset: usize) -> u16 {
        // SAFETY: `self.host` points to mapped SDHCI MMIO base for the lifetime of this driver.
        unsafe { core::ptr::read_volatile(self.host.wrapping_add(offset) as *const u16) }
    }

    fn reg_read_u32(&self, offset: usize) -> u32 {
        // SAFETY: `self.host` points to mapped SDHCI MMIO base for the lifetime of this driver.
        unsafe { core::ptr::read_volatile(self.host.wrapping_add(offset) as *const u32) }
    }

    fn reg_write_u8(&self, offset: usize, value: u8) {
        // SAFETY: `self.host` points to mapped SDHCI MMIO base for the lifetime of this driver.
        unsafe { core::ptr::write_volatile(self.host.wrapping_add(offset), value) };
    }

    fn reg_write_u16(&self, offset: usize, value: u16) {
        // SAFETY: `self.host` points to mapped SDHCI MMIO base for the lifetime of this driver.
        unsafe { core::ptr::write_volatile(self.host.wrapping_add(offset) as *mut u16, value) };
    }

    fn reg_write_u32(&self, offset: usize, value: u32) {
        // SAFETY: `self.host` points to mapped SDHCI MMIO base for the lifetime of this driver.
        unsafe { core::ptr::write_volatile(self.host.wrapping_add(offset) as *mut u32, value) };
    }

    fn cfg_read_u32(&self, offset: usize) -> u32 {
        // SAFETY: `self.cfg` points to mapped controller-specific CFG MMIO region.
        unsafe { core::ptr::read_volatile(self.cfg.wrapping_add(offset) as *const u32) }
    }

    fn cfg_write_u32(&self, offset: usize, value: u32) {
        // SAFETY: `self.cfg` points to mapped controller-specific CFG MMIO region.
        unsafe { core::ptr::write_volatile(self.cfg.wrapping_add(offset) as *mut u32, value) };
    }

    fn configure_bcm2712_sdio1_clock(&self) -> Result<(), SdhcError> {
        configure_bcm2712_sdio1_clock_regs(self.cfg, self.max_clock_hz)
    }

    fn card_detect_asserted(&self) -> Result<bool, SdhcError> {
        // Some BCM2712 firmware DTs expose removable media with `broken-cd`, so there is no
        // dedicated GPIO to sample and we fall back to host present-state reporting.
        self.card_detect
            .map(read_brcmstb_gpio_input)
            .transpose()
            .map(|gpio_present| gpio_present.unwrap_or(true))
    }

    fn ensure_card_inserted(&self) -> Result<(), SdhcError> {
        let gpio_present = self.card_detect_asserted()?;
        let present_state = self.reg_read_u32(SDHCI_PRESENT_STATE);
        let present = decode_present_state(present_state);

        if !gpio_present || !present.card_present {
            #[cfg(any(test, debug_assertions))]
            println!(
                "sdhc: card absent gpio_present={} present_state=0x{:08x}",
                gpio_present, present_state
            );
            return Err(SdhcError::NoCard);
        }
        if !present.card_state_stable {
            #[cfg(any(test, debug_assertions))]
            println!(
                "sdhc: card state unstable gpio_present={} present_state=0x{:08x}",
                gpio_present, present_state
            );
            return Err(SdhcError::InvalidState);
        }
        Ok(())
    }

    fn reset(&self, mask: u8) -> Result<(), SdhcError> {
        self.reg_write_u8(SDHCI_SOFTWARE_RESET, mask);
        self.wait_until(SDHC_RESET_TIMEOUT_US, |s| {
            s.reg_read_u8(SDHCI_SOFTWARE_RESET) & mask == 0
        })
    }

    fn configure_card_detect(&self) -> Result<(), SdhcError> {
        let ctrl = sdio_cfg_ctrl_real_card_detect(self.cfg_read_u32(SDIO_CFG_CTRL));
        self.cfg_write_u32(SDIO_CFG_CTRL, ctrl);

        let pin_sel = sdio_cfg_sd_pin_sel_card(self.cfg_read_u32(SDIO_CFG_SD_PIN_SEL));
        self.cfg_write_u32(SDIO_CFG_SD_PIN_SEL, pin_sel);

        wait_micros(SDHC_CARD_DETECT_SETTLE_DELAY_US);
        self.ensure_card_inserted()
    }

    fn power_on(&self) -> Result<(), SdhcError> {
        self.reg_write_u8(SDHCI_POWER_CONTROL, SDHCI_POWER_330 | SDHCI_POWER_ON);
        Ok(())
    }

    fn set_clock(&self, clock_hz: u32) -> Result<(), SdhcError> {
        if clock_hz == 0 || self.max_clock_hz == 0 {
            return Err(SdhcError::InvalidClock);
        }

        self.wait_until(SDHC_CMD_TIMEOUT_US, |s| {
            let state = s.reg_read_u32(SDHCI_PRESENT_STATE);
            (state & (SDHCI_CMD_INHIBIT | SDHCI_DATA_INHIBIT)) == 0
        })?;

        self.reg_write_u16(SDHCI_CLOCK_CONTROL, 0);

        let encoded_divider = encode_clock_divider(clock_hz, self.max_clock_hz)?;
        let mut clk: u16 = pack_clock_control_divider(encoded_divider) | SDHCI_CLOCK_INT_EN;
        self.reg_write_u16(SDHCI_CLOCK_CONTROL, clk);
        self.wait_until(SDHC_CLOCK_TIMEOUT_US, |s| {
            (s.reg_read_u16(SDHCI_CLOCK_CONTROL) & SDHCI_CLOCK_INT_STABLE) != 0
        })?;
        clk |= SDHCI_CLOCK_CARD_EN;
        self.reg_write_u16(SDHCI_CLOCK_CONTROL, clk);
        Ok(())
    }

    fn enable_interrupt_masks(&self) {
        self.reg_write_u32(SDHCI_INT_ENABLE, SDHCI_INT_CMD_MASK | SDHCI_INT_DATA_MASK);
        self.reg_write_u32(SDHCI_SIGNAL_ENABLE, 0);
        self.reg_write_u32(SDHCI_INT_STATUS, SDHCI_INT_ALL_MASK);
    }

    fn write_protect_bit_set(&self) -> bool {
        write_protect_bit_set_from_present_state(self.reg_read_u32(SDHCI_PRESENT_STATE))
    }

    fn card_read_only(&self) -> bool {
        // Match the Linux default SDHCI path: a clear PRESENT_STATE write-protect bit means
        // the media should be treated as read-only, while a set bit means writable.
        !self.write_protect_bit_set()
    }

    fn controller_snapshot(&self) -> ControllerSnapshot {
        ControllerSnapshot {
            int_status: self.reg_read_u32(SDHCI_INT_STATUS),
            present_state: self.reg_read_u32(SDHCI_PRESENT_STATE),
            block_count: self.reg_read_u16(SDHCI_BLOCK_COUNT),
            transfer_mode: self.reg_read_u16(SDHCI_TRANSFER_MODE),
            command: self.reg_read_u16(SDHCI_COMMAND),
            argument: self.reg_read_u32(SDHCI_ARGUMENT),
            clock_control: self.reg_read_u16(SDHCI_CLOCK_CONTROL),
            power_control: self.reg_read_u8(SDHCI_POWER_CONTROL),
        }
    }

    #[cfg(any(test, debug_assertions))]
    fn log_controller_failure(&self, site: &str, ctx: CommandFailureContext, err: SdhcError) {
        let snapshot = self.controller_snapshot();
        let present = decode_present_state(snapshot.present_state);
        let command = decode_command_register(snapshot.command);
        println!(
            "sdhc: {} failed op={} lba={} blocks={} cmd_idx={} arg=0x{:08x} data_phase={} err={:?} int_status=0x{:08x} present_state=0x{:08x} cmd_inhibit={} data_inhibit={} doing_read={} doing_write={} card_present={} cd_stable={} write_protect={} block_count=0x{:04x} transfer_mode=0x{:04x} command=0x{:04x} cmd_reg_idx={} cmd_reg_flags=0x{:02x} argument=0x{:08x} clock_control=0x{:04x} power_control=0x{:02x}",
            site,
            ctx.op.as_str(),
            ctx.lba,
            ctx.block_count,
            ctx.cmd_idx,
            ctx.arg,
            ctx.data_phase,
            err,
            snapshot.int_status,
            snapshot.present_state,
            present.cmd_inhibit,
            present.data_inhibit,
            present.doing_read,
            present.doing_write,
            present.card_present,
            present.card_state_stable,
            present.write_protect,
            snapshot.block_count,
            snapshot.transfer_mode,
            snapshot.command,
            command.cmd_idx,
            command.flags,
            snapshot.argument,
            snapshot.clock_control,
            snapshot.power_control,
        );
    }

    #[cfg(not(any(test, debug_assertions)))]
    fn log_controller_failure(&self, _site: &str, _ctx: CommandFailureContext, _err: SdhcError) {}

    #[cfg(any(test, debug_assertions))]
    fn log_run_rw_failure(
        &self,
        op: IoOperation,
        request_lba: u64,
        request_blocks: usize,
        current_lba: u64,
        current_block_index: usize,
        err: SdhcError,
    ) {
        println!(
            "sdhc: run_rw failed op={} request_lba={} request_blocks={} current_lba={} current_block_index={} err={:?}",
            op.as_str(),
            request_lba,
            request_blocks,
            current_lba,
            current_block_index,
            err,
        );
    }

    #[cfg(not(any(test, debug_assertions)))]
    fn log_run_rw_failure(
        &self,
        _op: IoOperation,
        _request_lba: u64,
        _request_blocks: usize,
        _current_lba: u64,
        _current_block_index: usize,
        _err: SdhcError,
    ) {
    }

    #[cfg(any(test, debug_assertions))]
    fn log_command_retry(&self, ctx: CommandFailureContext, reason: &str, attempt: usize) {
        println!(
            "sdhc: retrying command op={} lba={} blocks={} cmd_idx={} arg=0x{:08x} reason={} attempt={}",
            ctx.op.as_str(),
            ctx.lba,
            ctx.block_count,
            ctx.cmd_idx,
            ctx.arg,
            reason,
            attempt,
        );
    }

    #[cfg(not(any(test, debug_assertions)))]
    fn log_command_retry(&self, _ctx: CommandFailureContext, _reason: &str, _attempt: usize) {}

    #[cfg(any(test, debug_assertions))]
    fn log_command_retry_success(&self, ctx: CommandFailureContext) {
        println!(
            "sdhc: command retry succeeded op={} lba={} blocks={} cmd_idx={} arg=0x{:08x}",
            ctx.op.as_str(),
            ctx.lba,
            ctx.block_count,
            ctx.cmd_idx,
            ctx.arg,
        );
    }

    #[cfg(not(any(test, debug_assertions)))]
    fn log_command_retry_success(&self, _ctx: CommandFailureContext) {}

    fn prepare_command_retry(&self) -> Result<(), SdhcError> {
        self.reset(SDHCI_RESET_CMD)?;
        self.reset(SDHCI_RESET_DATA)?;
        self.reg_write_u32(SDHCI_INT_STATUS, SDHCI_INT_ALL_MASK);
        Ok(())
    }

    fn cmd_flags(resp_type: RespType, data: bool) -> u16 {
        let mut flags = match resp_type {
            RespType::None => SDHCI_CMD_RESP_NONE,
            RespType::R2 => SDHCI_CMD_RESP_LONG | SDHCI_CMD_CRC,
            RespType::R1 => SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX,
            RespType::R1b => SDHCI_CMD_RESP_SHORT_BUSY | SDHCI_CMD_CRC | SDHCI_CMD_INDEX,
            RespType::R3 => SDHCI_CMD_RESP_SHORT,
            RespType::R6 => SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX,
            RespType::R7 => SDHCI_CMD_RESP_SHORT | SDHCI_CMD_CRC | SDHCI_CMD_INDEX,
        };
        if data {
            flags |= SDHCI_CMD_DATA;
        }
        flags
    }

    fn send_command(
        &self,
        cmd_idx: u8,
        arg: u32,
        resp_type: RespType,
        mut data: Option<DataTransfer<'_>>,
        io: Option<IoTraceContext>,
    ) -> Result<[u32; 4], SdhcError> {
        let mask = if cmd_idx == MMC_CMD_STOP_TRANSMISSION {
            SDHCI_CMD_INHIBIT
        } else {
            SDHCI_CMD_INHIBIT | SDHCI_DATA_INHIBIT
        };
        let has_data = data.is_some();
        let transfer_blocks = data.as_ref().map_or(0, |transfer| transfer.blocks);
        let log_ctx = CommandFailureContext::from_io(io, cmd_idx, arg, has_data, transfer_blocks);
        let mut retried = false;

        for attempt in 0..=1usize {
            self.wait_until(SDHC_CMD_TIMEOUT_US, |s| {
                (s.reg_read_u32(SDHCI_PRESENT_STATE) & mask) == 0
            })
            .inspect_err(|&err| {
                self.log_controller_failure("waiting for inhibit clear", log_ctx, err);
            })?;

            self.reg_write_u32(SDHCI_INT_STATUS, SDHCI_INT_ALL_MASK);

            let mut interrupt_mask = SDHCI_INT_RESPONSE;
            if matches!(resp_type, RespType::R1b) {
                interrupt_mask |= SDHCI_INT_DATA_END;
            }

            if let Some(transfer) = data.as_ref() {
                self.reg_write_u8(SDHCI_TIMEOUT_CONTROL, 0x0e);
                let mut mode: u16 = SDHCI_TRNS_BLK_CNT_EN;
                if transfer.blocks > 1 {
                    mode |= SDHCI_TRNS_MULTI;
                }
                if transfer.read {
                    mode |= SDHCI_TRNS_READ;
                }
                self.reg_write_u16(
                    SDHCI_BLOCK_SIZE,
                    SDHCI_MAKE_BLKSZ | (transfer.block_size as u16 & 0x0fff),
                );
                self.reg_write_u16(SDHCI_BLOCK_COUNT, transfer.blocks as u16);
                self.reg_write_u16(SDHCI_TRANSFER_MODE, mode);
            }

            self.reg_write_u32(SDHCI_ARGUMENT, arg);
            let cmd = ((cmd_idx as u16) << 8) | Self::cmd_flags(resp_type, has_data);
            self.reg_write_u16(SDHCI_COMMAND, cmd);

            let response_wait = self.wait_until(SDHC_CMD_TIMEOUT_US, |s| {
                let stat = s.reg_read_u32(SDHCI_INT_STATUS);
                (stat & SDHCI_INT_ERROR) != 0 || (stat & interrupt_mask) == interrupt_mask
            });
            if let Err(err) = response_wait {
                self.log_controller_failure("waiting for response completion", log_ctx, err);
                if attempt == 0
                    && should_retry_response_timeout(
                        cmd_idx,
                        CommandFailureSite::ResponseCompletionWait,
                        err,
                    )
                {
                    self.log_command_retry(log_ctx, "response-timeout", attempt + 2);
                    self.prepare_command_retry()?;
                    retried = true;
                    continue;
                }
                return Err(err);
            }

            let status = self.reg_read_u32(SDHCI_INT_STATUS);
            if (status & SDHCI_INT_ERROR) != 0 {
                let err = if (status & SDHCI_INT_TIMEOUT) != 0 {
                    SdhcError::Timeout
                } else {
                    SdhcError::Io
                };
                self.log_controller_failure("response completion", log_ctx, err);
                self.reg_write_u32(SDHCI_INT_STATUS, SDHCI_INT_ALL_MASK);
                self.reset(SDHCI_RESET_CMD)?;
                self.reset(SDHCI_RESET_DATA)?;
                if attempt == 0
                    && should_retry_response_timeout(
                        cmd_idx,
                        CommandFailureSite::ResponseCompletionStatus,
                        err,
                    )
                {
                    self.log_command_retry(log_ctx, "response-timeout", attempt + 2);
                    self.reg_write_u32(SDHCI_INT_STATUS, SDHCI_INT_ALL_MASK);
                    retried = true;
                    continue;
                }
                return Err(err);
            }

            let resp = if matches!(resp_type, RespType::R2) {
                [
                    (self.reg_read_u32(SDHCI_RESPONSE + 12) << 8)
                        | u32::from(self.reg_read_u8(SDHCI_RESPONSE + 11)),
                    (self.reg_read_u32(SDHCI_RESPONSE + 8) << 8)
                        | u32::from(self.reg_read_u8(SDHCI_RESPONSE + 7)),
                    (self.reg_read_u32(SDHCI_RESPONSE + 4) << 8)
                        | u32::from(self.reg_read_u8(SDHCI_RESPONSE + 3)),
                    self.reg_read_u32(SDHCI_RESPONSE) << 8,
                ]
            } else {
                [self.reg_read_u32(SDHCI_RESPONSE), 0, 0, 0]
            };
            if let Some(transfer) = data.take() {
                self.transfer_data_pio(transfer, log_ctx)?;
                self.wait_until(SDHC_DATA_TIMEOUT_US, |s| {
                    let stat = s.reg_read_u32(SDHCI_INT_STATUS);
                    (stat & SDHCI_INT_ERROR) != 0 || (stat & SDHCI_INT_DATA_END) != 0
                })
                .inspect_err(|&err| {
                    self.log_controller_failure("waiting for data-end", log_ctx, err);
                })?;
                let stat = self.reg_read_u32(SDHCI_INT_STATUS);
                if (stat & SDHCI_INT_ERROR) != 0 {
                    self.log_controller_failure("data-end", log_ctx, SdhcError::Io);
                    self.reg_write_u32(SDHCI_INT_STATUS, SDHCI_INT_ALL_MASK);
                    self.reset(SDHCI_RESET_CMD)?;
                    self.reset(SDHCI_RESET_DATA)?;
                    return Err(SdhcError::Io);
                }
            }
            self.reg_write_u32(SDHCI_INT_STATUS, SDHCI_INT_ALL_MASK);
            if retried {
                self.log_command_retry_success(log_ctx);
            }
            return Ok(resp);
        }

        Err(SdhcError::InvalidState)
    }

    fn transfer_data_pio(
        &self,
        transfer: DataTransfer<'_>,
        log_ctx: CommandFailureContext,
    ) -> Result<(), SdhcError> {
        let mut remaining_blocks = transfer.blocks;
        let mut offset = 0usize;
        while remaining_blocks > 0 {
            let wait_site = if transfer.read {
                "waiting for data-ready"
            } else {
                "waiting for data-space"
            };
            self.wait_until(SDHC_DATA_TIMEOUT_US, |s| {
                let stat = s.reg_read_u32(SDHCI_INT_STATUS);
                if (stat & SDHCI_INT_ERROR) != 0 {
                    return true;
                }
                if transfer.read {
                    (s.reg_read_u32(SDHCI_PRESENT_STATE) & SDHCI_DATA_AVAILABLE) != 0
                } else {
                    (s.reg_read_u32(SDHCI_PRESENT_STATE) & SDHCI_SPACE_AVAILABLE) != 0
                }
            })
            .inspect_err(|&err| {
                self.log_controller_failure(wait_site, log_ctx, err);
            })?;

            let stat = self.reg_read_u32(SDHCI_INT_STATUS);
            if (stat & SDHCI_INT_ERROR) != 0 {
                self.log_controller_failure(wait_site, log_ctx, SdhcError::Io);
                return Err(SdhcError::Io);
            }

            let block_end = offset
                .checked_add(transfer.block_size)
                .ok_or(SdhcError::OutOfRange)?;
            if transfer.read {
                let slice = transfer
                    .buffer
                    .get_mut(offset..block_end)
                    .ok_or(SdhcError::OutOfRange)?;
                for chunk in slice.chunks_exact_mut(4) {
                    let data = self.reg_read_u32(SDHCI_BUFFER);
                    let bytes = data.to_le_bytes();
                    chunk.copy_from_slice(&bytes);
                }
            } else {
                let slice = transfer
                    .buffer
                    .get(offset..block_end)
                    .ok_or(SdhcError::OutOfRange)?;
                for chunk in slice.chunks_exact(4) {
                    let mut bytes = [0u8; 4];
                    bytes.copy_from_slice(chunk);
                    self.reg_write_u32(SDHCI_BUFFER, u32::from_le_bytes(bytes));
                }
            }

            if transfer.read {
                self.reg_write_u32(SDHCI_INT_STATUS, SDHCI_INT_DATA_AVAIL);
            } else {
                self.reg_write_u32(SDHCI_INT_STATUS, SDHCI_INT_SPACE_AVAIL);
            }
            remaining_blocks -= 1;
            offset = block_end;
        }
        Ok(())
    }

    fn wait_for_transfer_state(&self, rca: u16) -> Result<(), SdhcError> {
        let arg = u32::from(rca) << 16;
        let mut timer = SystemTimer::new();
        timer.init();
        let start = Self::now_ticks();

        let last_status = loop {
            let status = self
                .send_command(MMC_CMD_SEND_STATUS, arg, RespType::R1, None, None)
                .map(|resp| resp[0])?;
            if card_status_is_transfer_state(status) {
                return Ok(());
            }
            if Self::elapsed_us(&timer, start) >= SDHC_CARD_STATUS_TIMEOUT_US {
                break status;
            }
            timer.wait(Duration::from_micros(SDHC_CARD_STATUS_POLL_DELAY_US));
        };

        #[cfg(any(test, debug_assertions))]
        println!(
            "sdhc: card did not reach TRANSFER state rca=0x{:x} status=0x{:08x} state={} ready_for_data={}",
            rca,
            last_status,
            card_status_current_state(last_status),
            (last_status & SD_STATUS_READY_FOR_DATA) != 0,
        );
        Err(SdhcError::Timeout)
    }

    fn set_bus_width(&self, rca: u16, bus_width: u32) -> Result<(), SdhcError> {
        let setting = sd_bus_width_setting(bus_width)?;
        let app_cmd_arg = u32::from(rca) << 16;

        self.send_command(MMC_CMD_APP_CMD, app_cmd_arg, RespType::R1, None, None)?;
        self.send_command(
            SD_CMD_APP_SET_BUS_WIDTH,
            setting.acmd6_arg,
            RespType::R1,
            None,
            None,
        )?;

        let mut host_control = self.reg_read_u8(SDHCI_HOST_CONTROL);
        host_control &= !(SDHCI_CTRL_4BITBUS | SDHCI_CTRL_8BITBUS);
        host_control |= setting.host_control_bits;
        self.reg_write_u8(SDHCI_HOST_CONTROL, host_control);

        self.wait_for_transfer_state(rca)
    }

    fn apply_conservative_transfer_clock(&self, rca: u16) -> Result<(), SdhcError> {
        // The EL2 backend intentionally stays in conservative non-UHS mode for correctness
        // until voltage switching, tuning, and related host-side features are implemented.
        self.set_clock(conservative_transfer_clock_hz())?;
        wait_micros(post_clock_settle_delay_us());
        self.wait_for_transfer_state(rca).inspect_err(|&err| {
            #[cfg(any(test, debug_assertions))]
            println!(
                "sdhc: post-clock transfer-state recheck failed rca=0x{:x} clock_hz={} err={:?}",
                rca,
                conservative_transfer_clock_hz(),
                err,
            );
        })
    }

    fn card_init_sequence(&self) -> Result<(), SdhcError> {
        self.ensure_card_inserted()?;

        self.send_command(MMC_CMD_GO_IDLE_STATE, 0, RespType::None, None, None)?;
        self.send_command(MMC_CMD_SEND_IF_COND, 0x1aa, RespType::R7, None, None)?;

        let mut ocr = 0u32;
        let mut ready = false;
        let mut timer = SystemTimer::new();
        timer.init();
        for _ in 0..SDHC_ACMD41_RETRY_MAX {
            self.send_command(MMC_CMD_APP_CMD, 0, RespType::R1, None, None)?;
            let resp = self.send_command(
                SD_CMD_APP_SEND_OP_COND,
                OCR_REQUEST,
                RespType::R3,
                None,
                None,
            )?;
            ocr = resp[0];
            if (ocr & OCR_BUSY) != 0 {
                ready = true;
                break;
            }
            timer.wait(Duration::from_micros(SDHC_ACMD41_POLL_DELAY_US));
        }
        if !ready {
            return Err(SdhcError::Timeout);
        }
        let high_capacity = (ocr & OCR_HCS) != 0;

        let _cid = self.send_command(MMC_CMD_ALL_SEND_CID, 0, RespType::R2, None, None)?;
        let rca_resp = self.send_command(MMC_CMD_SET_RELATIVE_ADDR, 0, RespType::R6, None, None)?;
        let rca = (rca_resp[0] >> 16) as u16;
        let csd = self.send_command(
            MMC_CMD_SEND_CSD,
            u32::from(rca) << 16,
            RespType::R2,
            None,
            None,
        )?;
        let select_card = select_card_command(rca);
        self.send_command(
            select_card.idx,
            select_card.arg,
            select_card.resp_type,
            None,
            None,
        )?;
        self.wait_for_transfer_state(rca)?;
        self.set_bus_width(rca, self.bus_width)?;
        self.send_command(
            MMC_CMD_SET_BLOCKLEN,
            SDHC_BLOCK_SIZE as u32,
            RespType::R1,
            None,
            None,
        )?;
        self.wait_for_transfer_state(rca)?;
        self.apply_conservative_transfer_clock(rca)?;

        let capacity_blocks = decode_capacity_blocks(&csd, high_capacity)?;
        let present_state = self.reg_read_u32(SDHCI_PRESENT_STATE);
        let write_protect_bit_set = write_protect_bit_set_from_present_state(present_state);
        let read_only = !write_protect_bit_set;
        debug_assert_eq!(read_only, self.card_read_only());
        let mut guard = self.card.lock();
        *guard = Some(CardState {
            high_capacity,
            capacity_blocks,
            read_only,
        });
        println!(
            "sdhc: initialized rca=0x{:x} high_capacity={} blocks={}",
            rca, high_capacity, capacity_blocks
        );
        println!(
            "sdhc: ro-detect present_state=0x{:08x} wp_bit_set={} read_only={}",
            present_state, write_protect_bit_set, read_only
        );
        Ok(())
    }

    fn checked_card(&self) -> Result<CardState, SdhcError> {
        let guard = self.card.lock();
        (*guard).ok_or(SdhcError::NotReady)
    }

    fn run_rw(&self, lba: u64, buf: &mut [u8], write: bool) -> Result<(), SdhcError> {
        let card = self.checked_card()?;
        if buf.is_empty() {
            return Err(SdhcError::InvalidParam);
        }
        if (buf.len() % SDHC_BLOCK_SIZE) != 0 {
            return Err(SdhcError::Align);
        }
        let blocks =
            u64::try_from(buf.len() / SDHC_BLOCK_SIZE).map_err(|_| SdhcError::OutOfRange)?;
        if lba
            .checked_add(blocks)
            .filter(|end| *end <= card.capacity_blocks)
            .is_none()
        {
            return Err(SdhcError::OutOfRange);
        }
        if write && card.read_only {
            return Err(SdhcError::ReadOnly);
        }
        let op = if write {
            IoOperation::Write
        } else {
            IoOperation::Read
        };
        let mut current_lba = lba;
        let mut offset = 0usize;
        let total_blocks = usize::try_from(blocks).map_err(|_| SdhcError::OutOfRange)?;
        while offset < buf.len() {
            let remaining_blocks = (buf.len() - offset) / SDHC_BLOCK_SIZE;
            let plan = plan_data_command(current_lba, remaining_blocks, write, card.high_capacity)?;
            let chunk_blocks = plan.block_count;
            let chunk_end = offset
                .checked_add(chunk_blocks * SDHC_BLOCK_SIZE)
                .ok_or(SdhcError::OutOfRange)?;
            let chunk = buf
                .get_mut(offset..chunk_end)
                .ok_or(SdhcError::OutOfRange)?;
            let transfer = DataTransfer {
                read: !write,
                blocks: chunk_blocks,
                block_size: SDHC_BLOCK_SIZE,
                buffer: chunk,
            };
            let io = IoTraceContext {
                op,
                lba: current_lba,
                block_count: chunk_blocks,
            };
            if let Err(err) = self.send_command(
                plan.cmd_idx,
                plan.arg,
                RespType::R1,
                Some(transfer),
                Some(io),
            ) {
                self.log_run_rw_failure(
                    op,
                    lba,
                    total_blocks,
                    current_lba,
                    offset / SDHC_BLOCK_SIZE,
                    err,
                );
                return Err(err);
            }
            current_lba = current_lba
                .checked_add(u64::try_from(chunk_blocks).map_err(|_| SdhcError::OutOfRange)?)
                .ok_or(SdhcError::OutOfRange)?;
            offset = chunk_end;
        }
        debug_assert_eq!(offset, total_blocks * SDHC_BLOCK_SIZE);
        Ok(())
    }
}

impl BlockDevice for Bcm2712Sdhc {
    fn init(&mut self) -> Result<(), IoError> {
        self.configure_bcm2712_sdio1_clock()
            .map_err(IoError::from)?;
        self.reset(SDHCI_RESET_ALL).map_err(IoError::from)?;
        self.configure_card_detect().map_err(IoError::from)?;
        self.power_on().map_err(IoError::from)?;
        self.set_clock(self.min_clock_hz).map_err(IoError::from)?;
        self.enable_interrupt_masks();
        self.card_init_sequence().map_err(IoError::from)
    }

    fn block_size(&self) -> usize {
        SDHC_BLOCK_SIZE
    }

    fn num_blocks(&self) -> u64 {
        let guard = self.card.lock();
        guard.as_ref().map(|card| card.capacity_blocks).unwrap_or(0)
    }

    fn read_at(&self, lba: Lba, buf: &mut [MaybeUninit<u8>]) -> Result<(), IoError> {
        if !READY.load(Ordering::Acquire) {
            return Err(IoError::NotReady);
        }
        // SAFETY: `MaybeUninit<u8>` and `u8` share layout; read path fully initializes bytes.
        let raw =
            unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len()) };
        self.run_rw(lba, raw, false).map_err(IoError::from)
    }

    fn write_at(&self, lba: Lba, buf: &[u8]) -> Result<(), IoError> {
        if !READY.load(Ordering::Acquire) {
            return Err(IoError::NotReady);
        }
        if buf.is_empty() {
            return Err(IoError::InvalidParam);
        }
        if (buf.len() % SDHC_BLOCK_SIZE) != 0 {
            return Err(IoError::Align);
        }
        let mut local = [0u8; SDHC_BLOCK_SIZE * 8];
        let mut offset = 0usize;
        let mut current_lba = lba;
        while offset < buf.len() {
            let chunk_len = (buf.len() - offset).min(local.len());
            let chunk_end = offset.checked_add(chunk_len).ok_or(IoError::OutOfRange)?;
            local[..chunk_len].copy_from_slice(&buf[offset..chunk_end]);
            self.run_rw(current_lba, &mut local[..chunk_len], true)
                .map_err(IoError::from)?;
            current_lba = current_lba
                .checked_add((chunk_len / SDHC_BLOCK_SIZE) as u64)
                .ok_or(IoError::OutOfRange)?;
            offset = chunk_end;
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), IoError> {
        Ok(())
    }

    fn max_io_bytes(&self) -> Result<Option<usize>, IoError> {
        Ok(None)
    }

    fn is_read_only(&self) -> Result<bool, IoError> {
        let guard = self.card.lock();
        guard
            .as_ref()
            .map(|card| card.read_only)
            .ok_or(IoError::NotReady)
    }

    fn uninstall(&self) {}
}

pub fn init_from_dtb(dtb: &DtbParser) -> Result<&'static dyn BlockDevice, SdhcError> {
    let dev = Bcm2712Sdhc::init_from_dtb(dtb)?;
    Ok(dev)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RespType {
    None,
    R1,
    R1b,
    R2,
    R3,
    R6,
    R7,
}

struct DataTransfer<'a> {
    read: bool,
    blocks: usize,
    block_size: usize,
    buffer: &'a mut [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CommandSpec {
    idx: u8,
    arg: u32,
    resp_type: RespType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IoOperation {
    Control,
    Read,
    Write,
}

impl IoOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IoTraceContext {
    op: IoOperation,
    lba: u64,
    block_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CommandFailureContext {
    op: IoOperation,
    lba: u64,
    block_count: usize,
    cmd_idx: u8,
    arg: u32,
    data_phase: bool,
}

impl CommandFailureContext {
    fn from_io(
        io: Option<IoTraceContext>,
        cmd_idx: u8,
        arg: u32,
        data_phase: bool,
        transfer_blocks: usize,
    ) -> Self {
        let io = io.unwrap_or(IoTraceContext {
            op: IoOperation::Control,
            lba: 0,
            block_count: transfer_blocks,
        });
        Self {
            op: io.op,
            lba: io.lba,
            block_count: io.block_count,
            cmd_idx,
            arg,
            data_phase,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ControllerSnapshot {
    int_status: u32,
    present_state: u32,
    block_count: u16,
    transfer_mode: u16,
    command: u16,
    argument: u32,
    clock_control: u16,
    power_control: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PresentStateDecode {
    cmd_inhibit: bool,
    data_inhibit: bool,
    doing_read: bool,
    doing_write: bool,
    card_present: bool,
    card_state_stable: bool,
    write_protect: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CommandRegisterDecode {
    cmd_idx: u8,
    flags: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlannedDataCommand {
    cmd_idx: u8,
    arg: u32,
    block_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandFailureSite {
    ResponseCompletionWait,
    ResponseCompletionStatus,
}

fn block_command_arg(lba: u64, high_capacity: bool) -> Result<u32, SdhcError> {
    if high_capacity {
        return u32::try_from(lba).map_err(|_| SdhcError::OutOfRange);
    }

    let byte_addr = lba
        .checked_mul(SDHC_BLOCK_SIZE as u64)
        .ok_or(SdhcError::OutOfRange)?;
    u32::try_from(byte_addr).map_err(|_| SdhcError::OutOfRange)
}

fn conservative_chunk_blocks(remaining_blocks: usize) -> Result<usize, SdhcError> {
    if remaining_blocks == 0 {
        return Err(SdhcError::InvalidParam);
    }
    Ok(remaining_blocks
        .min(1)
        .min(SDHC_MAX_TRANSFER_BLOCKS_PER_CMD))
}

fn plan_data_command(
    lba: u64,
    remaining_blocks: usize,
    write: bool,
    high_capacity: bool,
) -> Result<PlannedDataCommand, SdhcError> {
    let block_count = conservative_chunk_blocks(remaining_blocks)?;
    Ok(PlannedDataCommand {
        cmd_idx: if write {
            MMC_CMD_WRITE_SINGLE_BLOCK
        } else {
            MMC_CMD_READ_SINGLE_BLOCK
        },
        arg: block_command_arg(lba, high_capacity)?,
        block_count,
    })
}

fn conservative_transfer_clock_hz() -> u32 {
    SDHC_CONSERVATIVE_TRANSFER_CLOCK_HZ
}

fn post_clock_settle_delay_us() -> u64 {
    SDHC_POST_CLOCK_SETTLE_DELAY_US
}

fn sdio_cfg_ctrl_real_card_detect(ctrl: u32) -> u32 {
    ctrl & !(SDIO_CFG_CTRL_SDCD_N_TEST_EN | SDIO_CFG_CTRL_SDCD_N_TEST_LEV)
}

fn sdio_cfg_sd_pin_sel_card(pin_sel: u32) -> u32 {
    (pin_sel & !SDIO_CFG_SD_PIN_SEL_MASK) | SDIO_CFG_SD_PIN_SEL_CARD
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SdBusWidthSetting {
    acmd6_arg: u32,
    host_control_bits: u8,
}

fn sd_bus_width_setting(bus_width: u32) -> Result<SdBusWidthSetting, SdhcError> {
    match bus_width {
        1 => Ok(SdBusWidthSetting {
            acmd6_arg: SD_BUS_WIDTH_1BIT_ARG,
            host_control_bits: 0,
        }),
        4 => Ok(SdBusWidthSetting {
            acmd6_arg: SD_BUS_WIDTH_4BIT_ARG,
            host_control_bits: SDHCI_CTRL_4BITBUS,
        }),
        _ => Err(SdhcError::Unsupported),
    }
}

fn resolve_sd_slot_bus_width(bus_width: u32, broken_cd: bool) -> Result<u32, SdhcError> {
    match bus_width {
        1 | 4 => Ok(bus_width),
        8 if broken_cd => Ok(4),
        0 => Err(SdhcError::DtbInvalid(
            "sdhc: missing or zero bus-width for SD slot",
        )),
        _ => Err(SdhcError::DtbInvalid(
            "sdhc: unsupported bus-width for SD slot",
        )),
    }
}

fn bcm2712_base_clock_mhz(clock_hz: u32) -> u32 {
    (clock_hz / 1_000_000)
        .max(1)
        .min(SDIO_CFG_CQ_CAPABILITY_BASE_CLOCK_MASK)
}

fn bcm2712_cq_capability_value(clock_hz: u32) -> u32 {
    SDIO_CFG_CQ_CAPABILITY_FMUL_DEFAULT | bcm2712_base_clock_mhz(clock_hz)
}

fn decode_present_state(state: u32) -> PresentStateDecode {
    PresentStateDecode {
        cmd_inhibit: (state & SDHCI_CMD_INHIBIT) != 0,
        data_inhibit: (state & SDHCI_DATA_INHIBIT) != 0,
        doing_read: (state & SDHCI_DOING_READ) != 0,
        doing_write: (state & SDHCI_DOING_WRITE) != 0,
        card_present: (state & SDHCI_CARD_PRESENT) != 0,
        card_state_stable: (state & SDHCI_CARD_STATE_STABLE) != 0,
        write_protect: (state & SDHCI_WRITE_PROTECT) != 0,
    }
}

fn write_protect_bit_set_from_present_state(present_state: u32) -> bool {
    (present_state & SDHCI_WRITE_PROTECT) != 0
}

fn decode_command_register(command: u16) -> CommandRegisterDecode {
    CommandRegisterDecode {
        cmd_idx: (command >> 8) as u8,
        flags: (command & 0x00ff) as u8,
    }
}

fn should_retry_response_timeout(cmd_idx: u8, site: CommandFailureSite, err: SdhcError) -> bool {
    err == SdhcError::Timeout
        && matches!(
            site,
            CommandFailureSite::ResponseCompletionWait
                | CommandFailureSite::ResponseCompletionStatus
        )
        && matches!(
            cmd_idx,
            MMC_CMD_READ_SINGLE_BLOCK | MMC_CMD_WRITE_SINGLE_BLOCK
        )
}

fn card_status_current_state(status: u32) -> u8 {
    ((status & SD_STATUS_CURRENT_STATE_MASK) >> SD_STATUS_CURRENT_STATE_SHIFT) as u8
}

fn card_status_is_transfer_state(status: u32) -> bool {
    card_status_current_state(status) == SD_STATUS_TRANSFER_STATE
}

fn decode_capacity_blocks(csd: &[u32; 4], high_capacity: bool) -> Result<u64, SdhcError> {
    if high_capacity {
        let c_size = ((u64::from(csd[1]) & 0x3f) << 16) | ((u64::from(csd[2]) & 0xffff_0000) >> 16);
        let bytes = (c_size + 1)
            .checked_shl(10)
            .ok_or(SdhcError::OutOfRange)?
            .checked_mul(SDHC_BLOCK_SIZE as u64)
            .ok_or(SdhcError::OutOfRange)?;
        return Ok(bytes / SDHC_BLOCK_SIZE as u64);
    }

    let c_size = ((u64::from(csd[1]) & 0x3ff) << 2) | ((u64::from(csd[2]) & 0xc000_0000) >> 30);
    let c_mult = (u64::from(csd[2]) & 0x0003_8000) >> 15;
    let read_bl_len_shift = ((csd[1] >> 16) & 0xf) as u32;
    let read_bl_len = 1u64
        .checked_shl(read_bl_len_shift)
        .ok_or(SdhcError::OutOfRange)?;
    let bytes = (c_size + 1)
        .checked_shl((c_mult + 2) as u32)
        .ok_or(SdhcError::OutOfRange)?
        .checked_mul(read_bl_len)
        .ok_or(SdhcError::OutOfRange)?;
    Ok(bytes / SDHC_BLOCK_SIZE as u64)
}

fn encode_clock_divider(requested_clock_hz: u32, max_clock_hz: u32) -> Result<u16, SdhcError> {
    if requested_clock_hz == 0 || max_clock_hz == 0 {
        return Err(SdhcError::InvalidClock);
    }
    if requested_clock_hz >= max_clock_hz {
        return Ok(0);
    }

    let denominator = u64::from(requested_clock_hz)
        .checked_mul(2)
        .ok_or(SdhcError::InvalidClock)?;
    let mut encoded = u64::from(max_clock_hz).div_ceil(denominator);
    if encoded == 0 {
        encoded = 1;
    }
    if encoded > u64::from(SDHC_MAX_CLOCK_DIVIDER) {
        encoded = u64::from(SDHC_MAX_CLOCK_DIVIDER);
    }
    u16::try_from(encoded).map_err(|_| SdhcError::InvalidClock)
}

fn pack_clock_control_divider(encoded_divider: u16) -> u16 {
    ((encoded_divider & SDHCI_DIV_MASK) << SDHCI_DIVIDER_SHIFT)
        | (((encoded_divider & SDHCI_DIV_HI_MASK) >> 8) << SDHCI_DIVIDER_HI_SHIFT)
}

fn select_card_command(rca: u16) -> CommandSpec {
    CommandSpec {
        idx: MMC_CMD_SELECT_CARD,
        arg: u32::from(rca) << 16,
        resp_type: RespType::R1b,
    }
}

fn property_string_matches(
    view: &dtb::DtbNodeView<'_, '_>,
    key: &str,
    expected: &str,
    invalid_msg: &'static str,
) -> Result<bool, SdhcError> {
    let Some(bytes) = view.property_bytes(key).map_err(SdhcError::DtbParse)? else {
        return Ok(false);
    };
    let value = bytes.split(|byte| *byte == 0).next().unwrap_or(bytes);
    let value = core::str::from_utf8(value).map_err(|_| SdhcError::DtbInvalid(invalid_msg))?;
    Ok(value == expected)
}

fn property_u32_or_default(
    view: &dtb::DtbNodeView<'_, '_>,
    key: &str,
    default: u32,
    invalid_msg: &'static str,
) -> Result<u32, SdhcError> {
    match view.property_u32_be(key) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Ok(default),
        Err(_) => Err(SdhcError::DtbInvalid(invalid_msg)),
    }
}

fn decode_reg_name(name: &str) -> SdhcRegName {
    match name {
        "host" => SdhcRegName::Host,
        "cfg" => SdhcRegName::Cfg,
        "busisol" => SdhcRegName::BusIsol,
        "lcpll" => SdhcRegName::LcPll,
        _ => SdhcRegName::Other,
    }
}

fn parse_reg_names(
    bytes: &[u8],
) -> Result<([SdhcRegName; SDHC_MAX_REG_ENTRIES], usize), SdhcError> {
    let mut names = [SdhcRegName::Other; SDHC_MAX_REG_ENTRIES];
    let mut count = 0usize;
    let mut start = 0usize;
    while start < bytes.len() {
        let end =
            start
                + bytes[start..].iter().position(|byte| *byte == 0).ok_or(
                    SdhcError::DtbInvalid("sdhc: reg-names missing NUL terminator"),
                )?;
        if count >= names.len() {
            return Err(SdhcError::DtbInvalid("sdhc: too many reg-names entries"));
        }
        let name = core::str::from_utf8(&bytes[start..end])
            .map_err(|_| SdhcError::DtbInvalid("sdhc: reg-names contains invalid UTF-8"))?;
        names[count] = decode_reg_name(name);
        count += 1;
        start = end + 1;
    }
    Ok((names, count))
}

fn assign_region(
    slot: &mut Option<HostRegion>,
    region: HostRegion,
    duplicate_msg: &'static str,
) -> Result<(), SdhcError> {
    if slot.replace(region).is_some() {
        return Err(SdhcError::DtbInvalid(duplicate_msg));
    }
    Ok(())
}

fn parse_named_regions(view: &dtb::DtbNodeView<'_, '_>) -> Result<NamedRegions, SdhcError> {
    let Some(reg_names_bytes) = view
        .property_bytes("reg-names")
        .map_err(SdhcError::DtbParse)?
    else {
        return Ok(NamedRegions::default());
    };
    let (reg_names, reg_name_count) = parse_reg_names(reg_names_bytes)?;

    let mut regs = [HostRegion::default(); SDHC_MAX_REG_ENTRIES];
    let mut reg_count = 0usize;
    let mut reg_iter = view.reg_iter().map_err(SdhcError::DtbParse)?;
    while let Some(entry) = reg_iter.next() {
        if reg_count >= regs.len() {
            return Err(SdhcError::DtbInvalid("sdhc: too many reg entries"));
        }
        let (base, size) = entry.map_err(SdhcError::DtbParse)?;
        regs[reg_count] = HostRegion { base, size };
        reg_count += 1;
    }
    if reg_count != reg_name_count {
        return Err(SdhcError::DtbInvalid("sdhc: reg/reg-names length mismatch"));
    }

    let mut named = NamedRegions::default();
    for index in 0..reg_count {
        match reg_names[index] {
            SdhcRegName::Host => {
                assign_region(&mut named.host, regs[index], "sdhc: duplicate host reg")?
            }
            SdhcRegName::Cfg => {
                assign_region(&mut named.cfg, regs[index], "sdhc: duplicate cfg reg")?
            }
            SdhcRegName::BusIsol => assign_region(
                &mut named.busisol,
                regs[index],
                "sdhc: duplicate busisol reg",
            )?,
            SdhcRegName::LcPll => {
                assign_region(&mut named.lcpll, regs[index], "sdhc: duplicate lcpll reg")?
            }
            SdhcRegName::Other => {}
        }
    }
    Ok(named)
}

fn has_wifi_child(view: &dtb::DtbNodeView<'_, '_>) -> Result<bool, SdhcError> {
    let mut found = false;
    let walk = view.for_each_child_view(&mut |child| {
        if child.name() == "wifi@1" {
            found = true;
            return Ok(core::ops::ControlFlow::Break(()));
        }
        Ok(core::ops::ControlFlow::Continue(()))
    });
    match walk {
        Ok(core::ops::ControlFlow::Break(())) | Ok(core::ops::ControlFlow::Continue(())) => {
            Ok(found)
        }
        Err(WalkError::Dtb(err)) => Err(SdhcError::DtbParse(err)),
        Err(WalkError::User(err)) => Err(err),
    }
}

fn parse_be_u32_triplet(
    bytes: &[u8],
    invalid_msg: &'static str,
) -> Result<(u32, u32, u32), SdhcError> {
    if bytes.len() < (size_of::<u32>() * 3) || !bytes.len().is_multiple_of(size_of::<u32>()) {
        return Err(SdhcError::DtbInvalid(invalid_msg));
    }
    let phandle = u32::from_be_bytes(
        bytes[0..4]
            .try_into()
            .map_err(|_| SdhcError::DtbInvalid(invalid_msg))?,
    );
    let pin = u32::from_be_bytes(
        bytes[4..8]
            .try_into()
            .map_err(|_| SdhcError::DtbInvalid(invalid_msg))?,
    );
    let flags = u32::from_be_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| SdhcError::DtbInvalid(invalid_msg))?,
    );
    Ok((phandle, pin, flags))
}

fn find_gpio_controller_base(dtb: &DtbParser, phandle: u32) -> Result<usize, SdhcError> {
    dtb.with_node_view_by_phandle(phandle, &mut |node| {
        // Keep semantic errors typed while the DTB helper owns traversal errors.
        Ok::<_, &'static str>((|| {
            if !node
                .compatible_contains("brcm,brcmstb-gpio")
                .map_err(SdhcError::DtbParse)?
            {
                return Err(SdhcError::DtbInvalid(
                    "sdhc: invalid supply gpio controller",
                ));
            }
            let (base, _size) = node
                .reg_iter()
                .map_err(SdhcError::DtbParse)?
                .next()
                .ok_or(SdhcError::DtbInvalid("sdhc: missing supply gpio reg"))?
                .map_err(SdhcError::DtbParse)?;
            Ok(base)
        })())
    })
    .map_err(SdhcError::DtbParse)?
    .ok_or(SdhcError::DtbInvalid(
        "sdhc: supply gpio controller not found",
    ))?
}

fn parse_gpio_line_config(
    dtb: &DtbParser,
    bytes: &[u8],
    invalid_msg: &'static str,
) -> Result<GpioLineConfig, SdhcError> {
    let (controller_phandle, pin, flags) = parse_be_u32_triplet(bytes, invalid_msg)?;
    let pin = u8::try_from(pin).map_err(|_| SdhcError::DtbInvalid(invalid_msg))?;
    Ok(GpioLineConfig {
        base: find_gpio_controller_base(dtb, controller_phandle)?,
        pin,
        active_low: (flags & OF_GPIO_ACTIVE_LOW) != 0,
    })
}

fn parse_vqmmc_supply(
    view: &dtb::DtbNodeView<'_, '_>,
    dtb: &DtbParser,
) -> Result<Option<(GpioOutputConfig, u32)>, SdhcError> {
    let Some(supply_phandle) = view
        .property_u32_be("vqmmc-supply")
        .map_err(SdhcError::DtbParse)?
    else {
        return Ok(None);
    };

    let node = dtb
        .find_node_view_by_phandle(supply_phandle)
        .map_err(SdhcError::DtbParse)?
        .ok_or(SdhcError::DtbInvalid("sdhc: vqmmc regulator not found"))?;
    if !node
        .compatible_contains("regulator-gpio")
        .map_err(SdhcError::DtbParse)?
    {
        return Err(SdhcError::DtbInvalid("sdhc: invalid vqmmc regulator"));
    }
    let gpios = node
        .property_bytes("gpios")
        .map_err(SdhcError::DtbParse)?
        .ok_or(SdhcError::DtbInvalid("sdhc: missing vqmmc gpios property"))?;
    let states = node
        .property_bytes("states")
        .map_err(SdhcError::DtbParse)?
        .ok_or(SdhcError::DtbInvalid("sdhc: missing vqmmc states property"))?;
    if states.len() < (size_of::<u32>() * 2) || !states.len().is_multiple_of(size_of::<u32>() * 2) {
        return Err(SdhcError::DtbInvalid("sdhc: invalid vqmmc states property"));
    }
    let desired_uv = node
        .property_u32_be("regulator-max-microvolt")
        .map_err(SdhcError::DtbParse)?
        .unwrap_or(SDHC_3V3_MICROVOLTS);
    let settle_us = node
        .property_u32_be("regulator-settling-time-us")
        .map_err(SdhcError::DtbParse)?
        .unwrap_or(0);
    let mut fallback = None;
    let mut selected = None;
    let invalid_states = |_| SdhcError::DtbInvalid("sdhc: invalid vqmmc states property");
    for chunk in states.chunks_exact(size_of::<u32>() * 2) {
        let microvolts = u32::from_be_bytes(chunk[0..4].try_into().map_err(invalid_states)?);
        let gpio_state = u32::from_be_bytes(chunk[4..8].try_into().map_err(invalid_states)?);
        fallback = Some(gpio_state != 0);
        if microvolts == desired_uv {
            selected = Some(gpio_state != 0);
        }
    }
    Ok(Some((
        GpioOutputConfig {
            line: parse_gpio_line_config(dtb, gpios, "sdhc: invalid vqmmc gpios property")?,
            logical_high: selected
                .or(fallback)
                .ok_or(SdhcError::DtbInvalid("sdhc: invalid vqmmc states property"))?,
        },
        settle_us,
    )))
}

fn parse_vmmc_supply(
    view: &dtb::DtbNodeView<'_, '_>,
    dtb: &DtbParser,
) -> Result<Option<GpioOutputConfig>, SdhcError> {
    let Some(supply_phandle) = view
        .property_u32_be("vmmc-supply")
        .map_err(SdhcError::DtbParse)?
    else {
        return Ok(None);
    };

    let node = dtb
        .find_node_view_by_phandle(supply_phandle)
        .map_err(SdhcError::DtbParse)?
        .ok_or(SdhcError::DtbInvalid("sdhc: vmmc regulator not found"))?;
    if !node
        .compatible_contains("regulator-fixed")
        .map_err(SdhcError::DtbParse)?
    {
        return Err(SdhcError::DtbInvalid("sdhc: invalid vmmc regulator"));
    }
    let gpios = match node.property_bytes("gpios").map_err(SdhcError::DtbParse)? {
        Some(bytes) => Some(bytes),
        None => node.property_bytes("gpio").map_err(SdhcError::DtbParse)?,
    }
    .ok_or(SdhcError::DtbInvalid("sdhc: missing vmmc gpio property"))?;
    Ok(Some(GpioOutputConfig {
        line: parse_gpio_line_config(dtb, gpios, "sdhc: invalid vmmc gpio property")?,
        logical_high: node
            .property_bytes("enable-active-high")
            .map_err(SdhcError::DtbParse)?
            .is_some(),
    }))
}

fn parse_power_from_view(
    view: &dtb::DtbNodeView<'_, '_>,
    dtb: &DtbParser,
) -> Result<SdhcPowerConfig, SdhcError> {
    let mut power = SdhcPowerConfig::default();
    if let Some((vqmmc_select, settle_us)) = parse_vqmmc_supply(view, dtb)? {
        power.vqmmc_select = Some(vqmmc_select);
        power.vqmmc_settle_us = settle_us;
    }
    power.vmmc_enable = parse_vmmc_supply(view, dtb)?;
    Ok(power)
}

fn parse_candidate_from_view(
    view: &dtb::DtbNodeView<'_, '_>,
    dtb: &DtbParser,
) -> Result<SdhcDtCandidate, SdhcError> {
    let named = parse_named_regions(view)?;
    Ok(SdhcDtCandidate {
        host: named.host,
        cfg: named.cfg,
        busisol: named.busisol,
        lcpll: named.lcpll,
        max_clock_hz: find_clock_frequency(view, dtb)?,
        power: parse_power_from_view(view, dtb)?,
        card_detect: view
            .property_bytes("cd-gpios")
            .map_err(SdhcError::DtbParse)?
            .map(|bytes| parse_gpio_line_config(dtb, bytes, "sdhc: invalid cd-gpios property"))
            .transpose()?,
        broken_cd: view
            .property_bytes("broken-cd")
            .map_err(SdhcError::DtbParse)?
            .is_some(),
        has_brcmstb_compat: view
            .compatible_contains(DT_COMPAT_BRCMSTB_SDHCI)
            .map_err(SdhcError::DtbParse)?,
        status_okay: property_string_matches(
            view,
            "status",
            "okay",
            "sdhc: invalid status property",
        )?,
        bus_width: property_u32_or_default(
            view,
            "bus-width",
            0,
            "sdhc: invalid bus-width property",
        )?,
        non_removable: view
            .property_bytes("non-removable")
            .map_err(SdhcError::DtbParse)?
            .is_some(),
        has_wifi_child: has_wifi_child(view)?,
    })
}

fn matching_candidate_indices(
    candidates: &[SdhcDtCandidate],
    predicate: impl Fn(&SdhcDtCandidate) -> bool,
) -> ([usize; SDHC_MAX_DT_CANDIDATES], usize) {
    let mut indices = [usize::MAX; SDHC_MAX_DT_CANDIDATES];
    let mut count = 0usize;
    for (index, candidate) in candidates.iter().enumerate() {
        if predicate(candidate) {
            indices[count] = index;
            count += 1;
        }
    }
    (indices, count)
}

fn prefer_candidate_subset(
    candidates: &[SdhcDtCandidate],
    indices: &mut [usize; SDHC_MAX_DT_CANDIDATES],
    len: usize,
    predicate: impl Fn(&SdhcDtCandidate) -> bool,
) -> usize {
    let mut preferred = [usize::MAX; SDHC_MAX_DT_CANDIDATES];
    let mut preferred_len = 0usize;
    for position in 0..len {
        let index = indices[position];
        if predicate(&candidates[index]) {
            preferred[preferred_len] = index;
            preferred_len += 1;
        }
    }
    if preferred_len == 0 {
        return len;
    }
    indices[..preferred_len].copy_from_slice(&preferred[..preferred_len]);
    preferred_len
}

#[cfg(test)]
fn log_dt_candidates(candidates: &[SdhcDtCandidate]) {
    for (index, candidate) in candidates.iter().enumerate() {
        std::println!(
            "sdhc: dt candidate[{}] host={:?} cfg={:?} cd={} broken_cd={} bus_width={} non_removable={} has_wifi_child={} has_brcmstb_compat={}",
            index,
            candidate.host,
            candidate.cfg,
            candidate.card_detect.is_some(),
            candidate.broken_cd,
            candidate.bus_width,
            candidate.non_removable,
            candidate.has_wifi_child,
            candidate.has_brcmstb_compat
        );
    }
}

#[cfg(all(not(test), debug_assertions))]
fn log_dt_candidates(candidates: &[SdhcDtCandidate]) {
    for (index, candidate) in candidates.iter().enumerate() {
        println!(
            "sdhc: dt candidate[{}] host={:?} cfg={:?} cd={} broken_cd={} bus_width={} non_removable={} has_wifi_child={} has_brcmstb_compat={}",
            index,
            candidate.host,
            candidate.cfg,
            candidate.card_detect.is_some(),
            candidate.broken_cd,
            candidate.bus_width,
            candidate.non_removable,
            candidate.has_wifi_child,
            candidate.has_brcmstb_compat
        );
    }
}

#[cfg(not(any(test, debug_assertions)))]
fn log_dt_candidates(_candidates: &[SdhcDtCandidate]) {}

fn select_sd_slot_candidate(candidates: &[SdhcDtCandidate]) -> Result<SdhcConfig, SdhcError> {
    let (mut indices, mut len) =
        matching_candidate_indices(candidates, |candidate| candidate.is_supported_shape());
    if len == 0 {
        return Err(SdhcError::DtbInvalid("sdhc: no valid SD-slot candidate"));
    }

    len = prefer_candidate_subset(candidates, &mut indices, len, |candidate| {
        candidate.host_base() == Some(BCM2712_SD_SLOT_HOST_BASE)
    });
    if len > 1 {
        len = prefer_candidate_subset(candidates, &mut indices, len, |candidate| {
            !candidate.non_removable
        });
    }
    if len > 1 {
        len = prefer_candidate_subset(candidates, &mut indices, len, |candidate| {
            !candidate.has_wifi_child
        });
    }
    if len > 1 {
        len = prefer_candidate_subset(candidates, &mut indices, len, |candidate| {
            candidate.has_brcmstb_compat
        });
    }

    match len {
        1 => {
            let candidate = candidates[indices[0]];
            if !candidate.is_viable_sd_slot() {
                return Err(SdhcError::DtbInvalid("sdhc: no valid SD-slot candidate"));
            }
            candidate.into_config()
        }
        0 => Err(SdhcError::DtbInvalid("sdhc: no valid SD-slot candidate")),
        _ => Err(SdhcError::DtbInvalid(
            "sdhc: ambiguous DT candidate selection",
        )),
    }
}

fn parse_from_dtb(dtb: &DtbParser) -> Result<SdhcConfig, SdhcError> {
    let mut candidates = [SdhcDtCandidate::default(); SDHC_MAX_DT_CANDIDATES];
    let mut count = 0usize;
    let walk =
        dtb.find_nodes_by_compatible_view(DT_COMPAT_BCM2712_SDHCI, &mut |view, _node_name| {
            if count >= candidates.len() {
                return Err(WalkError::User(SdhcError::DtbInvalid(
                    "sdhc: too many compatible nodes",
                )));
            }
            candidates[count] = parse_candidate_from_view(view, dtb).map_err(WalkError::User)?;
            count += 1;
            Ok(core::ops::ControlFlow::Continue(()))
        });

    match walk {
        Ok(core::ops::ControlFlow::Break(())) | Ok(core::ops::ControlFlow::Continue(())) => {}
        Err(WalkError::Dtb(err)) => return Err(SdhcError::DtbParse(err)),
        Err(WalkError::User(err)) => return Err(err),
    }
    if count == 0 {
        return Err(SdhcError::DtbNotFound);
    }
    log_dt_candidates(&candidates[..count]);
    select_sd_slot_candidate(&candidates[..count])
}

fn find_clock_frequency(
    view: &dtb::DtbNodeView<'_, '_>,
    dtb: &DtbParser,
) -> Result<u32, SdhcError> {
    let Some(clocks) = view.property_bytes("clocks").map_err(SdhcError::DtbParse)? else {
        return Ok(SDHC_DEFAULT_MAX_CLOCK_HZ);
    };
    if clocks.len() < 4 {
        return Err(SdhcError::DtbInvalid("sdhc: clocks property too short"));
    }
    let phandle = u32::from_be_bytes(
        clocks[0..4]
            .try_into()
            .map_err(|_| SdhcError::DtbInvalid("sdhc: invalid clocks phandle"))?,
    );
    let maybe_hz = dtb
        .property_u32_be_by_phandle(phandle, "clock-frequency")
        .map_err(SdhcError::DtbParse)?;
    Ok(maybe_hz.unwrap_or(SDHC_DEFAULT_MAX_CLOCK_HZ))
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::vec;
    use alloc::vec::Vec;
    use core::mem::size_of;

    use dtb::DeviceTree;
    use dtb::DeviceTreeEditExt;
    use dtb::DtbParser;
    use dtb::NameRef;
    use dtb::NodeEditExt;
    use dtb::ValueRef;
    use mutex::SpinLock;

    use super::BCM2712_SD_SLOT_HOST_BASE;
    use super::Bcm2712Sdhc;
    use super::CardState;
    use super::CommandFailureSite;
    use super::DT_COMPAT_BCM2712_SDHCI;
    use super::DT_COMPAT_BRCMSTB_SDHCI;
    use super::GpioLineConfig;
    use super::MMC_CMD_READ_SINGLE_BLOCK;
    use super::MMC_CMD_SELECT_CARD;
    use super::MMC_CMD_SEND_STATUS;
    use super::MMC_CMD_WRITE_SINGLE_BLOCK;
    use super::OF_GPIO_ACTIVE_LOW;
    use super::RespType;
    use super::SD_STATUS_READY_FOR_DATA;
    use super::SD_STATUS_TRANSFER_STATE;
    use super::SDHC_BLOCK_SIZE;
    use super::SDHC_CONSERVATIVE_TRANSFER_CLOCK_HZ;
    use super::SDHC_DEFAULT_MIN_CLOCK_HZ;
    use super::SDHC_MAX_TRANSFER_BLOCKS_PER_CMD;
    use super::SDHCI_CTRL_4BITBUS;
    use super::SDHCI_PRESENT_STATE;
    use super::SDHCI_WRITE_PROTECT;
    use super::SDIO_CFG_CTRL_SDCD_N_TEST_EN;
    use super::SDIO_CFG_CTRL_SDCD_N_TEST_LEV;
    use super::SDIO_CFG_SD_PIN_SEL_CARD;
    use super::SDIO_CFG_SD_PIN_SEL_MASK;
    use super::SdhcConfig;
    use super::SdhcDtCandidate;
    use super::SdhcError;
    use super::bcm2712_base_clock_mhz;
    use super::bcm2712_cq_capability_value;
    use super::block_command_arg;
    use super::card_status_current_state;
    use super::card_status_is_transfer_state;
    use super::conservative_chunk_blocks;
    use super::conservative_transfer_clock_hz;
    use super::decode_capacity_blocks;
    use super::encode_clock_divider;
    use super::pack_clock_control_divider;
    use super::parse_from_dtb;
    use super::plan_data_command;
    use super::post_clock_settle_delay_us;
    use super::resolve_sd_slot_bus_width;
    use super::rp1_sdio1_pad_value;
    use super::rp1_sdio1_pin_prep;
    use super::sd_bus_width_setting;
    use super::sdio_cfg_ctrl_real_card_detect;
    use super::sdio_cfg_sd_pin_sel_card;
    use super::select_card_command;
    use super::select_sd_slot_candidate;
    use super::should_retry_response_timeout;

    #[derive(Clone, Copy)]
    struct TestNodeSpec<'a> {
        name: &'a str,
        reg_names: &'a [&'a str],
        regs: &'a [(u64, u64)],
        non_removable: bool,
        wifi_child: bool,
        status: &'a str,
        has_cd_gpio: bool,
        broken_cd: bool,
        bus_width: u32,
        has_brcmstb_compat: bool,
    }

    fn u32_prop(value: u32) -> Vec<u8> {
        value.to_be_bytes().to_vec()
    }

    fn u32_list_prop(values: &[u32]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes
    }

    fn string_prop(value: &str) -> Vec<u8> {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        bytes
    }

    fn string_list(values: &[&str]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend_from_slice(value.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    fn reg_prop(entries: &[(u64, u64)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (addr, size) in entries {
            bytes.extend_from_slice(&((*addr >> 32) as u32).to_be_bytes());
            bytes.extend_from_slice(&(*addr as u32).to_be_bytes());
            bytes.extend_from_slice(&((*size >> 32) as u32).to_be_bytes());
            bytes.extend_from_slice(&(*size as u32).to_be_bytes());
        }
        bytes
    }

    fn compatible_prop(has_brcmstb_compat: bool) -> Vec<u8> {
        if has_brcmstb_compat {
            string_list(&[DT_COMPAT_BCM2712_SDHCI, DT_COMPAT_BRCMSTB_SDHCI])
        } else {
            string_list(&[DT_COMPAT_BCM2712_SDHCI])
        }
    }

    fn set_property<'dtb>(
        tree: &mut dtb::DeviceTree<'dtb, dtb::Owned>,
        node: usize,
        name: &'dtb str,
        value: Vec<u8>,
    ) {
        tree.node_mut(node)
            .unwrap()
            .set_property(NameRef::Borrowed(name), ValueRef::Owned(value));
    }

    fn build_test_dtb(nodes: &[TestNodeSpec<'_>]) -> impl core::ops::Deref<Target = [u8]> {
        let mut tree = DeviceTree::with_root(NameRef::Borrowed("/"));
        tree.header.version = 17;
        tree.header.last_comp_version = 16;

        let root = tree.root;
        set_property(&mut tree, root, "#address-cells", u32_prop(2));
        set_property(&mut tree, root, "#size-cells", u32_prop(2));

        let clocks = tree.add_child(root, NameRef::Borrowed("clocks")).unwrap();
        set_property(&mut tree, clocks, "phandle", u32_prop(1));
        set_property(&mut tree, clocks, "clock-frequency", u32_prop(100_000_000));

        let gpio = tree
            .add_child(root, NameRef::Borrowed("gpio@7d517c00"))
            .unwrap();
        set_property(
            &mut tree,
            gpio,
            "compatible",
            string_list(&["brcm,brcmstb-gpio"]),
        );
        // Exercise the Linux alias together with standard phandles below.
        set_property(&mut tree, gpio, "linux,phandle", u32_prop(0x5a));
        set_property(&mut tree, gpio, "reg", reg_prop(&[(0x10_7d51_7c00, 0x40)]));

        let vqmmc = tree
            .add_child(root, NameRef::Borrowed("sd-io-1v8-reg"))
            .unwrap();
        set_property(
            &mut tree,
            vqmmc,
            "compatible",
            string_list(&["regulator-gpio"]),
        );
        set_property(&mut tree, vqmmc, "phandle", u32_prop(0x0b));
        set_property(
            &mut tree,
            vqmmc,
            "regulator-max-microvolt",
            u32_prop(0x325aa0),
        );
        set_property(
            &mut tree,
            vqmmc,
            "regulator-settling-time-us",
            u32_prop(0x1388),
        );
        set_property(
            &mut tree,
            vqmmc,
            "gpios",
            u32_list_prop(&[0x5a, 0x03, 0x00]),
        );
        set_property(
            &mut tree,
            vqmmc,
            "states",
            u32_list_prop(&[0x1b7740, 0x01, 0x325aa0, 0x00]),
        );

        let vmmc = tree
            .add_child(root, NameRef::Borrowed("sd-vcc-reg"))
            .unwrap();
        set_property(
            &mut tree,
            vmmc,
            "compatible",
            string_list(&["regulator-fixed"]),
        );
        set_property(&mut tree, vmmc, "linux,phandle", u32_prop(0x0c));
        set_property(&mut tree, vmmc, "enable-active-high", Vec::new());
        set_property(&mut tree, vmmc, "gpios", u32_list_prop(&[0x5a, 0x04, 0x00]));

        for spec in nodes {
            let node = tree.add_child(root, NameRef::Borrowed(spec.name)).unwrap();
            set_property(
                &mut tree,
                node,
                "compatible",
                compatible_prop(spec.has_brcmstb_compat),
            );
            set_property(&mut tree, node, "status", string_prop(spec.status));
            set_property(&mut tree, node, "bus-width", u32_prop(spec.bus_width));
            set_property(&mut tree, node, "reg", reg_prop(spec.regs));
            set_property(&mut tree, node, "reg-names", string_list(spec.reg_names));
            set_property(&mut tree, node, "clocks", u32_prop(1));
            set_property(&mut tree, node, "vqmmc-supply", u32_prop(0x0b));
            set_property(&mut tree, node, "vmmc-supply", u32_prop(0x0c));
            if spec.has_cd_gpio {
                set_property(
                    &mut tree,
                    node,
                    "cd-gpios",
                    u32_list_prop(&[0x5a, 0x05, OF_GPIO_ACTIVE_LOW]),
                );
            }
            if spec.broken_cd {
                set_property(&mut tree, node, "broken-cd", Vec::new());
            }

            if spec.non_removable {
                set_property(&mut tree, node, "non-removable", Vec::new());
            }
            if spec.wifi_child {
                let _ = tree.add_child(node, NameRef::Borrowed("wifi@1")).unwrap();
            }
        }

        tree.into_dtb_box().unwrap()
    }

    fn parse_test_config(nodes: &[TestNodeSpec<'_>]) -> Result<SdhcConfig, SdhcError> {
        let dtb = build_test_dtb(nodes);
        let parser = DtbParser::init((&*dtb).as_ptr() as usize).unwrap();
        parse_from_dtb(&parser)
    }

    struct RegisterBackedTestSdhc {
        host_regs: Vec<u32>,
        _cfg_regs: Vec<u32>,
        dev: Bcm2712Sdhc,
    }

    impl RegisterBackedTestSdhc {
        fn new() -> Self {
            let mut host_regs = vec![0u32; 0x100 / size_of::<u32>()];
            let mut cfg_regs = vec![0u32; 0x80 / size_of::<u32>()];
            let dev = Bcm2712Sdhc {
                host: host_regs.as_mut_ptr() as *mut u8,
                cfg: cfg_regs.as_mut_ptr() as *mut u8,
                max_clock_hz: 100_000_000,
                min_clock_hz: SDHC_DEFAULT_MIN_CLOCK_HZ,
                card_detect: None,
                bus_width: 4,
                card: SpinLock::new(None),
            };
            Self {
                host_regs,
                _cfg_regs: cfg_regs,
                dev,
            }
        }

        fn set_host_u32(&mut self, offset: usize, value: u32) {
            assert_eq!(offset % size_of::<u32>(), 0);
            let index = offset / size_of::<u32>();
            self.host_regs[index] = value;
        }

        fn set_card_state(&self, read_only: bool) {
            *self.dev.card.lock() = Some(CardState {
                high_capacity: true,
                capacity_blocks: 4096,
                read_only,
            });
        }
    }

    #[test]
    fn decode_capacity_high_capacity() {
        let csd = [0u32, 0x3f, 0xffff_0000, 0];
        let blocks = decode_capacity_blocks(&csd, true).unwrap();
        assert!(blocks > 0);
    }

    #[test]
    fn card_read_only_matches_linux_default_path_when_wp_bit_clear() {
        let mut harness = RegisterBackedTestSdhc::new();
        harness.set_host_u32(SDHCI_PRESENT_STATE, 0);
        assert!(harness.dev.card_read_only());
    }

    #[test]
    fn card_read_only_matches_linux_default_path_when_wp_bit_set() {
        let mut harness = RegisterBackedTestSdhc::new();
        harness.set_host_u32(SDHCI_PRESENT_STATE, SDHCI_WRITE_PROTECT);
        assert!(!harness.dev.card_read_only());
    }

    #[test]
    fn block_device_is_read_only_reflects_stored_card_state_after_wp_sample() {
        let mut harness = RegisterBackedTestSdhc::new();
        harness.set_host_u32(SDHCI_PRESENT_STATE, SDHCI_WRITE_PROTECT);
        harness.set_card_state(harness.dev.card_read_only());
        assert!(
            !<Bcm2712Sdhc as io_api::block_device::BlockDevice>::is_read_only(&harness.dev)
                .unwrap()
        );
    }

    #[test]
    fn decode_capacity_standard_capacity() {
        let csd = [0, 0x03ff_0000, 0xc003_8000, 0];
        let blocks = decode_capacity_blocks(&csd, false).unwrap();
        assert!(blocks > 0);
    }

    #[test]
    fn chunk_limit_matches_expected_max_io() {
        assert_eq!(SDHC_MAX_TRANSFER_BLOCKS_PER_CMD, 1024);
        assert_eq!(
            SDHC_MAX_TRANSFER_BLOCKS_PER_CMD * SDHC_BLOCK_SIZE,
            512 * 1024
        );
    }

    #[test]
    fn invalid_param_maps_to_io_invalid_param() {
        let mapped = io_api::block_device::IoError::from(SdhcError::InvalidParam);
        assert_eq!(mapped, io_api::block_device::IoError::InvalidParam);
    }

    #[test]
    fn dt_selector_prefers_sd_slot_when_both_mmc_nodes_exist() {
        let wifi_regs = &[(0x10_0110_0000, 0x104), (0x10_0110_1000, 0x80)];
        let slot_regs = &[
            (BCM2712_SD_SLOT_HOST_BASE as u64, 0x104),
            (0x10_00ff_f200, 0x80),
        ];
        let nodes = [
            TestNodeSpec {
                name: "mmc@1100000",
                reg_names: &["host", "cfg"],
                regs: wifi_regs,
                non_removable: true,
                wifi_child: true,
                status: "okay",
                has_cd_gpio: false,
                broken_cd: false,
                bus_width: 4,
                has_brcmstb_compat: true,
            },
            TestNodeSpec {
                name: "mmc@fff000",
                reg_names: &["host", "cfg"],
                regs: slot_regs,
                non_removable: false,
                wifi_child: false,
                status: "okay",
                has_cd_gpio: true,
                broken_cd: false,
                bus_width: 4,
                has_brcmstb_compat: true,
            },
        ];

        let config = parse_test_config(&nodes).unwrap();
        assert_eq!(config.host.base, BCM2712_SD_SLOT_HOST_BASE);
        assert_eq!(config.cfg.base, 0x10_00ff_f200);
        assert_eq!(config.power.vqmmc_select.unwrap().line.base, 0x10_7d51_7c00);
        assert_eq!(config.power.vqmmc_select.unwrap().line.pin, 3);
        assert!(!config.power.vqmmc_select.unwrap().logical_high);
        assert_eq!(config.power.vqmmc_settle_us, 0x1388);
        assert_eq!(config.power.vmmc_enable.unwrap().line.pin, 4);
        assert!(config.power.vmmc_enable.unwrap().logical_high);
        assert_eq!(
            config.card_detect,
            Some(GpioLineConfig {
                base: 0x10_7d51_7c00,
                pin: 5,
                active_low: true,
            })
        );
        assert_eq!(config.bus_width, 4);
    }

    #[test]
    fn wifi_candidate_without_slot_host_base_is_rejected() {
        let candidates = [SdhcDtCandidate {
            host: Some(super::HostRegion {
                base: 0x10_0110_0000,
                size: 0x104,
            }),
            cfg: Some(super::HostRegion {
                base: 0x10_0110_1000,
                size: 0x80,
            }),
            busisol: None,
            lcpll: None,
            max_clock_hz: 100_000_000,
            power: super::SdhcPowerConfig::default(),
            card_detect: None,
            broken_cd: false,
            has_brcmstb_compat: true,
            status_okay: true,
            bus_width: 4,
            non_removable: true,
            has_wifi_child: true,
        }];

        let err = select_sd_slot_candidate(&candidates).unwrap_err();
        assert_eq!(
            err,
            SdhcError::DtbInvalid("sdhc: no valid SD-slot candidate")
        );
    }

    #[test]
    fn selected_sd_slot_candidate_without_cd_gpios_is_rejected() {
        let regs = &[
            (BCM2712_SD_SLOT_HOST_BASE as u64, 0x104),
            (0x10_00ff_f200, 0x80),
        ];
        let nodes = [TestNodeSpec {
            name: "mmc@fff000",
            reg_names: &["host", "cfg"],
            regs,
            non_removable: false,
            wifi_child: false,
            status: "okay",
            has_cd_gpio: false,
            broken_cd: false,
            bus_width: 4,
            has_brcmstb_compat: false,
        }];

        let err = parse_test_config(&nodes).unwrap_err();
        assert_eq!(
            err,
            SdhcError::DtbInvalid("sdhc: missing cd-gpios for SD slot")
        );
    }

    #[test]
    fn selected_sd_slot_candidate_with_unsupported_bus_width_is_rejected() {
        let regs = &[
            (BCM2712_SD_SLOT_HOST_BASE as u64, 0x104),
            (0x10_00ff_f200, 0x80),
        ];
        let nodes = [TestNodeSpec {
            name: "mmc@fff000",
            reg_names: &["host", "cfg"],
            regs,
            non_removable: false,
            wifi_child: false,
            status: "okay",
            has_cd_gpio: true,
            broken_cd: false,
            bus_width: 8,
            has_brcmstb_compat: false,
        }];

        let err = parse_test_config(&nodes).unwrap_err();
        assert_eq!(
            err,
            SdhcError::DtbInvalid("sdhc: unsupported bus-width for SD slot")
        );
    }

    #[test]
    fn reg_names_mapping_is_independent_of_reg_order() {
        let regs = &[
            (0x10_00ff_f200, 0x80),
            (BCM2712_SD_SLOT_HOST_BASE as u64, 0x104),
        ];
        let nodes = [TestNodeSpec {
            name: "mmc@fff000",
            reg_names: &["cfg", "host"],
            regs,
            non_removable: false,
            wifi_child: false,
            status: "okay",
            has_cd_gpio: true,
            broken_cd: false,
            bus_width: 4,
            has_brcmstb_compat: true,
        }];

        let config = parse_test_config(&nodes).unwrap();
        assert_eq!(config.host.base, BCM2712_SD_SLOT_HOST_BASE);
        assert_eq!(config.cfg.base, 0x10_00ff_f200);
    }

    #[test]
    fn ambiguous_structurally_valid_candidates_fail() {
        let candidates = [
            SdhcDtCandidate {
                host: Some(super::HostRegion {
                    base: 0x10_0200_0000,
                    size: 0x104,
                }),
                cfg: Some(super::HostRegion {
                    base: 0x10_0200_1000,
                    size: 0x80,
                }),
                busisol: None,
                lcpll: None,
                max_clock_hz: 100_000_000,
                power: super::SdhcPowerConfig::default(),
                card_detect: Some(GpioLineConfig {
                    base: 0x10_7d51_7c00,
                    pin: 5,
                    active_low: true,
                }),
                broken_cd: false,
                has_brcmstb_compat: true,
                status_okay: true,
                bus_width: 4,
                non_removable: false,
                has_wifi_child: false,
            },
            SdhcDtCandidate {
                host: Some(super::HostRegion {
                    base: 0x10_0300_0000,
                    size: 0x104,
                }),
                cfg: Some(super::HostRegion {
                    base: 0x10_0300_1000,
                    size: 0x80,
                }),
                busisol: None,
                lcpll: None,
                max_clock_hz: 100_000_000,
                power: super::SdhcPowerConfig::default(),
                card_detect: Some(GpioLineConfig {
                    base: 0x10_7d51_7c00,
                    pin: 5,
                    active_low: true,
                }),
                broken_cd: false,
                has_brcmstb_compat: true,
                status_okay: true,
                bus_width: 4,
                non_removable: false,
                has_wifi_child: false,
            },
        ];

        let err = select_sd_slot_candidate(&candidates).unwrap_err();
        assert_eq!(
            err,
            SdhcError::DtbInvalid("sdhc: ambiguous DT candidate selection")
        );
    }

    #[test]
    fn rp1_sdio1_pins_use_bank_one_offsets() {
        let clk = rp1_sdio1_pin_prep(28).unwrap();
        let cmd = rp1_sdio1_pin_prep(29).unwrap();
        let dat3 = rp1_sdio1_pin_prep(33).unwrap();

        assert_eq!(clk.ctrl_offset, 0x4004);
        assert_eq!(clk.pad_offset, 0x4004);
        assert_eq!(cmd.ctrl_offset, 0x400c);
        assert_eq!(cmd.pad_offset, 0x4008);
        assert_eq!(dat3.ctrl_offset, 0x402c);
        assert_eq!(dat3.pad_offset, 0x4018);
    }

    #[test]
    fn rp1_sdio1_pads_match_expected_pull_configuration() {
        assert_eq!(rp1_sdio1_pad_value(28), Some(0x73));
        assert_eq!(rp1_sdio1_pad_value(29), Some(0x7b));
        assert_eq!(rp1_sdio1_pad_value(33), Some(0x7b));
        assert_eq!(rp1_sdio1_pad_value(27), None);
    }

    #[test]
    fn divider_encoding_returns_zero_when_requested_clock_meets_or_exceeds_max() {
        assert_eq!(encode_clock_divider(100_000_000, 100_000_000).unwrap(), 0);
        assert_eq!(encode_clock_divider(150_000_000, 100_000_000).unwrap(), 0);
    }

    #[test]
    fn divider_packing_uses_low_and_high_bits() {
        let encoded = encode_clock_divider(100_000, 200_000_000).unwrap();
        assert_eq!(encoded, 1000);
        assert_eq!(pack_clock_control_divider(encoded), 0xe8c0);
    }

    #[test]
    fn broken_cd_eight_bit_dt_width_maps_to_four_bit_sd_runtime() {
        assert_eq!(resolve_sd_slot_bus_width(8, true).unwrap(), 4);
        assert_eq!(
            resolve_sd_slot_bus_width(8, false),
            Err(SdhcError::DtbInvalid(
                "sdhc: unsupported bus-width for SD slot"
            ))
        );
    }

    #[test]
    fn broken_cd_candidate_without_gpio_is_accepted() {
        let regs = &[
            (BCM2712_SD_SLOT_HOST_BASE as u64, 0x260),
            (0x10_00ff_f400, 0x200),
        ];
        let nodes = [TestNodeSpec {
            name: "mmc@fff000",
            reg_names: &["host", "cfg"],
            regs,
            non_removable: false,
            wifi_child: false,
            status: "okay",
            has_cd_gpio: false,
            broken_cd: true,
            bus_width: 8,
            has_brcmstb_compat: true,
        }];

        let config = parse_test_config(&nodes).unwrap();
        assert_eq!(config.host.base, BCM2712_SD_SLOT_HOST_BASE);
        assert_eq!(config.cfg.base, 0x10_00ff_f400);
        assert_eq!(config.card_detect, None);
        assert_eq!(config.bus_width, 4);
    }

    #[test]
    fn sd_bus_width_settings_map_to_acmd6_and_host_control_bits() {
        let one_bit = sd_bus_width_setting(1).unwrap();
        assert_eq!(one_bit.acmd6_arg, 0);
        assert_eq!(one_bit.host_control_bits, 0);

        let four_bit = sd_bus_width_setting(4).unwrap();
        assert_eq!(four_bit.acmd6_arg, 2);
        assert_eq!(four_bit.host_control_bits, SDHCI_CTRL_4BITBUS);

        assert_eq!(sd_bus_width_setting(0), Err(SdhcError::Unsupported));
        assert_eq!(sd_bus_width_setting(8), Err(SdhcError::Unsupported));
    }

    #[test]
    fn real_card_detect_programming_disables_fake_detect_and_routes_sd_slot() {
        let ctrl = sdio_cfg_ctrl_real_card_detect(
            0x55aa_0000 | SDIO_CFG_CTRL_SDCD_N_TEST_EN | SDIO_CFG_CTRL_SDCD_N_TEST_LEV,
        );
        assert_eq!(ctrl & SDIO_CFG_CTRL_SDCD_N_TEST_EN, 0);
        assert_eq!(ctrl & SDIO_CFG_CTRL_SDCD_N_TEST_LEV, 0);

        let pin_sel = sdio_cfg_sd_pin_sel_card(0xffff_ffff);
        assert_eq!(pin_sel & SDIO_CFG_SD_PIN_SEL_MASK, SDIO_CFG_SD_PIN_SEL_CARD);
    }

    #[test]
    fn cmd7_select_card_uses_r1b() {
        let command = select_card_command(0x1234);
        assert_eq!(command.idx, MMC_CMD_SELECT_CARD);
        assert_eq!(command.arg, 0x1234_0000);
        assert_eq!(command.resp_type, RespType::R1b);
    }

    #[test]
    fn conservative_chunk_policy_is_single_block() {
        assert_eq!(conservative_chunk_blocks(1).unwrap(), 1);
        assert_eq!(conservative_chunk_blocks(8).unwrap(), 1);
    }

    #[test]
    fn conservative_transfer_clock_is_12_5mhz() {
        assert_eq!(
            conservative_transfer_clock_hz(),
            SDHC_CONSERVATIVE_TRANSFER_CLOCK_HZ
        );
        assert_eq!(conservative_transfer_clock_hz(), 12_500_000);
        assert!((10..=100).contains(&post_clock_settle_delay_us()));
    }

    #[test]
    fn bcm2712_cq_capability_uses_mhz_base_clock_estimate() {
        assert_eq!(bcm2712_base_clock_mhz(100_000_000), 100);
        assert_eq!(bcm2712_base_clock_mhz(999_999), 1);
        assert_eq!(bcm2712_cq_capability_value(100_000_000), 0x3064);
    }

    #[test]
    fn high_capacity_argument_uses_lba() {
        assert_eq!(block_command_arg(7, true).unwrap(), 7);
    }

    #[test]
    fn standard_capacity_argument_uses_byte_address() {
        assert_eq!(
            block_command_arg(7, false).unwrap(),
            7 * SDHC_BLOCK_SIZE as u32
        );
    }

    #[test]
    fn multi_sector_reads_are_planned_as_repeated_single_block_commands() {
        let mut lba = 0x40;
        let mut remaining_blocks = 3usize;
        let mut args = Vec::new();

        while remaining_blocks > 0 {
            let plan = plan_data_command(lba, remaining_blocks, false, true).unwrap();
            assert_eq!(plan.cmd_idx, MMC_CMD_READ_SINGLE_BLOCK);
            assert_eq!(plan.block_count, 1);
            args.push(plan.arg);
            lba += plan.block_count as u64;
            remaining_blocks -= plan.block_count;
        }

        assert_eq!(args, vec![0x40, 0x41, 0x42]);
    }

    #[test]
    fn multi_sector_writes_are_planned_as_repeated_single_block_commands() {
        let mut lba = 2u64;
        let mut remaining_blocks = 3usize;
        let mut args = Vec::new();

        while remaining_blocks > 0 {
            let plan = plan_data_command(lba, remaining_blocks, true, false).unwrap();
            assert_eq!(plan.cmd_idx, MMC_CMD_WRITE_SINGLE_BLOCK);
            assert_eq!(plan.block_count, 1);
            args.push(plan.arg);
            lba += plan.block_count as u64;
            remaining_blocks -= plan.block_count;
        }

        assert_eq!(
            args,
            vec![
                2 * SDHC_BLOCK_SIZE as u32,
                3 * SDHC_BLOCK_SIZE as u32,
                4 * SDHC_BLOCK_SIZE as u32
            ]
        );
    }

    #[test]
    fn transfer_state_helper_decodes_cmd13_state() {
        let status = SD_STATUS_READY_FOR_DATA | (u32::from(SD_STATUS_TRANSFER_STATE) << 9);

        assert_eq!(card_status_current_state(status), SD_STATUS_TRANSFER_STATE);
        assert!(card_status_is_transfer_state(status));
        assert!(!card_status_is_transfer_state(0));
    }

    #[test]
    fn retry_policy_is_limited_to_single_block_data_commands() {
        assert!(should_retry_response_timeout(
            MMC_CMD_READ_SINGLE_BLOCK,
            CommandFailureSite::ResponseCompletionWait,
            SdhcError::Timeout,
        ));
        assert!(should_retry_response_timeout(
            MMC_CMD_WRITE_SINGLE_BLOCK,
            CommandFailureSite::ResponseCompletionStatus,
            SdhcError::Timeout,
        ));
        assert!(!should_retry_response_timeout(
            MMC_CMD_SEND_STATUS,
            CommandFailureSite::ResponseCompletionStatus,
            SdhcError::Timeout,
        ));
        assert!(!should_retry_response_timeout(
            MMC_CMD_READ_SINGLE_BLOCK,
            CommandFailureSite::ResponseCompletionWait,
            SdhcError::Io,
        ));
    }
}
