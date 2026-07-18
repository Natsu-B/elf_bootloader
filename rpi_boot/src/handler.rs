use arch_hal::cpu;
use arch_hal::exceptions;
use arch_hal::exceptions::emulation::EmulationOutcome;
use arch_hal::exceptions::emulation::MmioDecoded;
use arch_hal::exceptions::emulation::{self};
use arch_hal::exceptions::memory_hook::AccessClass;
use arch_hal::exceptions::memory_hook::MmioError;
use arch_hal::exceptions::memory_hook::MmioHandler;
use arch_hal::exceptions::memory_hook::SplitPolicy;
use arch_hal::gic::GicCpuInterface;
use arch_hal::gic::GicError;
use arch_hal::gic::PIntId;
use arch_hal::gic::PhysicalIrqBindingKind;
use arch_hal::gic::gicv2::Gicv2;
use arch_hal::gic::gicv2::Gicv2AccessSize;
use arch_hal::println;
use arch_hal::psci::PsciFunctionId;
use arch_hal::psci::PsciReturnCode;
use arch_hal::psci::default_psci_handler;
use arch_hal::psci::{self};
use arch_hal::soc::bcm2712;
use arch_hal::tls;
use core::ffi::c_void;
use core::ptr::read_volatile;
use core::ptr::write_volatile;
use core::sync::atomic::Ordering;
use exceptions::registers::ESR_EL2;
use exceptions::registers::InstructionRegisterSize;
use exceptions::registers::SyndromeAccessSize;
use exceptions::registers::WriteNotRead;
use exceptions::synchronous_handler::DataAbortHandlerEntry;
use exceptions::synchronous_handler::DataAbortInfo;
use mutex::pod::RawAtomicPod;

use crate::GICV2_DRIVER;
use crate::GUEST_PL011_UART0_ADDR;
use crate::HYPERVISOR_PL011_UART1_ADDR;
use crate::vgic;
use crate::virtio_blk;

pub(crate) fn setup_handler() {
    exceptions::synchronous_handler::set_data_abort_handler(DATA_ABORT_HANDLER);
    exceptions::irq_handler::set_irq_handler(irq_handler);

    // intercept guest PSCI CPU_ON for hypervisor-controlled AP bring-up.
    // psci::set_psci_handler(PsciFunctionId::CpuOnSmc64, ap_on);
    // psci::set_psci_handler(PsciFunctionId::CpuOnSmc32, ap_on);
    psci::set_psci_handler(PsciFunctionId::CpuOnSmc64, deny_handler);
    psci::set_psci_handler(PsciFunctionId::CpuOnSmc32, deny_handler);
    psci::set_psci_handler(PsciFunctionId::CpuSuspendSmc32, deny_handler);
    psci::set_psci_handler(PsciFunctionId::CpuSuspendSmc64, deny_handler);
    psci::set_psci_handler(PsciFunctionId::CpuDefaultSuspendSmc32, deny_handler);
    psci::set_psci_handler(PsciFunctionId::CpuDefaultSuspendSmc64, deny_handler);

    psci::set_unknown_psci_handler(unknown_psci_handler);
}

pub(crate) fn register_gic(gic: Gicv2) {
    // SAFETY: Called once during early boot before IRQs are enabled.
    // This write happens during single-core initialization with no concurrent access.
    unsafe {
        let slot = &mut *GICV2_DRIVER.get();
        assert!(slot.is_none(), "gic: already registered");
        *slot = Some(gic);
    }
}

pub(crate) fn gic() -> Option<&'static Gicv2> {
    // SAFETY: GICV2_DRIVER is initialized once during boot and then treated as read-only.
    unsafe { &*GICV2_DRIVER.get() }.as_ref()
}

struct PassthroughMmio;

impl MmioHandler for PassthroughMmio {
    fn read(&self, ipa: u64, size: u8) -> Result<u64, MmioError> {
        // SAFETY: `probe_subaccess` admits only aligned device-MMIO-sized accesses for trapped
        // identity-mapped regions; the access width is the decoded guest access size.
        let value = unsafe {
            match size {
                1 => read_volatile(ipa as *const u8) as u64,
                2 => read_volatile(ipa as *const u16) as u64,
                4 => read_volatile(ipa as *const u32) as u64,
                8 => read_volatile(ipa as *const u64),
                _ => return Err(MmioError::Unhandled),
            }
        };
        note_rp1_passthrough_mmio_access(ipa as usize, false);
        Ok(value)
    }

    fn write(&self, ipa: u64, size: u8, value: u64) -> Result<(), MmioError> {
        // SAFETY: `probe_subaccess` admits only aligned device-MMIO-sized accesses for trapped
        // identity-mapped regions; the access width is the decoded guest access size.
        unsafe {
            match size {
                1 => write_volatile(ipa as *mut u8, value as u8),
                2 => write_volatile(ipa as *mut u16, value as u16),
                4 => write_volatile(ipa as *mut u32, value as u32),
                8 => write_volatile(ipa as *mut u64, value),
                _ => return Err(MmioError::Unhandled),
            }
        }
        note_rp1_passthrough_mmio_access(ipa as usize, true);
        Ok(())
    }

    fn probe_subaccess(&self, ipa: u64, size: u8, _is_write: bool) -> bool {
        let addr = ipa as usize;
        matches!(size, 1 | 2 | 4 | 8)
            && !vgic::handles_gicd(addr)
            && !vgic::handles_gicc_phys(addr)
            && !is_hypervisor_uart_addr(addr)
            && !virtio_blk::handles_mmio(addr)
    }

    fn access_class(&self) -> AccessClass {
        AccessClass::DeviceMmio
    }

    fn split_policy(&self) -> SplitPolicy {
        SplitPolicy::Never
    }
}

fn read_guest_mmio_source_reg(
    regs: &cpu::Registers,
    reg_num: usize,
    reg_size: InstructionRegisterSize,
) -> u64 {
    if reg_num >= 32 {
        panic!("invalid register {}", reg_num);
    }
    let raw = if reg_num == 31 {
        0
    } else {
        regs.gprs()[reg_num]
    };
    match reg_size {
        InstructionRegisterSize::Instruction32bit => raw & (u32::MAX as u64),
        InstructionRegisterSize::Instruction64bit => raw,
    }
}

fn write_guest_mmio_dest_reg(
    regs: &mut cpu::Registers,
    reg_num: usize,
    reg_size: InstructionRegisterSize,
    value: u64,
) {
    if reg_num >= 32 {
        panic!("invalid register {}", reg_num);
    }
    if reg_num == 31 {
        return;
    }
    let val = match reg_size {
        InstructionRegisterSize::Instruction32bit => value & (u32::MAX as u64),
        InstructionRegisterSize::Instruction64bit => value,
    };
    regs.gprs_mut()[reg_num] = val;
}

fn data_abort_handler(
    _ctx: *mut c_void,
    regs: &mut cpu::Registers,
    info: &DataAbortInfo,
    decoded: Option<&MmioDecoded>,
) {
    if let Some(plan) = decoded {
        if handle_gicd_decoded(plan, regs) {
            return;
        }
        if handle_virtio_blk_decoded(plan, regs) {
            return;
        }
        if gicc_access_denied(plan) {
            panic!("gicc: physical GICC access denied; guest must use GICV");
        }
        if hypervisor_uart_access_denied(plan) {
            panic!("uart1: guest access to hypervisor debug UART denied");
        }
        log_uart_write(plan, regs);
        match emulation::execute_mmio(regs, info, &PASSTHROUGH_MMIO, plan) {
            EmulationOutcome::Handled => return,
            EmulationOutcome::NotHandled => {
                panic!("decoded MMIO request was not handled: {:?}", plan)
            }
        }
    }

    let Some(address) = info.fault_ipa else {
        panic!(
            "data abort without IPA (FnV={}, FAR_EL2={:#X})",
            info.esr_el2.get(ESR_EL2::fnv),
            info.far_el2
        );
    };

    let Some(access) = info.access else {
        panic!(
            "data abort at IPA 0x{:X} without access info (ISV decode failed)",
            address
        );
    };

    let reg_num = access.reg_num;
    if reg_num >= 32 {
        panic!(
            "data abort at IPA 0x{:X} with invalid register {}",
            address, reg_num
        );
    }
    let reg_size: InstructionRegisterSize = access.reg_size;
    let access_width: SyndromeAccessSize = access.access_width;
    let write_access: WriteNotRead = access.write_access;
    let access_bytes = match access_width {
        SyndromeAccessSize::Byte => 1,
        SyndromeAccessSize::HalfWord => 2,
        SyndromeAccessSize::Word => 4,
        SyndromeAccessSize::DoubleWord => 8,
    };

    if vgic::handles_gicd(address as usize) {
        let Some(gic_access) = gic_access_size_from_bytes(access_bytes) else {
            panic!("gicd: unsupported access size {}", access_bytes);
        };
        match write_access {
            WriteNotRead::ReadingMemoryAbort => {
                let value =
                    vgic::handle_gicd_read(address as usize, gic_access).expect("gicd read failed");
                write_guest_mmio_dest_reg(regs, reg_num, reg_size, value as u64);
            }
            WriteNotRead::WritingMemoryAbort => {
                let reg_val = read_guest_mmio_source_reg(regs, reg_num, reg_size);
                vgic::handle_gicd_write(address as usize, gic_access, reg_val as u32)
                    .unwrap_or_else(|e| {
                        panic!(
                            "gicd write failed {:?}: addr: 0x{:x}, size: {:?}, value: 0x{:x}",
                            e, address, gic_access, reg_val
                        )
                    });
            }
        }
        cpu::set_elr_el2(cpu::get_elr_el2() + 4);
        return;
    }
    if virtio_blk::handles_mmio(address as usize) {
        match write_access {
            WriteNotRead::ReadingMemoryAbort => {
                let value = virtio_blk::handle_mmio_read(address as usize, access_bytes)
                    .unwrap_or_else(|err| panic!("virtio-blk read failed: {err}"));
                write_guest_mmio_dest_reg(regs, reg_num, reg_size, value);
            }
            WriteNotRead::WritingMemoryAbort => {
                let reg_val = read_guest_mmio_source_reg(regs, reg_num, reg_size);
                virtio_blk::handle_mmio_write(address as usize, access_bytes, reg_val)
                    .unwrap_or_else(|err| panic!("virtio-blk write failed: {err}"));
            }
        }
        cpu::set_elr_el2(cpu::get_elr_el2() + 4);
        return;
    }
    if vgic::handles_gicc_phys(address as usize) {
        panic!("gicc: physical GICC access denied; guest must use GICV");
    }
    if is_hypervisor_uart_addr(address as usize) {
        panic!("uart1: guest access to hypervisor debug UART denied");
    }

    // SAFETY: The abort IPA is in an identity-mapped device region admitted by the stage-2 trap
    // policy, and the access size comes from the decoded ESR syndrome.
    unsafe {
        match write_access {
            WriteNotRead::ReadingMemoryAbort => {
                let v = match access_width {
                    SyndromeAccessSize::Byte => read_volatile(address as *const u8) as u64,
                    SyndromeAccessSize::HalfWord => read_volatile(address as *const u16) as u64,
                    SyndromeAccessSize::Word => read_volatile(address as *const u32) as u64,
                    SyndromeAccessSize::DoubleWord => read_volatile(address as *const u64),
                };

                write_guest_mmio_dest_reg(regs, reg_num, reg_size, v);
            }

            WriteNotRead::WritingMemoryAbort => {
                let reg_val = read_guest_mmio_source_reg(regs, reg_num, reg_size);
                let log_value = if reg_num == 31 {
                    0
                } else {
                    regs.gprs()[reg_num]
                };
                let uart = GUEST_PL011_UART0_ADDR.0;
                if is_guest_uart_addr(address as usize) {
                    if uart == address as usize && reg_val == b'\n' as u64 {
                        println!("\nhypervisor: alive");
                    }
                } else {
                    println!(
                        "data abort trapped: addr: {:X}, access_width: {:?}, write?: {}, data: {}",
                        address,
                        access_width,
                        write_access == WriteNotRead::WritingMemoryAbort,
                        log_value
                    );
                };
                match access_width {
                    SyndromeAccessSize::Byte => {
                        write_volatile(address as *mut u8, reg_val as u8);
                    }
                    SyndromeAccessSize::HalfWord => {
                        write_volatile(address as *mut u16, reg_val as u16);
                    }
                    SyndromeAccessSize::Word => {
                        write_volatile(address as *mut u32, reg_val as u32);
                    }
                    SyndromeAccessSize::DoubleWord => {
                        write_volatile(address as *mut u64, reg_val as u64);
                    }
                }
            }
        }
    }
    note_rp1_passthrough_mmio_access(
        address as usize,
        write_access == WriteNotRead::WritingMemoryAbort,
    );
    // advance elr_el2
    cpu::set_elr_el2(cpu::get_elr_el2() + 4);
}

fn handle_gicd_decoded(plan: &MmioDecoded, regs: &mut cpu::Registers) -> bool {
    match plan {
        MmioDecoded::Single {
            desc, ipa, split, ..
        } => {
            if !vgic::handles_gicd(*ipa as usize) {
                return false;
            }
            if split.is_some() {
                panic!("gicd: split access not supported");
            }
            let Some(access) = gic_access_size_from_bytes(desc.size) else {
                panic!("gicd: unsupported access size {}", desc.size);
            };
            if desc.is_store {
                let reg_val = store_value(desc, regs);
                vgic::handle_gicd_write(*ipa as usize, access, reg_val as u32)
                    .expect("gicd write failed");
            } else {
                let value =
                    vgic::handle_gicd_read(*ipa as usize, access).expect("gicd read failed");
                if desc.rt == 31 {
                    // x31 reads as zero; discard the loaded value.
                } else {
                    if desc.rt >= 32 {
                        panic!("gicd: invalid register {}", desc.rt);
                    }
                    write_guest_mmio_dest_reg(regs, desc.rt as usize, desc.rt_size, value as u64);
                }
            }
            cpu::set_elr_el2(cpu::get_elr_el2() + 4);
            true
        }
        MmioDecoded::Pair { ipa0, ipa1, .. } => {
            if vgic::handles_gicd(*ipa0 as usize) || vgic::handles_gicd(*ipa1 as usize) {
                panic!("gicd: pair access not supported");
            }
            false
        }
        MmioDecoded::Prefetch { .. } => false,
    }
}

fn gicc_access_denied(plan: &MmioDecoded) -> bool {
    match plan {
        MmioDecoded::Single { ipa, .. } => vgic::handles_gicc_phys(*ipa as usize),
        MmioDecoded::Pair { ipa0, ipa1, .. } => {
            vgic::handles_gicc_phys(*ipa0 as usize) || vgic::handles_gicc_phys(*ipa1 as usize)
        }
        MmioDecoded::Prefetch { .. } => false,
    }
}

fn hypervisor_uart_access_denied(plan: &MmioDecoded) -> bool {
    match plan {
        MmioDecoded::Single { ipa, .. } => is_hypervisor_uart_addr(*ipa as usize),
        MmioDecoded::Pair { ipa0, ipa1, .. } => {
            is_hypervisor_uart_addr(*ipa0 as usize) || is_hypervisor_uart_addr(*ipa1 as usize)
        }
        MmioDecoded::Prefetch { .. } => false,
    }
}

fn is_hypervisor_uart_addr(addr: usize) -> bool {
    in_mmio_window(addr, HYPERVISOR_PL011_UART1_ADDR.0, 0x1000)
}

fn is_guest_uart_addr(addr: usize) -> bool {
    in_mmio_window(addr, GUEST_PL011_UART0_ADDR.0, 0x1000)
}

fn in_mmio_window(addr: usize, base: usize, size: usize) -> bool {
    let Some(end) = base.checked_add(size) else {
        return false;
    };
    (base..end).contains(&addr)
}

fn gic_access_size_from_bytes(access_bytes: u8) -> Option<Gicv2AccessSize> {
    match access_bytes {
        1 => Some(Gicv2AccessSize::U8),
        4 => Some(Gicv2AccessSize::U32),
        _ => None,
    }
}

fn handle_virtio_blk_decoded(plan: &MmioDecoded, regs: &mut cpu::Registers) -> bool {
    match plan {
        MmioDecoded::Single {
            desc, ipa, split, ..
        } => {
            if split.is_some() || !virtio_blk::handles_mmio(*ipa as usize) {
                return false;
            }
            if desc.is_store {
                let value = store_value(desc, regs);
                virtio_blk::handle_mmio_write(*ipa as usize, desc.size, value)
                    .unwrap_or_else(|err| panic!("virtio-blk write failed: {err}"));
            } else if desc.rt == 31 {
                virtio_blk::handle_mmio_read(*ipa as usize, desc.size)
                    .unwrap_or_else(|err| panic!("virtio-blk read failed: {err}"));
            } else {
                let value = virtio_blk::handle_mmio_read(*ipa as usize, desc.size)
                    .unwrap_or_else(|err| panic!("virtio-blk read failed: {err}"));
                if desc.rt >= 32 {
                    panic!("virtio-blk: invalid register {}", desc.rt);
                }
                write_guest_mmio_dest_reg(regs, desc.rt as usize, desc.rt_size, value);
            }
            cpu::set_elr_el2(cpu::get_elr_el2() + 4);
            true
        }
        MmioDecoded::Pair { ipa0, ipa1, .. } => {
            if virtio_blk::handles_mmio(*ipa0 as usize) || virtio_blk::handles_mmio(*ipa1 as usize)
            {
                panic!("virtio-blk: pair access is not supported");
            }
            false
        }
        MmioDecoded::Prefetch { .. } => false,
    }
}

const LOG_MAINT_ERR: u32 = 1 << 0;
const LOG_PIRQ_ERR: u32 = 1 << 1;
const LOG_EOI_ERR: u32 = 1 << 3;
const LOG_DEACT_ERR: u32 = 1 << 4;
const LOG_KICK_ERR: u32 = 1 << 5;
const LOG_RP1_RESAMPLE_ERR: u32 = 1 << 6;
const LOG_RP1_MMIO_ERR: u32 = 1 << 7;
static IRQ_LOG_ONCE: RawAtomicPod<u32> = unsafe { RawAtomicPod::new_raw_unchecked(0) };

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum AckedPirqDelivery {
    Bound(PhysicalIrqBindingKind),
    Error(GicError),
}

fn handle_acknowledged_pirq(irq_intid: u32) -> AckedPirqDelivery {
    let pintid = PIntId(irq_intid);
    match vgic::physical_irq_guest_state(pintid) {
        Ok(Some(state)) if state.accepts_asserted_ingress() => {
            match vgic::on_physical_irq_asserted(pintid) {
                Ok(()) => AckedPirqDelivery::Bound(state.binding_kind),
                Err(err) => AckedPirqDelivery::Error(err),
            }
        }
        Ok(Some(state)) => {
            panic!(
                "vgic: physical IRQ {} reached EL2 before guest enable (binding={:?}, guest_enable={}, distributor_enable={}, cpu_if={:?})",
                irq_intid,
                state.binding_kind,
                state.guest_enable,
                state.distributor_enable,
                tls::cpu_if(),
            );
        }
        Ok(None) => AckedPirqDelivery::Error(GicError::UnsupportedIntId),
        Err(err) => AckedPirqDelivery::Error(err),
    }
}

fn needs_deactivate_after_ack(
    irq_intid: u32,
    maintenance_intid: Option<u32>,
    kick_sgi_intid: u32,
    pirq_delivery: Option<AckedPirqDelivery>,
) -> bool {
    if Some(irq_intid) == maintenance_intid || irq_intid == kick_sgi_intid {
        return true;
    }

    match pirq_delivery.expect("physical IRQs must provide a vGIC delivery result") {
        AckedPirqDelivery::Bound(PhysicalIrqBindingKind::SoftwareLr) => true,
        AckedPirqDelivery::Bound(PhysicalIrqBindingKind::HardwareLr) => false,
        AckedPirqDelivery::Error(err) if pirq_error_requires_panic(err) => {
            panic!(
                "vgic: unbound/policy-denied physical IRQ {} reached EL2",
                irq_intid
            );
        }
        AckedPirqDelivery::Error(_) => true,
    }
}

fn pirq_error_requires_panic(err: GicError) -> bool {
    matches!(err, GicError::UnsupportedIntId)
}

fn note_rp1_passthrough_mmio_access(addr: usize, is_write: bool) {
    if let Err(err) = bcm2712::pirq_hook::after_passthrough_mmio_access(addr, is_write) {
        if IRQ_LOG_ONCE.fetch_or(LOG_RP1_MMIO_ERR, Ordering::Relaxed) & LOG_RP1_MMIO_ERR == 0 {
            println!("rp1: passthrough MMIO completion failed: {:?}", err);
        }
    }
}

fn irq_handler(_regs: &mut cpu::Registers) {
    let Some(gicv2) = gic() else {
        return;
    };
    let Ok(Some(irq)) = gicv2.acknowledge() else {
        return;
    };
    let maintenance_intid = vgic::maintenance_intid();
    let kick_sgi_intid = vgic::kick_sgi_intid();
    let mut pirq_delivery = None;
    if Some(irq.intid) == maintenance_intid {
        if let Err(err) = vgic::handle_maintenance_irq() {
            if IRQ_LOG_ONCE.fetch_or(LOG_MAINT_ERR, Ordering::Relaxed) & LOG_MAINT_ERR == 0 {
                println!("vgic: maintenance IRQ failed: {:?}", err);
            }
        }
    } else if irq.intid == kick_sgi_intid {
        if let Err(err) = vgic::handle_kick_sgi() {
            if IRQ_LOG_ONCE.fetch_or(LOG_KICK_ERR, Ordering::Relaxed) & LOG_KICK_ERR == 0 {
                println!("vgic: kick SGI handling failed: {:?}", err);
            }
        }
    } else {
        if irq.intid == 27 {
            // println!("IRQ 27 (physical) CpuId: {}", tls::cpu_if().unwrap());
        }
        let delivery = handle_acknowledged_pirq(irq.intid);
        if let AckedPirqDelivery::Error(err) = delivery {
            if err != GicError::UnsupportedIntId
                && IRQ_LOG_ONCE.fetch_or(LOG_PIRQ_ERR, Ordering::Relaxed) & LOG_PIRQ_ERR == 0
            {
                println!("vgic: pIRQ {} handling failed: {:?}", irq.intid, err);
            }
        }
        pirq_delivery = Some(delivery);
    }
    let needs_deactivate =
        needs_deactivate_after_ack(irq.intid, maintenance_intid, kick_sgi_intid, pirq_delivery);
    if let Err(err) = gicv2.end_of_interrupt(irq) {
        if IRQ_LOG_ONCE.fetch_or(LOG_EOI_ERR, Ordering::Relaxed) & LOG_EOI_ERR == 0 {
            println!("gic: end_of_interrupt failed: {:?}", err);
        }
    }
    if needs_deactivate {
        if let Err(err) = gicv2.deactivate(irq) {
            if IRQ_LOG_ONCE.fetch_or(LOG_DEACT_ERR, Ordering::Relaxed) & LOG_DEACT_ERR == 0 {
                println!("gic: deactivate failed: {:?}", err);
            }
        }
    }
    if let Err(err) = bcm2712::pirq_hook::resample_level_sources(Some(irq.intid)) {
        if IRQ_LOG_ONCE.fetch_or(LOG_RP1_RESAMPLE_ERR, Ordering::Relaxed) & LOG_RP1_RESAMPLE_ERR
            == 0
        {
            println!("rp1: level MSI-X resample failed: {:?}", err);
        }
    }
}

static PASSTHROUGH_MMIO: PassthroughMmio = PassthroughMmio;

static DATA_ABORT_HANDLER: DataAbortHandlerEntry = DataAbortHandlerEntry {
    ctx: core::ptr::null_mut(),
    handler: data_abort_handler,
};

fn log_uart_write(plan: &MmioDecoded, regs: &cpu::Registers) {
    let (ipa, desc) = match plan {
        MmioDecoded::Single {
            desc,
            ipa,
            split: _,
            ..
        } if desc.is_store => (*ipa as usize, desc),
        _ => return,
    };
    if !is_guest_uart_addr(ipa) {
        return;
    }
    let uart = GUEST_PL011_UART0_ADDR.0;
    let value = store_value(desc, regs);
    if ipa == uart && value == b'\n' as u64 {
        println!("\nhypervisor: alive");
    }
}

fn store_value(desc: &emulation::SingleDesc, regs: &cpu::Registers) -> u64 {
    let regs_view = regs.gprs();
    let raw = if desc.rt == 31 {
        0
    } else {
        regs_view[desc.rt as usize]
    };
    match desc.size {
        1 => raw & 0xff,
        2 => raw & 0xffff,
        4 => raw & 0xffff_ffff,
        _ => raw,
    }
}

fn deny_handler(regs: &mut cpu::Registers) {
    regs.x0 = PsciReturnCode::Denied.to_x0();
}

fn unknown_psci_handler(fid_raw: u32, regs: &mut cpu::Registers) {
    println!("unknown psci call fid=0x{:08X}", fid_raw);
    println!(
        "x0={:#018x} x1={:#018x} x2={:#018x} x3={:#018x}",
        regs.x0, regs.x1, regs.x2, regs.x3
    );
    println!(
        "x4={:#018x} x5={:#018x} x6={:#018x} x7={:#018x}",
        regs.x4, regs.x5, regs.x6, regs.x7
    );
    println!(
        "x8={:#018x} x9={:#018x} x10={:#018x} x11={:#018x}",
        regs.x8, regs.x9, regs.x10, regs.x11
    );
    println!(
        "x12={:#018x} x13={:#018x} x14={:#018x} x15={:#018x}",
        regs.x12, regs.x13, regs.x14, regs.x15
    );
    println!(
        "x16={:#018x} x17={:#018x} x18={:#018x} x19={:#018x}",
        regs.x16, regs.x17, regs.x18, regs.x19
    );
    println!(
        "x20={:#018x} x21={:#018x} x22={:#018x} x23={:#018x}",
        regs.x20, regs.x21, regs.x22, regs.x23
    );
    println!(
        "x24={:#018x} x25={:#018x} x26={:#018x} x27={:#018x}",
        regs.x24, regs.x25, regs.x26, regs.x27
    );
    println!(
        "x28={:#018x} x29={:#018x} x30={:#018x} x31={:#018x}",
        regs.x28, regs.x29, regs.x30, regs.x31
    );
    default_psci_handler(regs);
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;

    #[test_case]
    fn maintenance_irq_still_requests_deactivate() {
        assert!(needs_deactivate_after_ack(25, Some(25), 15, None));
    }

    #[test_case]
    fn kick_sgi_still_requests_deactivate() {
        assert!(needs_deactivate_after_ack(15, Some(25), 15, None));
    }

    #[test_case]
    fn acknowledged_software_lr_pirq_requests_deactivate() {
        assert!(needs_deactivate_after_ack(
            27,
            Some(25),
            15,
            Some(AckedPirqDelivery::Bound(PhysicalIrqBindingKind::SoftwareLr))
        ));
    }

    #[test_case]
    fn acknowledged_hardware_lr_pirq_skips_deactivate() {
        assert!(!needs_deactivate_after_ack(
            27,
            Some(25),
            15,
            Some(AckedPirqDelivery::Bound(PhysicalIrqBindingKind::HardwareLr))
        ));
    }

    #[test_case]
    fn unsupported_passthrough_is_classified_as_panic() {
        assert!(pirq_error_requires_panic(GicError::UnsupportedIntId));
    }
}
