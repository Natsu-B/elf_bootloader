use arch_hal::cpu;
use arch_hal::gic::GicError;
use arch_hal::gic::GicIrqMirror;
use arch_hal::gic::GicSgi;
use arch_hal::gic::IrqSense;
use arch_hal::gic::PIntId;
use arch_hal::gic::PhysicalIrqGuestState;
use arch_hal::gic::SgiTarget;
use arch_hal::gic::VIntId;
use arch_hal::gic::VcpuId;
use arch_hal::gic::VgicHw;
use arch_hal::gic::gicv2::Gicv2;
use arch_hal::gic::gicv2::Gicv2AccessSize;
use arch_hal::gic::gicv2::Gicv2DistIdRegs;
use arch_hal::gic::vm::PirqLifecycleHook;
use arch_hal::gic::vm::manager::VgicDelegate;
use arch_hal::gic::vm::manager::VgicManager;
use arch_hal::println;
use arch_hal::soc::bcm2712;
use arch_hal::tls;
use core::cell::SyncUnsafeCell;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;

use crate::Gicv2Info;
use crate::handler;

#[derive(Clone, Copy, Debug)]
pub(crate) struct UartIrq {
    pub pintid: u32,
    pub sense: IrqSense,
}

const MIP_SPI_OFFSET: u32 = 128;
const EXPLICIT_SPI_PASSTHROUGH_SPIS: [u32; 3] = [265, 266, 305];
const VCPU_COUNT: usize = 4;
const KICK_SGI_ID: u8 = 15;
const KICK_INTID: u32 = KICK_SGI_ID as u32;
const GUEST_EL1_VTIMER_PPI_INTID: u32 = 27;

struct RpiVgicDelegate;

static DELEGATE: RpiVgicDelegate = RpiVgicDelegate;
static VGIC: VgicManager<VCPU_COUNT> = VgicManager::new(&DELEGATE, 0);
static GICD_RANGE: SyncUnsafeCell<Option<(usize, usize)>> = SyncUnsafeCell::new(None);
static GICC_RANGE: SyncUnsafeCell<Option<(usize, usize)>> = SyncUnsafeCell::new(None);
static MAINT_INTID: SyncUnsafeCell<Option<u32>> = SyncUnsafeCell::new(None);
static GICD_ID: SyncUnsafeCell<Option<Gicv2DistIdRegs>> = SyncUnsafeCell::new(None);
static VCPU_ONLINE_BITMAP: AtomicUsize = AtomicUsize::new(0);
static VCPU_AFFINITIES: SyncUnsafeCell<[Option<cpu::CoreAffinity>; VCPU_COUNT]> =
    SyncUnsafeCell::new([None; VCPU_COUNT]);

impl VgicDelegate for RpiVgicDelegate {
    fn irq_mirror(&self) -> Result<&'static dyn GicIrqMirror, GicError> {
        handler::gic()
            .ok_or(GicError::InvalidState)
            .map(|gic| gic as &'static dyn GicIrqMirror)
    }

    fn get_resident_affinity(
        &self,
        _vm_id: usize,
        vcpu_id: u16,
    ) -> Result<Option<cpu::CoreAffinity>, GicError> {
        let vcpu = VcpuId(vcpu_id);
        vcpu_index(vcpu)?;
        if !is_vcpu_online(vcpu) {
            return Ok(None);
        }
        Ok(Some(affinity_for_vcpu(vcpu)?))
    }

    fn get_home_affinity(
        &self,
        _vm_id: usize,
        vcpu_id: u16,
    ) -> Result<cpu::CoreAffinity, GicError> {
        affinity_for_vcpu(VcpuId(vcpu_id))
    }

    fn get_current_vcpu(&self, _vm_id: usize) -> Result<VcpuId, GicError> {
        current_vcpu_id()
    }

    fn kick_pcpu(&self, target: cpu::CoreAffinity) -> Result<(), GicError> {
        let gic = handler::gic().ok_or(GicError::InvalidState)?;
        let targets = [target];
        gic.send_sgi(KICK_SGI_ID, SgiTarget::Specific(&targets))
    }
}

fn map_guest_local_ppis_for_vcpu(gic: &Gicv2, vcpu: VcpuId) -> Result<(), GicError> {
    VGIC.bind_local_pirq_passthrough(
        gic,
        PIntId(GUEST_EL1_VTIMER_PPI_INTID),
        vcpu,
        VIntId(GUEST_EL1_VTIMER_PPI_INTID),
    )
}

fn current_vcpu_id() -> Result<VcpuId, GicError> {
    let vcpu_id = VcpuId(u16::from(tls::cpu_if().ok_or(GicError::InvalidState)?));
    vcpu_index(vcpu_id)?;
    Ok(vcpu_id)
}

fn vcpu_index(vcpu_id: VcpuId) -> Result<usize, GicError> {
    let index = usize::from(vcpu_id.0);
    if index >= VCPU_COUNT || index >= usize::BITS as usize {
        return Err(GicError::InvalidVcpuId);
    }
    Ok(index)
}

fn vcpu_bit(vcpu_id: VcpuId) -> Result<usize, GicError> {
    Ok(1usize << vcpu_index(vcpu_id)?)
}

fn reset_online_state_for_init() {
    VCPU_ONLINE_BITMAP.store(0, Ordering::Release);
}

fn reset_vcpu_affinities_for_init() {
    // SAFETY: Called once on BSP before concurrent vGIC operations start.
    unsafe {
        *VCPU_AFFINITIES.get() = [None; VCPU_COUNT];
    }
}

fn mark_vcpu_online(vcpu_id: VcpuId) -> Result<(), GicError> {
    let bit = vcpu_bit(vcpu_id)?;
    VCPU_ONLINE_BITMAP.fetch_or(bit, Ordering::AcqRel);
    Ok(())
}

fn register_vcpu_affinity(vcpu_id: VcpuId, affinity: cpu::CoreAffinity) -> Result<(), GicError> {
    let index = vcpu_index(vcpu_id)?;
    // SAFETY: The table is mutated only during CPU bring-up. Conflicting updates are rejected,
    // and idempotent re-registration of the same value is allowed.
    let table = unsafe { &mut *VCPU_AFFINITIES.get() };
    match table[index] {
        Some(existing) if existing != affinity => Err(GicError::InvalidState),
        Some(_) => Ok(()),
        None => {
            table[index] = Some(affinity);
            Ok(())
        }
    }
}

fn is_vcpu_online(vcpu_id: VcpuId) -> bool {
    let Ok(bit) = vcpu_bit(vcpu_id) else {
        return false;
    };
    (VCPU_ONLINE_BITMAP.load(Ordering::Acquire) & bit) != 0
}

fn affinity_for_vcpu(vcpu_id: VcpuId) -> Result<cpu::CoreAffinity, GicError> {
    let index = vcpu_index(vcpu_id)?;
    // SAFETY: Entries are written during vCPU bring-up and then read by delegate callbacks.
    let table = unsafe { &*VCPU_AFFINITIES.get() };
    table[index].ok_or(GicError::InvalidState)
}

fn is_explicit_spi_passthrough(spi: u32) -> bool {
    EXPLICIT_SPI_PASSTHROUGH_SPIS.contains(&spi)
        || bcm2712::pirq_hook::is_guest_rp1_passthrough_spi(spi)
}

fn bind_spi_passthrough(gic: &Gicv2, spi: u32) -> Result<(), &'static str> {
    match VGIC.bind_spi_pirq_passthrough(gic, PIntId(spi), VIntId(spi)) {
        Ok(()) | Err(GicError::InvalidState) => Ok(()),
        Err(err) => {
            println!("vgic: map pirq failed: {:?}", err);
            Err("vgic: map pirq failed")
        }
    }
}

fn bind_spi_write_through_software_lr(gic: &Gicv2, spi: u32) -> Result<(), &'static str> {
    match VGIC.bind_spi_pirq_write_through_software_lr(gic, PIntId(spi), VIntId(spi)) {
        Ok(()) | Err(GicError::InvalidState) => Ok(()),
        Err(err) => {
            println!("vgic: map software-lr pirq failed: {:?}", err);
            Err("vgic: map software-lr pirq failed")
        }
    }
}

fn bind_spi_shadow_software_lr(gic: &Gicv2, spi: u32) -> Result<(), &'static str> {
    match VGIC.bind_spi_pirq_shadow_software_lr(gic, PIntId(spi), VIntId(spi)) {
        Ok(()) | Err(GicError::InvalidState) => Ok(()),
        Err(err) => {
            println!("vgic: map shadow software-lr pirq failed: {:?}", err);
            Err("vgic: map shadow software-lr pirq failed")
        }
    }
}

pub fn init(gic: &Gicv2, info: &Gicv2Info, uart_irq: Option<UartIrq>) -> Result<(), &'static str> {
    reset_online_state_for_init();
    reset_vcpu_affinities_for_init();
    gic.hw_init().map_err(|_| "vgic: hw init failed")?;

    // SAFETY: Called exactly once on BSP during single-core bring-up before concurrent vGIC use.
    // `VGIC` is a `'static` manager, so in-place model storage has a stable address for lifetime.
    unsafe {
        VGIC.init_from_gicv2(gic, VCPU_COUNT as u8)
            .map_err(|_| "vgic: create vm failed")?
    };

    let boot_vcpu_id = current_vcpu_id().map_err(|_| "vgic: invalid vcpu id")?;
    let boot_affinity = cpu::get_current_core_id();
    register_vcpu_affinity(boot_vcpu_id, boot_affinity).map_err(|_| "vgic: register affinity")?;
    VGIC.switch_in(gic, boot_vcpu_id, boot_affinity)
        .map_err(|_| "vgic: switch in")?;
    map_guest_local_ppis_for_vcpu(gic, boot_vcpu_id).map_err(|x| {
        println!("vgic: map guest local ppis failed: {:?}", x);
        "vgic: map guest local ppis failed"
    })?;
    mark_vcpu_online(boot_vcpu_id).map_err(|_| "vgic: mark online")?;

    if let Some(uart_irq) = uart_irq
        && uart_irq.pintid >= MIP_SPI_OFFSET + 32
        && !is_explicit_spi_passthrough(uart_irq.pintid)
    {
        // RP1 MSI-X sources use a separate IACK path. Software LR delivery keeps that IACK tied
        // to the guest EOI, after the PL011 driver has drained RX state.
        bind_spi_write_through_software_lr(gic, uart_irq.pintid)?;
    }

    let max_intid = gic.max_intid();
    for intid in 32..max_intid {
        if intid >= MIP_SPI_OFFSET + 32 {
            continue;
        }
        bind_spi_passthrough(gic, intid).map_err(|_| "vgic: map pirq")?;
    }

    for spi in EXPLICIT_SPI_PASSTHROUGH_SPIS {
        bind_spi_passthrough(gic, spi)?;
    }
    for spi in bcm2712::pirq_hook::GUEST_RP1_PASSTHROUGH_SPIS {
        if uart_irq.is_some_and(|irq| irq.pintid == spi) {
            // Guest UART0 is level-triggered PL011 state, but the host sees an RP1 MSI-X edge.
            // Keep the virtual line shadowed and tie MSI-X IACK to the guest EOI.
            bind_spi_shadow_software_lr(gic, spi)?;
        } else {
            bind_spi_passthrough(gic, spi)?;
        }
    }

    // SAFETY: vGIC state is initialized once before guest entry.
    unsafe {
        *GICD_RANGE.get() = Some((info.dist.base, info.dist.size));
        *GICC_RANGE.get() = Some((info.cpu.base, info.cpu.size));
        *MAINT_INTID.get() = info.maintenance_intid;
        *GICD_ID.get() = Some(Gicv2DistIdRegs::from_hw_gicd(gic.distributor()));
    }
    Ok(())
}

pub fn set_pirq_hook(hook: Option<&'static dyn PirqLifecycleHook>) -> Result<(), GicError> {
    VGIC.set_pirq_hook(hook)
}

pub fn on_cpu_online(gic: &Gicv2) -> Result<(), GicError> {
    gic.hw_init()?;
    let vcpu = current_vcpu_id()?;
    let affinity = cpu::get_current_core_id();
    register_vcpu_affinity(vcpu, affinity)?;
    VGIC.switch_in(gic, vcpu, affinity)?;
    map_guest_local_ppis_for_vcpu(gic, vcpu)?;
    mark_vcpu_online(vcpu)
}

pub fn maintenance_intid() -> Option<u32> {
    // SAFETY: set during vGIC initialization and then read-only.
    unsafe { *MAINT_INTID.get() }
}

pub fn kick_sgi_intid() -> u32 {
    KICK_INTID
}

pub fn handles_gicd(addr: usize) -> bool {
    // SAFETY: range set once during init and then treated as read-only.
    let Some((base, size)) = (unsafe { *GICD_RANGE.get() }) else {
        return false;
    };
    (base..base + size).contains(&addr)
}

pub fn handles_gicc_phys(addr: usize) -> bool {
    // SAFETY: range set once during init and then treated as read-only.
    let Some((base, size)) = (unsafe { *GICC_RANGE.get() }) else {
        return false;
    };
    (base..base + size).contains(&addr)
}

pub fn handle_gicd_read(addr: usize, access_size: Gicv2AccessSize) -> Result<u32, GicError> {
    // SAFETY: GICD range is initialized once before guest entry.
    let (base, _) = unsafe { (*GICD_RANGE.get()).ok_or(GicError::InvalidAddress)? };
    let offset = addr.saturating_sub(base) as u32;
    let dist_id = unsafe { (*GICD_ID.get()).ok_or(GicError::InvalidState)? };

    VGIC.handle_distributor_read(current_vcpu_id()?, dist_id, offset, access_size)
}

pub fn handle_gicd_write(
    addr: usize,
    access_size: Gicv2AccessSize,
    value: u32,
) -> Result<(), GicError> {
    // SAFETY: GICD range is initialized once before guest entry.
    let (base, _) = unsafe { (*GICD_RANGE.get()).ok_or(GicError::InvalidAddress)? };
    let offset = addr.saturating_sub(base) as u32;
    let gic = handler::gic().ok_or(GicError::InvalidState)?;
    let dist_id = unsafe { (*GICD_ID.get()).ok_or(GicError::InvalidState)? };

    VGIC.handle_distributor_write(gic, current_vcpu_id()?, dist_id, offset, access_size, value)
}

pub fn on_physical_irq(pintid: PIntId, level: bool) -> Result<(), GicError> {
    // println!(
    //     "vgic CpuId {}: physical IRQ received: pintid={}, level={}",
    //     tls::cpu_if().unwrap(),
    //     pintid.0,
    //     level
    // );
    let gic = handler::gic().ok_or(GicError::InvalidState)?;
    VGIC.handle_physical_irq(gic, pintid, level)
}

/// Handle asserted physical IRQ ingress observed by the EL2 IRQ exception path after GIC ACK.
pub fn on_physical_irq_asserted(pintid: PIntId) -> Result<(), GicError> {
    let gic = handler::gic().ok_or(GicError::InvalidState)?;
    VGIC.handle_physical_irq_asserted(gic, pintid)
}

pub(crate) fn physical_irq_guest_state(
    pintid: PIntId,
) -> Result<Option<PhysicalIrqGuestState>, GicError> {
    VGIC.physical_irq_guest_state(pintid)
}

pub fn handle_maintenance_irq() -> Result<(), GicError> {
    let gic = handler::gic().ok_or(GicError::InvalidState)?;
    VGIC.handle_maintenance(gic, current_vcpu_id()?)
}

pub fn handle_kick_sgi() -> Result<(), GicError> {
    let gic = handler::gic().ok_or(GicError::InvalidState)?;
    let _kick = VGIC.refill_vcpu(gic, current_vcpu_id()?)?;
    Ok(())
}

pub fn inject_virtual_spi(intid: u32, level: bool) -> Result<(), GicError> {
    let gic = handler::gic().ok_or(GicError::InvalidState)?;
    VGIC.inject_physical_irq_as_pending(gic, intid, level)
}
