use crate::bcm2712::Bcm2712Error;
use crate::bcm2712::rp1_interrupt;
use common::PirqHookError;
use common::PirqHookOp;
use common::PirqLifecycleHook;
use common::TriggerMode;
use core::cell::SyncUnsafeCell;
use core::mem::size_of;
use core::ptr::read_volatile;
use core::ptr::write_volatile;
use core::sync::atomic::Ordering;
use mutex::pod::RawAtomicPod;

const MIP_SPI_OFFSET: u32 = 128;
pub const RP1_MSIX_SPI_START: u32 = MIP_SPI_OFFSET + 32;
const RP1_PCIE_CFG_OFFSET: usize = 0x10_8000;
const RP1_PCIE_CFG_REG_SET: usize = 0x800;
const RP1_PCIE_CFG_REG_CLR: usize = 0xc00;
const RP1_PCIE_MSIX_CFG_OFFSET: usize = 0x08;
const RP1_PCIE_CFG_LEN: usize = 64;
const RP1_MSIX_CFG_ENABLE: u32 = 1 << 0;
const RP1_MSIX_CFG_IACK: u32 = 1 << 2;
const RP1_MSIX_CFG_IACK_EN: u32 = 1 << 3;
pub const GUEST_RP1_PASSTHROUGH_MSIX_INDICES: [usize; 9] = [0, 1, 2, 7, 13, 25, 47, 48, 58];
pub const GUEST_RP1_PASSTHROUGH_SPIS: [u32; 9] = [160, 161, 162, 167, 173, 185, 207, 208, 218];
pub const RP1_UART0_MSIX_INDEX: usize = 25;
pub const RP1_UART0_SPI: u32 = RP1_MSIX_SPI_START + RP1_UART0_MSIX_INDEX as u32;
const RP1_UART0_OFFSET: usize = 0x3_0000;
const RP1_PL011_WINDOW_SIZE: usize = 0x1000;
const PL011_MIS_OFFSET: usize = 0x40;
const PL011_ICR_OFFSET: usize = 0x44;

static RP1_PERIPHERAL_BASE: SyncUnsafeCell<Option<usize>> = SyncUnsafeCell::new(None);
static RP1_LEVEL_MSIX_SOURCES: RawAtomicPod<u64> = unsafe { RawAtomicPod::new_raw_unchecked(0) };
static RP1_ENABLED_MSIX_SOURCES: RawAtomicPod<u64> = unsafe { RawAtomicPod::new_raw_unchecked(0) };
static RP1_RESAMPLE_INFLIGHT_MSIX_SOURCES: RawAtomicPod<u64> =
    unsafe { RawAtomicPod::new_raw_unchecked(0) };

struct Rp1LevelMsixSource {
    int_id: u32,
    index: usize,
    mmio_offset: usize,
    mmio_size: usize,
    is_completion_access: fn(usize, bool) -> bool,
    is_asserted: fn(usize) -> bool,
}

const RP1_LEVEL_MSIX_SOURCES_TO_RESAMPLE: [Rp1LevelMsixSource; 1] = [Rp1LevelMsixSource {
    int_id: RP1_UART0_SPI,
    index: RP1_UART0_MSIX_INDEX,
    mmio_offset: RP1_UART0_OFFSET,
    mmio_size: RP1_PL011_WINDOW_SIZE,
    is_completion_access: rp1_pl011_completion_access,
    is_asserted: rp1_uart0_asserted,
}];

/// # Safety
/// The caller must ensure there is no concurrent access while updating this global state.
/// `Some(base)` must point to a valid, mapped RP1 peripheral MMIO window.
pub(crate) unsafe fn set_rp1_peripheral_base(base: Option<usize>) {
    // SAFETY: Guaranteed by the caller contract above.
    unsafe { *RP1_PERIPHERAL_BASE.get() = base };
}

pub(crate) fn rp1_peripheral_base() -> Option<usize> {
    // SAFETY: RP1 base configuration is written during initialization and then treated as
    // immutable runtime configuration; readers copy out the `Option<usize>` by value.
    unsafe { *RP1_PERIPHERAL_BASE.get() }
}

fn map_bcm2712_error(err: Bcm2712Error) -> PirqHookError {
    match err {
        Bcm2712Error::InvalidWindow
        | Bcm2712Error::InvalidPciHeaderType
        | Bcm2712Error::InvalidSettings => PirqHookError::InvalidInput,
        Bcm2712Error::DtbParseError(_)
        | Bcm2712Error::DtbDeviceNotFound
        | Bcm2712Error::PcieIsNotInitialized
        | Bcm2712Error::MdioTimeout
        | Bcm2712Error::LinkTimeout
        | Bcm2712Error::PcieEndpointNotFound
        | Bcm2712Error::UnexpectedDevice(_) => PirqHookError::InvalidState,
    }
}

fn msix_index_from_intid(int_id: u32) -> Result<usize, PirqHookError> {
    let offset = int_id
        .checked_sub(RP1_MSIX_SPI_START)
        .ok_or(PirqHookError::InvalidInput)?;
    let index = usize::try_from(offset).map_err(|_| PirqHookError::InvalidInput)?;
    if index >= RP1_PCIE_CFG_LEN {
        return Err(PirqHookError::InvalidInput);
    }
    Ok(index)
}

fn msix_source_bit(index: usize) -> Result<u64, PirqHookError> {
    if index >= u64::BITS as usize {
        return Err(PirqHookError::InvalidInput);
    }
    Ok(1u64 << index)
}

fn rp1_msix_cfg_alias(index: usize, alias_offset: usize) -> Result<*mut u32, PirqHookError> {
    // SAFETY: The base is written during RP1 init and then treated as immutable runtime
    // configuration while hooks are active.
    let peripheral_base =
        unsafe { *RP1_PERIPHERAL_BASE.get() }.ok_or(PirqHookError::InvalidState)?;
    peripheral_base
        .checked_add(RP1_PCIE_CFG_OFFSET)
        .and_then(|base| base.checked_add(alias_offset))
        .and_then(|base| base.checked_add(RP1_PCIE_MSIX_CFG_OFFSET))
        .and_then(|base| base.checked_add(index.checked_mul(size_of::<u32>())?))
        .map(|addr| addr as *mut u32)
        .ok_or(PirqHookError::InvalidState)
}

fn rp1_msix_cfg_set(index: usize, bits: u32) -> Result<(), PirqHookError> {
    let addr = rp1_msix_cfg_alias(index, RP1_PCIE_CFG_REG_SET)?;
    // SAFETY: `addr` points at the RP1 PCIE_CFG SET alias for a bounds-checked MSI-X source.
    unsafe { write_volatile(addr, bits) };
    Ok(())
}

fn rp1_msix_cfg_clear(index: usize, bits: u32) -> Result<(), PirqHookError> {
    let addr = rp1_msix_cfg_alias(index, RP1_PCIE_CFG_REG_CLR)?;
    // SAFETY: `addr` points at the RP1 PCIE_CFG CLR alias for a bounds-checked MSI-X source.
    unsafe { write_volatile(addr, bits) };
    Ok(())
}

fn rp1_msix_iack_index(index: usize) -> Result<(), PirqHookError> {
    rp1_msix_cfg_set(index, RP1_MSIX_CFG_IACK)
}

fn rp1_msix_iack_level_source(int_id: u32) -> Result<(), PirqHookError> {
    let index = msix_index_from_intid(int_id)?;
    let bit = msix_source_bit(index)?;
    let level_sources = RP1_LEVEL_MSIX_SOURCES.load(Ordering::Acquire);
    let enabled_sources = RP1_ENABLED_MSIX_SOURCES.load(Ordering::Acquire);
    if (level_sources & enabled_sources & bit) != 0 {
        rp1_msix_iack_index(index)?;
        RP1_RESAMPLE_INFLIGHT_MSIX_SOURCES.fetch_and(!bit, Ordering::AcqRel);
    }
    Ok(())
}

fn rp1_uart0_asserted(peripheral_base: usize) -> bool {
    let Some(mis_addr) = peripheral_base
        .checked_add(RP1_UART0_OFFSET)
        .and_then(|base| base.checked_add(PL011_MIS_OFFSET))
    else {
        return false;
    };
    // SAFETY: The RP1 peripheral base is captured from DT-backed RP1 init. PL011 MIS is a
    // naturally aligned, read-only interrupt status register.
    unsafe { read_volatile(mis_addr as *const u32) != 0 }
}

fn rp1_pl011_completion_access(offset: usize, is_write: bool) -> bool {
    !is_write || offset == PL011_ICR_OFFSET
}

fn rp1_msix_arm_level_source(int_id: u32, index: usize, bit: u64) -> Result<(), PirqHookError> {
    RP1_LEVEL_MSIX_SOURCES.fetch_or(bit, Ordering::AcqRel);
    rp1_msix_cfg_set(index, RP1_MSIX_CFG_ENABLE | RP1_MSIX_CFG_IACK_EN)?;
    rp1_interrupt::enable_interrupt(int_id).map_err(map_bcm2712_error)?;
    RP1_ENABLED_MSIX_SOURCES.fetch_or(bit, Ordering::AcqRel);
    Ok(())
}

fn rp1_msix_reissue_level_source(source: Rp1LevelMsixSource) -> Result<(), PirqHookError> {
    let bit = msix_source_bit(source.index)?;
    rp1_msix_arm_level_source(source.int_id, source.index, bit)?;
    rp1_msix_iack_index(source.index)?;
    RP1_RESAMPLE_INFLIGHT_MSIX_SOURCES.fetch_and(!bit, Ordering::AcqRel);
    Ok(())
}

/// Completes RP1 level MSI-X handling after the guest has touched a child peripheral MMIO window.
///
/// The exception handler reports generic passthrough accesses; the RP1 hook decides whether that
/// access corresponds to a source-specific completion point that should IACK and re-arm a source.
pub fn after_passthrough_mmio_access(addr: usize, is_write: bool) -> Result<(), PirqHookError> {
    let peripheral_base = rp1_peripheral_base().ok_or(PirqHookError::InvalidState)?;
    for source in RP1_LEVEL_MSIX_SOURCES_TO_RESAMPLE {
        let Some(source_base) = peripheral_base.checked_add(source.mmio_offset) else {
            continue;
        };
        let Some(source_end) = source_base.checked_add(source.mmio_size) else {
            continue;
        };
        if !(source_base..source_end).contains(&addr) {
            continue;
        }
        let offset = addr - source_base;
        if (source.is_completion_access)(offset, is_write) {
            rp1_msix_reissue_level_source(source)?;
        }
    }
    Ok(())
}

/// Re-issues RP1 level MSI-X sources whose child peripheral line is still asserted.
///
/// RP1 level sources are delivered to the host GIC as MSI-X edges. With `IACK_EN`, RP1 masks a
/// source after the first edge and emits a new edge only after IACK if the child line remains high.
/// This helper lets the board IRQ path ask the RP1 hook to perform that source-level resample
/// without embedding UART-specific MMIO knowledge in the exception handler.
pub fn resample_level_sources(skip_int_id: Option<u32>) -> Result<(), PirqHookError> {
    let peripheral_base = rp1_peripheral_base().ok_or(PirqHookError::InvalidState)?;
    for source in RP1_LEVEL_MSIX_SOURCES_TO_RESAMPLE {
        if skip_int_id == Some(source.int_id) {
            continue;
        }

        let bit = msix_source_bit(source.index)?;
        if !(source.is_asserted)(peripheral_base) {
            RP1_RESAMPLE_INFLIGHT_MSIX_SOURCES.fetch_and(!bit, Ordering::AcqRel);
            continue;
        }

        if RP1_RESAMPLE_INFLIGHT_MSIX_SOURCES.fetch_or(bit, Ordering::AcqRel) & bit != 0 {
            continue;
        }

        if let Err(err) = rp1_msix_reissue_level_source(source) {
            RP1_RESAMPLE_INFLIGHT_MSIX_SOURCES.fetch_and(!bit, Ordering::AcqRel);
            return Err(err);
        }
    }

    Ok(())
}

fn rp1_msix_configure_source(
    int_id: u32,
    trigger: TriggerMode,
    enable: bool,
) -> Result<(), PirqHookError> {
    let index = msix_index_from_intid(int_id)?;
    let bit = msix_source_bit(index)?;

    if !enable {
        rp1_interrupt::disable_interrupt(int_id).map_err(map_bcm2712_error)?;
        RP1_ENABLED_MSIX_SOURCES.fetch_and(!bit, Ordering::AcqRel);
        rp1_msix_cfg_clear(index, RP1_MSIX_CFG_ENABLE | RP1_MSIX_CFG_IACK_EN)?;
        return Ok(());
    }

    match trigger {
        TriggerMode::Level => {
            rp1_msix_arm_level_source(int_id, index, bit)?;
        }
        TriggerMode::Edge => {
            RP1_LEVEL_MSIX_SOURCES.fetch_and(!bit, Ordering::AcqRel);
            rp1_msix_cfg_clear(index, RP1_MSIX_CFG_IACK_EN)?;
            rp1_msix_cfg_set(index, RP1_MSIX_CFG_ENABLE)?;
            rp1_interrupt::enable_interrupt(int_id).map_err(map_bcm2712_error)?;
            RP1_ENABLED_MSIX_SOURCES.fetch_or(bit, Ordering::AcqRel);
        }
    }

    rp1_msix_iack_index(index)
}

pub fn is_guest_rp1_passthrough_spi(int_id: u32) -> bool {
    GUEST_RP1_PASSTHROUGH_SPIS.contains(&int_id)
}

struct Bcm2712PirqLifecycleHook;

impl PirqLifecycleHook for Bcm2712PirqLifecycleHook {
    fn on_pirq_event(&self, int_id: u32, op: PirqHookOp) -> Result<(), PirqHookError> {
        if !is_guest_rp1_passthrough_spi(int_id) {
            return Ok(());
        }

        match op {
            PirqHookOp::Configure {
                trigger, enable, ..
            } => rp1_msix_configure_source(int_id, trigger, enable),
            PirqHookOp::Eoi => rp1_msix_iack_level_source(int_id),
            PirqHookOp::Deactivate | PirqHookOp::Resample => Ok(()),
        }
    }
}

static BCM2712_PIRQ_LIFECYCLE_HOOK: Bcm2712PirqLifecycleHook = Bcm2712PirqLifecycleHook;

/// RP1 pIRQ lifecycle callback used by the virtual GIC.
pub static PIRQ_LIFECYCLE_HOOK: &dyn PirqLifecycleHook = &BCM2712_PIRQ_LIFECYCLE_HOOK;
