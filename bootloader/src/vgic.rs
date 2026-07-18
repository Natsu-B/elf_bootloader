use arch_hal::cpu;
use arch_hal::gic::GicError;
use arch_hal::gic::GicIrqMirror;
use arch_hal::gic::GicPpi;
use arch_hal::gic::GicSgi;
use arch_hal::gic::PIntId;
use arch_hal::gic::PhysicalIrqBindingKind;
use arch_hal::gic::SgiTarget;
use arch_hal::gic::VIntId;
use arch_hal::gic::VcpuId;
use arch_hal::gic::VgicHw;
use arch_hal::gic::gicv2::Gicv2;
use arch_hal::gic::gicv2::Gicv2AccessSize;
use arch_hal::gic::gicv2::Gicv2DistIdRegs;
use arch_hal::gic::vm::PirqHookError;
use arch_hal::gic::vm::PirqHookOp;
use arch_hal::gic::vm::PirqLifecycleHook;
use arch_hal::gic::vm::manager::VgicDelegate;
use arch_hal::gic::vm::manager::VgicManager;
use arch_hal::println;
use core::cell::SyncUnsafeCell;

use crate::Gicv2Info;
use crate::UartNode;
use crate::handler;
use crate::irq_monitor;

struct BootVgicDelegate;

const KICK_SGI_ID: u8 = 15;
const KICK_INTID: u32 = KICK_SGI_ID as u32;
const GUEST_EL1_VTIMER_PPI_INTID: u32 = 27;

static DELEGATE: BootVgicDelegate = BootVgicDelegate;
static VGIC: VgicManager<1> = VgicManager::new(&DELEGATE, 0);
static GICD_RANGE: SyncUnsafeCell<Option<(usize, usize)>> = SyncUnsafeCell::new(None);
static MAINT_INTID: SyncUnsafeCell<Option<u32>> = SyncUnsafeCell::new(None);
static GICD_ID: SyncUnsafeCell<Option<Gicv2DistIdRegs>> = SyncUnsafeCell::new(None);

impl VgicDelegate for BootVgicDelegate {
    fn irq_mirror(&self) -> Result<&'static dyn GicIrqMirror, GicError> {
        handler::gic()
            .ok_or(GicError::InvalidState)
            .map(|gic| gic as &'static dyn GicIrqMirror)
    }

    fn get_resident_affinity(
        &self,
        _vm_id: usize,
        _vcpu_id: u16,
    ) -> Result<Option<cpu::CoreAffinity>, GicError> {
        Ok(Some(cpu::get_current_core_id()))
    }

    fn get_home_affinity(
        &self,
        _vm_id: usize,
        _vcpu_id: u16,
    ) -> Result<cpu::CoreAffinity, GicError> {
        Ok(cpu::get_current_core_id())
    }

    fn get_current_vcpu(&self, _vm_id: usize) -> Result<VcpuId, GicError> {
        Ok(VcpuId(0))
    }

    fn kick_pcpu(&self, target: cpu::CoreAffinity) -> Result<(), GicError> {
        let gic = handler::gic().ok_or(GicError::InvalidState)?;
        let targets = [target];
        gic.send_sgi(KICK_SGI_ID, SgiTarget::Specific(&targets))
    }
}

fn enable_guest_ppis(gic: &Gicv2) -> Result<(), GicError> {
    for ppi in 16..32 {
        if matches!(ppi, 25 | 26 | 29) {
            continue;
        }
        gic.set_ppi_enable(ppi, true)?;
    }
    Ok(())
}

struct BootPirqLifecycleHook;

impl PirqLifecycleHook for BootPirqLifecycleHook {
    fn on_pirq_event(&self, int_id: u32, op: PirqHookOp) -> Result<(), PirqHookError> {
        match op {
            PirqHookOp::Eoi => irq_monitor::record_pirq_eoi(int_id),
            PirqHookOp::Deactivate => irq_monitor::record_pirq_deactivate(int_id),
            PirqHookOp::Configure { .. } | PirqHookOp::Resample => {}
        }
        Ok(())
    }
}

static BOOT_PIRQ_LIFECYCLE_HOOK: BootPirqLifecycleHook = BootPirqLifecycleHook;

pub(crate) fn init(
    gic: &Gicv2,
    info: &Gicv2Info,
    guest_uart: UartNode,
    gdb_uart_intid: Option<u32>,
) -> Result<(), &'static str> {
    gic.hw_init().map_err(|_| "vgic: hw init failed")?;
    enable_guest_ppis(gic).map_err(|_| "vgic: enable guest ppis")?;

    // SAFETY: Boot path initializes vGIC once on the BSP before exposing concurrent access.
    // `VGIC` is a static manager, so model storage address remains stable for program lifetime.
    unsafe { VGIC.init_from_gicv2(gic, 1) }.map_err(|_| "vgic: create vm failed")?;

    VGIC.switch_in(gic, VcpuId(0), cpu::get_current_core_id())
        .map_err(|_| "vgic: switch in")?;

    VGIC.bind_local_pirq_write_through_software_lr(
        gic,
        PIntId(GUEST_EL1_VTIMER_PPI_INTID),
        VcpuId(0),
        VIntId(GUEST_EL1_VTIMER_PPI_INTID),
    )
    .map_err(|e| {
        println!(
            "Failed to bind guest timer PIRQ {}: {:?}",
            GUEST_EL1_VTIMER_PPI_INTID, e
        );
        "vgic: map guest timer pirq"
    })?;

    if let Some(pintid) = guest_uart.irq {
        if Some(pintid) != gdb_uart_intid {
            VGIC.bind_spi_pirq_passthrough(gic, PIntId(pintid), VIntId(pintid))
                .map_err(|e| {
                    println!("Failed to map guest UART PIRQ {}: {:?}", pintid, e);
                    "vgic: map pirq"
                })?;
        }
    }

    let max_intid = gic.max_intid();
    for intid in 32..max_intid {
        if Some(intid) == gdb_uart_intid || guest_uart.irq == Some(intid) {
            continue;
        }
        match VGIC.bind_spi_pirq_passthrough(gic, PIntId(intid), VIntId(intid)) {
            Ok(()) => {}
            Err(GicError::InvalidState) => continue,
            Err(e) => {
                println!("Failed to map PIRQ {}: {:?}", intid, e);
                return Err("vgic: map pirq");
            }
        }
    }

    VGIC.set_pirq_hook(Some(&BOOT_PIRQ_LIFECYCLE_HOOK))
        .map_err(|_| "vgic: hooks")?;

    // SAFETY: vGIC state is initialized once before guest entry.
    unsafe {
        *GICD_RANGE.get() = Some((info.dist.base, info.dist.size));
        *MAINT_INTID.get() = info.maintenance_intid;
        *GICD_ID.get() = Some(Gicv2DistIdRegs::from_hw_gicd(gic.distributor()));
    }
    Ok(())
}

pub(crate) fn maintenance_intid() -> Option<u32> {
    // SAFETY: set during vGIC initialization and then read-only.
    unsafe { *MAINT_INTID.get() }
}

pub(crate) fn kick_sgi_intid() -> u32 {
    KICK_INTID
}

pub(crate) fn handles_gicd(addr: usize) -> bool {
    // SAFETY: range set once during init and then treated as read-only.
    let Some((base, size)) = (unsafe { *GICD_RANGE.get() }) else {
        return false;
    };
    (base..base + size).contains(&addr)
}

pub(crate) fn handle_gicd_read(addr: usize, access_size: Gicv2AccessSize) -> Result<u32, GicError> {
    // SAFETY: GICD range is initialized once before guest entry.
    let (base, _) = unsafe { (*GICD_RANGE.get()).ok_or(GicError::InvalidAddress)? };
    let offset = addr.saturating_sub(base) as u32;
    let dist_id = unsafe { (*GICD_ID.get()).ok_or(GicError::InvalidState)? };

    VGIC.handle_distributor_read(VcpuId(0), dist_id, offset, access_size)
}

pub(crate) fn handle_gicd_write(
    addr: usize,
    access_size: Gicv2AccessSize,
    value: u32,
) -> Result<(), GicError> {
    // SAFETY: GICD range is initialized once before guest entry.
    let (base, _) = unsafe { (*GICD_RANGE.get()).ok_or(GicError::InvalidAddress)? };
    let offset = addr.saturating_sub(base) as u32;
    let gic = handler::gic().ok_or(GicError::InvalidState)?;
    let dist_id = unsafe { (*GICD_ID.get()).ok_or(GicError::InvalidState)? };

    VGIC.handle_distributor_write(gic, VcpuId(0), dist_id, offset, access_size, value)
}

pub(crate) fn on_physical_irq(intid: u32, level: bool) -> Result<(), GicError> {
    let gic = handler::gic().ok_or(GicError::InvalidState)?;
    let result = VGIC.handle_physical_irq(gic, PIntId(intid), level);
    irq_monitor::record_injected_pirq(intid, result.is_ok());
    result
}

pub(crate) fn physical_irq_binding_kind(
    intid: u32,
) -> Result<Option<PhysicalIrqBindingKind>, GicError> {
    VGIC.physical_irq_binding_kind(PIntId(intid))
}

pub(crate) fn passthrough_physical_irq(intid: u32, level: bool) -> Result<(), GicError> {
    let gic = handler::gic().ok_or(GicError::InvalidState)?;
    let result = VGIC.inject_physical_irq_as_pending(gic, intid, level);
    irq_monitor::record_injected_pirq(intid, result.is_ok());
    result
}

pub(crate) fn handle_maintenance_irq() -> Result<(), GicError> {
    let gic = handler::gic().ok_or(GicError::InvalidState)?;
    VGIC.handle_maintenance(gic, VcpuId(0))
}

pub(crate) fn handle_kick_sgi() -> Result<(), GicError> {
    let gic = handler::gic().ok_or(GicError::InvalidState)?;
    let _kick = VGIC.refill_vcpu(gic, VcpuId(0))?;
    Ok(())
}
