// Each integration-test wrapper supplies its result label and EOI modes.
extern crate alloc;

use aarch64_test::exit_failure;
use aarch64_test::exit_success;
use alloc::vec::Vec;
use core::arch::asm;
use core::arch::naked_asm;
use core::ptr;
use core::ptr::NonNull;
use cpu;
use gic::BinaryPoint;
use gic::EnableOp;
use gic::EoiMode;
use gic::GicCpuConfig;
use gic::GicCpuInterface;
use gic::GicDistributor;
use gic::GicError;
use gic::IrqGroup;
use gic::MmioRegion;
use gic::SpiRoute;
use gic::TriggerMode;
use gic::gicv2::GICV2_GICC_FRAME_SIZE;
use gic::gicv2::GICV2_GICD_FRAME_SIZE;
use gic::gicv2::Gicv2;
use print::debug_uart;
use print::println;
use tls;

const UART_BASE: usize = 0x900_0000;
const UART_CLOCK_HZ: u32 = 48 * 1_000_000;
const GICD_BASE: usize = 0x0800_0000;
const GICC_BASE: usize = 0x0801_0000;
const SPI_ID: u32 = 89;

unsafe extern "C" {
    static __bss_start: u8;
    static __bss_end: u8;
    static __stack_top: u8;
}

const TLS_BUF_SIZE: usize = 4096;

#[repr(align(64))]
struct TlsBuf([u8; TLS_BUF_SIZE]);

static mut TLS_BUF: TlsBuf = TlsBuf([0; TLS_BUF_SIZE]);

struct Case {
    label: &'static str,
    group: IrqGroup,
    enable_group0: bool,
    enable_group1: bool,
}

fn entry() -> ! {
    debug_uart::init(UART_BASE, UART_CLOCK_HZ as u64, 115200);

    match run() {
        Ok(()) => {
            println!("{}: PASS", TEST_NAME);
            exit_success();
        }
        Err(err) => {
            println!("{}: FAIL: {}", TEST_NAME, err);
            exit_failure();
        }
    }
}

#[unsafe(no_mangle)]
#[unsafe(naked)]
extern "C" fn _start() -> ! {
    naked_asm!("ldr x0, =__stack_top", "mov sp, x0", "bl rust_entry", "b .",);
}

#[unsafe(no_mangle)]
extern "C" fn rust_entry() -> ! {
    unsafe { clear_bss() };
    let need = tls::template_size();
    if TLS_BUF_SIZE < need {
        println!(
            "tls buffer too small: need {} bytes, have {} bytes",
            need, TLS_BUF_SIZE
        );
        exit_failure();
    }
    // SAFETY: TLS_BUF is a single-core test buffer, 64-byte aligned, and lives
    // for the duration of the test. No other core aliases it.
    let tls_result = unsafe {
        let tls_ptr = core::ptr::addr_of_mut!(TLS_BUF.0).cast::<u8>();
        tls::init_current_cpu(NonNull::new_unchecked(tls_ptr), TLS_BUF_SIZE)
    };
    if let Err(err) = tls_result {
        println!("tls init failed: {:?}", err);
        exit_failure();
    }
    entry()
}

fn run() -> Result<(), &'static str> {
    unsafe { asm!("msr daifset, #0b1111",) };

    let gicd_typer = read_mmio32(GICD_BASE, 0x004);
    let security_extension_implemented = ((gicd_typer >> 10) & 0x1) != 0;
    println!(
        "GICD_TYPER=0x{:08x} SecurityExtn={}",
        gicd_typer, security_extension_implemented
    );
    if !security_extension_implemented {
        println!("SecurityExtn=0: logical Group0/Group1 emulation (AckCtl=0, IAR-only)");
    }

    let gic = Gicv2::new(
        MmioRegion {
            base: GICD_BASE,
            size: GICV2_GICD_FRAME_SIZE,
        },
        MmioRegion {
            base: GICC_BASE,
            size: GICV2_GICC_FRAME_SIZE,
        },
        None,
        None,
    )
    .map_err(|err| map_err("gic_new", err))?;

    gic.init_distributor()
        .map_err(|err| map_err("gicd_init", err))?;
    let caps = gic
        .init_cpu_interface()
        .map_err(|err| map_err("gicc_init", err))?;
    if security_extension_implemented {
        if !caps.supports_group1 {
            return Err("group1_not_supported");
        }
    } else if !caps.supports_group0 || !caps.supports_group1 {
        return Err("group_support_mismatch");
    }

    let mut cases = Vec::new();
    cases.push(Case {
        label: "group1",
        group: IrqGroup::Group1,
        enable_group0: true,
        enable_group1: true,
    });
    if !security_extension_implemented {
        cases.push(Case {
            label: "group0",
            group: IrqGroup::Group0,
            enable_group0: true,
            enable_group1: false,
        });
    }

    for case in &cases {
        for &eoi_mode in EOI_MODES {
            println!("=== running case {} mode {:?} ===", case.label, eoi_mode);
            run_case(&gic, case, security_extension_implemented, eoi_mode)?;
        }
    }

    Ok(())
}

fn run_case(
    gic: &Gicv2,
    case: &Case,
    security_extension_implemented: bool,
    eoi_mode: EoiMode,
) -> Result<(), &'static str> {
    unsafe { asm!("msr daifset, #0b1111",) };

    if !security_extension_implemented {
        println!(
            "case {}: logical grouping emulation active (hardware IGROUPR forced to Group0)",
            case.label
        );
    }

    // Mask SGIs/PPIs to avoid interference.
    write_mmio32(GICD_BASE, 0x180, 0x0000_ffff);
    write_mmio32(GICD_BASE, 0x280, 0x0000_ffff);
    write_mmio32(GICD_BASE, 0x380, 0x0000_ffff);
    cpu::dsb_sy();
    cpu::isb();

    // Disable and clear SPIs; re-enable only the target SPI.
    for word in 1..4 {
        write_mmio32(GICD_BASE, 0x180 + word * 4, 0xffff_ffff);
        write_mmio32(GICD_BASE, 0x280 + word * 4, 0xffff_ffff);
        write_mmio32(GICD_BASE, 0x380 + word * 4, 0xffff_ffff);
    }
    cpu::dsb_sy();
    cpu::isb();

    let cfg = GicCpuConfig {
        priority_mask: 0xff,
        enable_group0: case.enable_group0,
        enable_group1: case.enable_group1,
        binary_point: BinaryPoint::Common(1),
        eoi_mode,
    };
    GicCpuInterface::configure(gic, &cfg).map_err(|err| map_err("gicc_configure", err))?;

    let gicd_ctlr = read_mmio32(GICD_BASE, 0x000);
    let gicc_ctlr = read_mmio32(GICC_BASE, 0x000);
    println!(
        "case {} mode {:?}: GICD_CTLR=0x{:08x} GICC_CTLR=0x{:08x}",
        case.label, eoi_mode, gicd_ctlr, gicc_ctlr
    );

    unsafe { asm!("msr daifclr, #0b1111") };
    let daif: u64;
    unsafe { asm!("mrs {}, daif", out(reg) daif) };
    println!("case {}: DAIF=0x{:x}", case.label, daif);

    // Clear SPI state before programming.
    let word = (SPI_ID / 32) as usize;
    let bit = 1u32 << (SPI_ID % 32);
    write_mmio32(GICD_BASE, 0x180 + word * 4, bit);
    write_mmio32(GICD_BASE, 0x280 + word * 4, bit);
    write_mmio32(GICD_BASE, 0x380 + word * 4, bit);

    let route = SpiRoute::Specific(cpu::get_current_core_id());
    GicDistributor::configure_spi(
        gic,
        SPI_ID,
        case.group,
        if matches!(case.group, IrqGroup::Group0) {
            0x00
        } else {
            0x80
        },
        TriggerMode::Edge,
        route,
        EnableOp::Enable,
    )
    .map_err(|err| map_err("configure_spi", err))?;

    let logical_group = gic
        .logical_group(SPI_ID)
        .map_err(|err| map_err("logical_group", err))?;
    let hw_igroupr = read_mmio32(GICD_BASE, 0x080 + word * 4);
    println!(
        "case {}: logical_group={:?} hw_igroupr=0x{:08x}",
        case.label, logical_group, hw_igroupr
    );

    // Set target to CPU0 explicitly.
    let spi_offset = SPI_ID.saturating_sub(32) as usize;
    let target_reg = spi_offset / 4;
    let target_byte = spi_offset % 4;
    write_mmio8(GICD_BASE, 0x800 + target_reg * 4 + target_byte, 0x01);

    write_mmio32(GICD_BASE, 0x280, 0xffff_ffff);
    write_mmio32(GICD_BASE, 0x380, 0xffff_ffff);
    cpu::dsb_sy();
    cpu::isb();

    GicDistributor::set_pending(gic, SPI_ID, true).map_err(|err| map_err("set_pending", err))?;
    println!(
        "case {} HPPIR=0x{:08x} AHPPIR=0x{:08x}",
        case.label,
        read_mmio32(GICC_BASE, 0x018),
        read_mmio32(GICC_BASE, 0x028)
    );

    wait_for_spi(gic, case.group, eoi_mode)?;

    cpu::dsb_sy();
    cpu::isb();
    let ispendr = read_mmio32(GICD_BASE, 0x200 + word * 4);
    if (ispendr & bit) != 0 {
        dump_state(case.label, gic);
        return Err("spi_pending_bit_set");
    }

    Ok(())
}

fn wait_for_spi(gic: &Gicv2, group: IrqGroup, eoi_mode: EoiMode) -> Result<(), &'static str> {
    for _ in 0..200_000 {
        if let Some(ack) =
            GicCpuInterface::acknowledge(gic).map_err(|err| map_err("ack_wait", err))?
        {
            if ack.intid == SPI_ID {
                if ack.group != group {
                    dump_state("ack_group_mismatch", gic);
                    return Err("ack_group_mismatch");
                }
                GicCpuInterface::end_of_interrupt(gic, ack).map_err(|err| map_err("eoi", err))?;
                if matches!(eoi_mode, EoiMode::DropOnly) {
                    GicCpuInterface::deactivate(gic, ack).map_err(|err| map_err("dir", err))?;
                }
                return Ok(());
            }

            GicCpuInterface::end_of_interrupt(gic, ack)
                .map_err(|err| map_err("eoi_unexpected", err))?;
            if matches!(eoi_mode, EoiMode::DropOnly) {
                GicCpuInterface::deactivate(gic, ack)
                    .map_err(|err| map_err("dir_unexpected", err))?;
            }
        } else {
            cpu::isb();
        }
    }
    dump_state("timeout", gic);
    Err("timeout_waiting_spi")
}

fn dump_state(label: &str, gic: &Gicv2) {
    let gicc_ctlr = read_mmio32(GICC_BASE, 0x000);
    let gicc_iar = read_mmio32(GICC_BASE, 0x00c);
    let gicc_hppir = read_mmio32(GICC_BASE, 0x018);
    let gicc_ahppir = read_mmio32(GICC_BASE, 0x028);
    let gicd_igroupr = read_mmio32(GICD_BASE, 0x080 + (SPI_ID as usize / 32) * 4);
    let gicd_ispendr = read_mmio32(GICD_BASE, 0x200 + (SPI_ID as usize / 32) * 4);
    let gicd_isactiver = read_mmio32(GICD_BASE, 0x300 + (SPI_ID as usize / 32) * 4);
    let shadow_group = match gic.logical_group(SPI_ID) {
        Ok(IrqGroup::Group0) => "Group0",
        Ok(IrqGroup::Group1) => "Group1",
        Err(_) => "Err",
    };
    println!(
        "dump({}): GICC_CTLR=0x{:08x} IAR=0x{:08x} HPPIR=0x{:08x} AHPPIR=0x{:08x} IGROUPR=0x{:08x} ISPENDR=0x{:08x} ISACTIVER=0x{:08x} shadow_group={}",
        label,
        gicc_ctlr,
        gicc_iar,
        gicc_hppir,
        gicc_ahppir,
        gicd_igroupr,
        gicd_ispendr,
        gicd_isactiver,
        shadow_group
    );
}

fn read_mmio32(base: usize, offset: usize) -> u32 {
    unsafe { ptr::read_volatile((base + offset) as *const u32) }
}

fn write_mmio8(base: usize, offset: usize, value: u8) {
    unsafe { ptr::write_volatile((base + offset) as *mut u8, value) }
}

fn write_mmio32(base: usize, offset: usize, value: u32) {
    unsafe { ptr::write_volatile((base + offset) as *mut u32, value) }
}

fn map_err(label: &'static str, err: GicError) -> &'static str {
    println!("{}: {:?}", label, err);
    label
}

unsafe fn clear_bss() {
    let (start, end) = unsafe {
        (
            &__bss_start as *const u8 as usize,
            &__bss_end as *const u8 as usize,
        )
    };
    if end > start {
        unsafe {
            ptr::write_bytes(start as *mut u8, 0, end - start);
        }
    }
}

#[panic_handler]
fn panic_handler(info: &core::panic::PanicInfo<'_>) -> ! {
    println!("PANIC: {}", info);
    exit_failure();
}
