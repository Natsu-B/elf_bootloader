//! Raspberry Pi EL2 boot binary that prepares Linux handoff and virtualization state.

#![cfg_attr(all(test, target_arch = "aarch64"), feature(custom_test_frameworks))]
#![cfg_attr(
    all(test, target_arch = "aarch64"),
    test_runner(aarch64_unit_test::test_runner)
)]
#![cfg_attr(
    all(test, target_arch = "aarch64"),
    reexport_test_harness_main = "test_main"
)]
#![no_std]
#![no_main]
#![feature(generic_const_exprs)]
#![feature(sync_unsafe_cell)]

extern crate alloc;
mod dtb;
mod handler;
mod multicore;
mod pcie;
mod platform_irq;
mod stack_overflow;
mod vgic;
mod virtio_blk;

#[cfg(all(test, target_arch = "aarch64"))]
aarch64_unit_test::uboot_unit_test_harness!(aarch64_unit_test::init_default_uart);

use ::dtb::DtbParser;
use ::dtb::WalkError;
use alloc::boxed::Box;
use alloc::vec::Vec;
use allocator::define_global_allocator;
use arch_hal::cpu;
use arch_hal::debug_uart;
use arch_hal::exceptions;
use arch_hal::gic::GicCpuConfig;
use arch_hal::gic::GicCpuInterface;
use arch_hal::gic::GicDistributor;
use arch_hal::gic::MmioRegion;
use arch_hal::gic::dt_irq;
use arch_hal::gic::gicv2::Gicv2;
use arch_hal::paging::EL2Stage1PageTypes;
use arch_hal::paging::EL2Stage1Paging;
use arch_hal::paging::EL2Stage1PagingSetting;
use arch_hal::paging::Stage2AccessPermission;
use arch_hal::paging::Stage2PageTypes;
use arch_hal::paging::Stage2Paging;
use arch_hal::paging::Stage2PagingSetting;
use arch_hal::pl011::Pl011Uart;
use arch_hal::println;
use arch_hal::soc::bcm2712;
use arch_hal::soc::bcm2712::rp1::Rp1GpioBank0;
use arch_hal::soc::bcm2712::rp1::Rp1PeripheralMap;
use arch_hal::timer::SystemTimer;
use arch_hal::tls;
use core::alloc::Layout;
use core::arch::global_asm;
use core::cell::SyncUnsafeCell;
use core::fmt::Write;
use core::mem::MaybeUninit;
use core::ops::ControlFlow;
use core::panic::PanicInfo;
use core::ptr::NonNull;
use core::str;
use file::OpenOptions;
use file::StorageDevice;
use file::StorageDeviceErr;
use mutex::pod::RawAtomicPod;
use typestate::Le;

unsafe extern "C" {
    static mut _BSS_START: usize;
    static mut _BSS_END: usize;
    static mut _PROGRAM_START: usize;
    static mut _PROGRAM_END: usize;
    static mut _STACK_BOTTOM: usize;
    static mut _STACK_TOP: usize;
    static mut _LINUX_IMAGE: usize;
    static mut __el2_tls_bsp_start: usize;
    static mut __el2_tls_bsp_end: usize;
}

pub(crate) const SPSR_EL2_M_EL1H: u64 = 0b0101; // EL1 with SP_EL1(EL1h)
static LINUX_ADDR: SyncUnsafeCell<usize> = SyncUnsafeCell::new(0);
static DTB_ADDR: SyncUnsafeCell<usize> = SyncUnsafeCell::new(0);
pub(crate) static GICV2_DRIVER: SyncUnsafeCell<Option<Gicv2>> = SyncUnsafeCell::new(None);
pub(crate) const RP1_BASE: usize = 0x1c_0000_0000;
pub(crate) const GUEST_PL011_UART0_ADDR: (usize, u64) = (RP1_BASE + 0x3_0000, 48 * 1000 * 1000);
pub(crate) const HYPERVISOR_PL011_UART1_ADDR: (usize, u64) =
    (RP1_BASE + 0x3_4000, 48 * 1000 * 1000);
const RP1_GPIO_BANK0_BASE: usize = RP1_BASE + 0xd_0000;
const RP1_PAD_BANK0_BASE: usize = RP1_BASE + 0xf_0000;
const RP1_GPIO_CTRL_FUNCSEL_MASK: u32 = 0x1f;
const RP1_GPIO_CTRL_OUTOVER_MASK: u32 = 0x0000_3000;
const RP1_GPIO_CTRL_OEOVER_MASK: u32 = 0x0000_c000;
const RP1_GPIO_CTRL_INOVER_MASK: u32 = 0x0003_0000;
const RP1_GPIO_CTRL_OVERRIDE_MASK: u32 =
    RP1_GPIO_CTRL_OUTOVER_MASK | RP1_GPIO_CTRL_OEOVER_MASK | RP1_GPIO_CTRL_INOVER_MASK;
// The Raspberry Pi Linux RP1 pinctrl/DTS reference routes RP1 UART1 on GPIO0/1
// with function selector 2.
const RP1_GPIO_FUNCSEL_UART1: u32 = 0x02;
const RP1_PAD_SCHMITT: u32 = 1 << 1;
const RP1_PAD_PULL_SHIFT: u32 = 2;
const RP1_PAD_PULL_MASK: u32 = 0b11 << RP1_PAD_PULL_SHIFT;
const RP1_PAD_PULL_UP: u32 = 2 << RP1_PAD_PULL_SHIFT;
const RP1_PAD_INPUT_ENABLE: u32 = 1 << 6;
const RP1_PAD_OUTPUT_DISABLE: u32 = 1 << 7;
const RP1_PAD_RX_MASK: u32 = RP1_PAD_SCHMITT | RP1_PAD_PULL_MASK | RP1_PAD_INPUT_ENABLE;

#[derive(Copy, Clone, Debug)]
struct Gicv2Info {
    dist: MmioRegion,
    cpu: MmioRegion,
    gich: Option<MmioRegion>,
    gicv: Option<MmioRegion>,
    maintenance_intid: Option<u32>,
}

#[repr(C)]
struct LinuxHeader {
    code0: u32,
    code1: u32,
    text_offset: Le<u64>,
    image_size: Le<u64>,
    flags: Le<u64>,
    res2: u64,
    res3: u64,
    res4: u64,
    magic: [u8; 4],
    res5: u32,
}

impl LinuxHeader {
    const MAGIC: [u8; 4] = [b'A', b'R', b'M', 0x64];
}

fn read_sd_boot_config(
    dev: &'static dyn block_device_api::BlockDevice,
) -> Result<file::AlignedSliceBox<u8>, StorageDeviceErr> {
    let storage = StorageDevice::from_ready_block_device(dev)?;
    let handle = storage.open(0, "/config.txt", &OpenOptions::Read)?;
    handle.read(1).map_err(StorageDeviceErr::FileSystemErr)
}

fn print_sd_boot_config(dev: &'static dyn block_device_api::BlockDevice) {
    match read_sd_boot_config(dev) {
        Ok(config_txt) => match str::from_utf8(&config_txt) {
            Ok(text) => {
                println!("sdhc-fs: /config.txt begin");
                println!("{}", text);
                println!("sdhc-fs: /config.txt end");
            }
            Err(err) => {
                println!(
                    "sdhc-fs: /config.txt is not valid utf-8: {:?} ({} bytes)",
                    err,
                    config_txt.len()
                );
            }
        },
        Err(err) => println!("sdhc-fs: failed to read /config.txt: {:?}", err),
    }
}

fn log_guest_virtio_blk_dtb(dtb_bytes: &[u8]) {
    let parser = match DtbParser::init(dtb_bytes.as_ptr() as usize) {
        Ok(parser) => parser,
        Err(err) => {
            println!("guest-dtb: failed to parse generated dtb: {}", err);
            return;
        }
    };

    let mut found = false;
    let result = parser.find_nodes_by_compatible_view("virtio,mmio", &mut |view,
                                                                           name|
     -> Result<
        ControlFlow<()>,
        WalkError<()>,
    > {
        let mut regs = view.reg_iter().map_err(WalkError::Dtb)?;
        let Some(entry) = regs.next() else {
            return Ok(ControlFlow::Continue(()));
        };
        let (base, size) = entry.map_err(WalkError::Dtb)?;
        println!(
            "guest-dtb: virtio node {} base=0x{:x} size=0x{:x} root=/dev/vda2 irq={}",
            name,
            base,
            size,
            virtio_blk::VIRTIO_BLK_IRQ_INTID
        );
        found = true;
        Ok(ControlFlow::Break(()))
    });
    match result {
        Ok(ControlFlow::Break(())) | Ok(ControlFlow::Continue(())) => {}
        Err(WalkError::Dtb(err)) => println!("guest-dtb: virtio node scan failed: {}", err),
        Err(WalkError::User(())) => println!("guest-dtb: unexpected virtio node scan user error"),
    }
    if !found {
        println!("guest-dtb: no virtio,mmio node found");
    }
}

#[cfg(not(all(test, target_arch = "aarch64")))]
define_global_allocator!(GLOBAL_ALLOCATOR, 4096);

#[cfg(all(test, target_arch = "aarch64"))]
static GLOBAL_ALLOCATOR: allocator::MemoryAllocator<4096, { allocator::levels!(4096) }> =
    allocator::MemoryAllocator::new();

#[cfg(not(all(test, target_arch = "aarch64")))]
global_asm!(
    r#"
.global _start
.section ".text.boot"

_start:
    msr spsel, #1
    isb
    ldr x0, =_STACK_TOP
    mov sp, x0
clear_bss:
    ldr x0, =_BSS_START
    ldr x1, =_BSS_END
clear_bss_loop:
    cmp x0, x1
    beq clear_bss_end
    str xzr, [x0], #8
    b clear_bss_loop
clear_bss_end:
    bl main
loop:
    wfe
    b loop
    "#
);

fn gpio_ctrl_addr(pin: usize) -> *mut u32 {
    (RP1_GPIO_BANK0_BASE + pin * 8 + 4) as *mut u32
}

fn pad_ctrl_addr(pin: usize) -> *mut u32 {
    (RP1_PAD_BANK0_BASE + 4 + pin * 4) as *mut u32
}

fn configure_rp1_uart1_pinmux() {
    // SAFETY: RP1 peripheral MMIO is identity-mapped in EL2 stage-1 at this point, and these are
    // naturally aligned 32-bit GPIO/PAD control registers for GPIO0/1.
    unsafe {
        for pin in [0, 1] {
            let ctrl_addr = gpio_ctrl_addr(pin);
            let ctrl = core::ptr::read_volatile(ctrl_addr)
                & !(RP1_GPIO_CTRL_FUNCSEL_MASK | RP1_GPIO_CTRL_OVERRIDE_MASK);
            core::ptr::write_volatile(ctrl_addr, ctrl | RP1_GPIO_FUNCSEL_UART1);
        }

        let tx_pad_addr = pad_ctrl_addr(0);
        let tx_pad =
            core::ptr::read_volatile(tx_pad_addr) & !(RP1_PAD_PULL_MASK | RP1_PAD_OUTPUT_DISABLE);
        core::ptr::write_volatile(tx_pad_addr, tx_pad);

        let rx_pad_addr = pad_ctrl_addr(1);
        let rx_pad = core::ptr::read_volatile(rx_pad_addr) & !RP1_PAD_RX_MASK;
        core::ptr::write_volatile(
            rx_pad_addr,
            rx_pad | RP1_PAD_SCHMITT | RP1_PAD_PULL_UP | RP1_PAD_INPUT_ENABLE,
        );
    }
}

#[cfg(not(all(test, target_arch = "aarch64")))]
#[unsafe(no_mangle)]
extern "C" fn main() -> ! {
    let program_start = &raw mut _PROGRAM_START as *const _ as usize;
    let program_end = &raw mut _PROGRAM_END as *const _ as usize;
    let linux_image = &raw mut _LINUX_IMAGE as *const _ as usize;
    let stack_top = &raw mut _STACK_TOP as *const _ as usize;
    let tls_bsp_start = &raw mut __el2_tls_bsp_start as *const _ as usize;
    let tls_bsp_end = &raw mut __el2_tls_bsp_end as *const _ as usize;

    // setup tls
    unsafe {
        tls::init_current_cpu(
            NonNull::new(tls_bsp_start as *mut u8).unwrap(),
            tls_bsp_end.checked_sub(tls_bsp_start).unwrap(),
        )
        .unwrap()
    };

    // EL2 uses RP1 UART1 for hypervisor-only logs.
    configure_rp1_uart1_pinmux();
    cpu::dsb_ish();
    cpu::isb();
    debug_uart::init(
        HYPERVISOR_PL011_UART1_ADDR.0,
        HYPERVISOR_PL011_UART1_ADDR.1,
        115200,
    );
    cpu::isb();
    cpu::dsb_ish();
    debug_uart::write("HelloWorld!!!");
    println!("debug uart starting...\r\n");

    println!("setup exception");
    exceptions::setup_exception();
    handler::setup_handler();

    const DTB_PTR: usize = 0x2000_0000;
    let dtb = DtbParser::init(DTB_PTR).unwrap();
    assert_eq!(cpu::get_current_el(), 2);
    let gic_info = find_gicv2_info(&dtb).unwrap();
    let uart_irq = vgic::UartIrq {
        pintid: bcm2712::pirq_hook::RP1_UART0_SPI,
        sense: arch_hal::gic::IrqSense::Level,
    };

    let mut systimer = SystemTimer::new();
    systimer.init();
    println!(
        "system counter frequency: {}Hz",
        systimer.counter_frequency_hz()
    );
    println!("setup allocator");
    GLOBAL_ALLOCATOR.init();
    let result = dtb.find_node(Some("memory"), None, &mut |addr, size| {
        println!("available region addr=0x{:X}, size=0x{:X}", addr, size);
        GLOBAL_ALLOCATOR
            .add_available_region(addr, size)
            .map_err(|_| WalkError::User(()))?;
        Ok(ControlFlow::Continue(()))
    });
    match result {
        Ok(ControlFlow::Continue(())) | Ok(ControlFlow::Break(())) => {}
        Err(WalkError::Dtb(err)) => panic!("{}", err),
        Err(WalkError::User(())) => panic!("find_node: allocator error"),
    }
    dtb.find_memory_reservation_block(&mut |addr, size| {
        println!("reserved (memreserve) addr=0x{:X}, size=0x{:X}", addr, size);
        GLOBAL_ALLOCATOR.add_reserved_region(addr, size).unwrap();
        ControlFlow::Continue(())
    });
    let result = dtb.find_reserved_memory_node(
        &mut |addr, size| {
            println!(
                "reserved (node static) addr=0x{:X}, size=0x{:X}",
                addr, size
            );
            GLOBAL_ALLOCATOR
                .add_reserved_region(addr, size)
                .map_err(|_| WalkError::User(()))?;
            Ok(ControlFlow::Continue(()))
        },
        &mut |size, align, alloc_range| {
            println!(
                "reserved (node dynamic) size=0x{:X}, align={:?}, range={:?}",
                size, align, alloc_range
            );
            let allocated = GLOBAL_ALLOCATOR
                .allocate_dynamic_reserved_region(size, align, alloc_range)
                .map_err(|_| WalkError::User(()))?;
            if allocated.is_some() {
                Ok(ControlFlow::Continue(()))
            } else {
                Err(WalkError::User(()))
            }
        },
    );
    match result {
        Ok(ControlFlow::Break(())) | Ok(ControlFlow::Continue(())) => {}
        Err(WalkError::Dtb(err)) => panic!("{}", err),
        Err(WalkError::User(())) => panic!("reserved-memory: allocator error"),
    }
    println!(
        "reserved program image addr=0x{:X}, size=0x{:X}",
        program_start,
        program_end - program_start
    );
    GLOBAL_ALLOCATOR
        .add_reserved_region(program_start, program_end - program_start)
        .unwrap();
    println!(
        "reserved dtb addr=0x{:X}, size=0x{:X}",
        DTB_PTR,
        dtb.get_size()
    );
    GLOBAL_ALLOCATOR
        .add_reserved_region(DTB_PTR, dtb.get_size())
        .unwrap();
    println!("get linux header");
    let linux_header = unsafe { &*(linux_image as *const LinuxHeader) };
    // check
    if linux_header.magic != LinuxHeader::MAGIC {
        panic!("invalid linux header");
    }
    let image_size = linux_header.image_size.read() as usize;
    let text_offset = linux_header.text_offset.read() as usize;
    let jump_addr = linux_image + text_offset;
    unsafe { *LINUX_ADDR.get() = jump_addr };

    GLOBAL_ALLOCATOR
        .add_reserved_region(linux_image, image_size)
        .unwrap();
    println!("finalizing allocator...");
    GLOBAL_ALLOCATOR.finalize().unwrap();
    println!("allocator free regions after finalize:");
    GLOBAL_ALLOCATOR
        .for_each_free_region(|addr, size| {
            println!("  free: addr=0x{:X}, size=0x{:X}", addr, size);
        })
        .unwrap();
    println!("allocator reserved regions after finalize:");
    GLOBAL_ALLOCATOR
        .for_each_reserved_region(|addr, size| {
            println!("  reserved: addr=0x{:X}, size=0x{:X}", addr, size);
        })
        .unwrap();
    println!("allocator setup success!!!");

    // setup paging
    println!("start paging...");
    let parange = match cpu::get_parange().unwrap() {
        cpu::registers::PARange::PA32bits4GB => 32,
        cpu::registers::PARange::PA36bits64GB => 36,
        cpu::registers::PARange::PA40bits1TB => 40,
        cpu::registers::PARange::PA42bits4TB => 42,
        cpu::registers::PARange::PA44bits16TB => 44,
        cpu::registers::PARange::PA48bits256TB => 48,
        cpu::registers::PARange::PA52bits4PB => 52,
        cpu::registers::PARange::PA56bits64PB => 56,
    };
    let ipa_space = 1usize << parange;
    let mut paging_data: Vec<Stage2PagingSetting> = Vec::new();
    let mut stage1_paging_data: Vec<EL2Stage1PagingSetting> = Vec::new();
    let result = dtb.find_node(Some("memory"), None, &mut |addr, size| {
        let memory_last = paging_data.last();
        let memory_last_addr = if let Some(memory_last) = memory_last {
            memory_last.ipa + memory_last.size
        } else {
            0
        };
        assert!(memory_last_addr <= addr);
        if memory_last_addr < addr {
            paging_data.push(Stage2PagingSetting {
                ipa: memory_last_addr,
                pa: memory_last_addr,
                size: addr - memory_last_addr,
                types: Stage2PageTypes::Device,
                perm: Stage2AccessPermission::ReadWrite,
            });
            stage1_paging_data.push(EL2Stage1PagingSetting {
                va: memory_last_addr,
                pa: memory_last_addr,
                size: addr - memory_last_addr,
                types: EL2Stage1PageTypes::Device,
            });
        }
        paging_data.push(Stage2PagingSetting {
            ipa: addr,
            pa: addr,
            size,
            types: Stage2PageTypes::Normal,
            perm: Stage2AccessPermission::ReadWrite,
        });
        stage1_paging_data.push(EL2Stage1PagingSetting {
            va: addr,
            pa: addr,
            size,
            types: EL2Stage1PageTypes::Normal,
        });
        Ok(ControlFlow::Continue(()))
    });
    match result {
        Ok(ControlFlow::Continue(())) | Ok(ControlFlow::Break(())) => {}
        Err(WalkError::Dtb(err)) => panic!("{}", err),
        Err(WalkError::User(())) => panic!("find_node: paging allocator error"),
    }
    let memory_last = paging_data.last().unwrap();
    let memory_last_addr = memory_last.ipa + memory_last.size;
    #[derive(Copy, Clone, Debug)]
    struct TrapWindow {
        start: usize,
        end: usize,
    }

    fn trap_window(start: usize, size: usize, label: &str) -> TrapWindow {
        const PAGE_SIZE: usize = 0x1000;
        let end = start
            .checked_add(size)
            .unwrap_or_else(|| panic!("trap window {} end overflow", label));
        assert!(start < end, "trap window {} is empty", label);
        assert_eq!(
            start & (PAGE_SIZE - 1),
            0,
            "trap window {} start not 4KB aligned",
            label
        );
        assert_eq!(
            end & (PAGE_SIZE - 1),
            0,
            "trap window {} end not 4KB aligned",
            label
        );
        TrapWindow { start, end }
    }

    fn push_stage2_gap(paging_data: &mut Vec<Stage2PagingSetting>, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let size = end
            .checked_sub(start)
            .expect("stage2 device mapping underflow");
        paging_data.push(Stage2PagingSetting {
            ipa: start,
            pa: start,
            size,
            types: Stage2PageTypes::Device,
            perm: Stage2AccessPermission::ReadWrite,
        });
    }

    let mut trap_windows = [
        trap_window(gic_info.dist.base, gic_info.dist.size, "gicd"),
        trap_window(gic_info.cpu.base, gic_info.cpu.size, "gicc"),
        trap_window(GUEST_PL011_UART0_ADDR.0, 0x1000, "guest-uart0"),
        trap_window(HYPERVISOR_PL011_UART1_ADDR.0, 0x1000, "hypervisor-uart1"),
        trap_window(
            virtio_blk::VIRTIO_BLK_MMIO_BASE,
            virtio_blk::VIRTIO_BLK_MMIO_SIZE,
            "virtio-blk-mmio",
        ),
    ];
    trap_windows.sort_unstable_by_key(|window| window.start);

    let mut cursor = memory_last_addr;
    for window in trap_windows.iter() {
        assert!(
            window.start >= cursor,
            "trap windows overlap or go backwards (start=0x{:X}, cursor=0x{:X})",
            window.start,
            cursor
        );
        assert!(
            window.end <= ipa_space,
            "trap window exceeds IPA space (end=0x{:X}, ipa_space=0x{:X})",
            window.end,
            ipa_space
        );
        push_stage2_gap(&mut paging_data, cursor, window.start);
        cursor = window.end;
    }
    push_stage2_gap(&mut paging_data, cursor, ipa_space);
    stage1_paging_data.push(EL2Stage1PagingSetting {
        va: memory_last_addr,
        pa: memory_last_addr,
        size: ipa_space - memory_last_addr,
        types: EL2Stage1PageTypes::Device,
    });

    let guard_start = &raw const _STACK_BOTTOM as usize;
    let guard_end = guard_start.checked_add(0x1000).expect("guard end overflow");
    assert_eq!(guard_start & 0xFFF, 0, "guard start not 4KB aligned");
    assert_eq!(guard_end & 0xFFF, 0, "guard end not 4KB aligned");

    let mut guard_removed = false;
    let mut stage1_sanitized = Vec::with_capacity(stage1_paging_data.len().saturating_add(1));
    for setting in stage1_paging_data.into_iter() {
        let start = setting.va;
        let end = start
            .checked_add(setting.size)
            .expect("EL2 Stage-1 mapping end overflow");
        if guard_end <= start || guard_start >= end {
            stage1_sanitized.push(setting);
            continue;
        }

        guard_removed = true;
        if start < guard_start {
            let left_size = guard_start
                .checked_sub(start)
                .expect("EL2 Stage-1 left size underflow");
            assert_eq!(start & 0xFFF, 0, "EL2 Stage-1 left start not aligned");
            assert_eq!(left_size & 0xFFF, 0, "EL2 Stage-1 left size not aligned");
            stage1_sanitized.push(EL2Stage1PagingSetting {
                va: start,
                pa: setting.pa,
                size: left_size,
                types: setting.types,
            });
        }
        if guard_end < end {
            let right_size = end
                .checked_sub(guard_end)
                .expect("EL2 Stage-1 right size underflow");
            let offset = guard_end
                .checked_sub(start)
                .expect("EL2 Stage-1 right offset underflow");
            let right_pa = setting
                .pa
                .checked_add(offset)
                .expect("EL2 Stage-1 right pa overflow");
            assert_eq!(guard_end & 0xFFF, 0, "EL2 Stage-1 right start not aligned");
            assert_eq!(right_size & 0xFFF, 0, "EL2 Stage-1 right size not aligned");
            stage1_sanitized.push(EL2Stage1PagingSetting {
                va: guard_end,
                pa: right_pa,
                size: right_size,
                types: setting.types,
            });
        }
    }
    assert!(
        guard_removed,
        "guard page mapping was not removed from EL2 Stage-1 settings"
    );
    stage1_paging_data = stage1_sanitized;

    println!("Stage2Paging: {:#?}", paging_data);
    println!("EL2Stage1Paging: {:#?}", stage1_paging_data);
    Stage2Paging::init_stage2paging(&paging_data, &GLOBAL_ALLOCATOR).unwrap();
    Stage2Paging::enable_stage2_translation(true, false);
    EL2Stage1Paging::init_stage1paging(&stage1_paging_data).unwrap();
    println!("EL2 Stage-1 paging enabled");

    // SAFETY: emergency stack is initialized after Stage-1 is enabled.
    // The stack is mapped as Normal memory with identity mapping.
    unsafe {
        stack_overflow::init_emergency_stack();
    }
    println!("Emergency stack initialized");
    println!("paging success!!!");

    // Raw atomics are unsynchronized; enable only once during BSP bring-up after paging/caches.
    // IRQ/FIQ remain masked at this point and are unmasked later via `msr daifclr`.
    mutex::enable_raw_atomics();

    // setup gicv2 (with virtualization)
    println!("setup gicv2...");
    let virt = match (gic_info.gich, gic_info.gicv, gic_info.maintenance_intid) {
        (Some(gich), Some(gicv), Some(maint)) => {
            Some(arch_hal::gic::gicv2::Gicv2VirtualizationRegion {
                gich,
                gicv,
                maintenance_interrupt_id: maint,
            })
        }
        _ => None,
    }
    .expect("gic: missing virtualization region or maintenance interrupt");
    println!("gic v2: {:?}", gic_info);
    let gicv2 = Gicv2::new(gic_info.dist, gic_info.cpu, Some(virt), None).unwrap();
    gicv2.init_distributor().unwrap();
    let caps = gicv2.init_cpu_interface().unwrap();
    println!("GICv2 CPU Interface capabilities: {:?}", caps);
    gicv2
        .configure(&GicCpuConfig {
            priority_mask: 0xff,
            enable_group0: caps.supports_group0,
            enable_group1: true,
            binary_point: arch_hal::gic::BinaryPoint::Common(caps.binary_points_min),
            eoi_mode: arch_hal::gic::EoiMode::DropOnly,
        })
        .unwrap();

    handler::register_gic(gicv2);
    let gicv2 = handler::gic().unwrap();

    let result = dtb.find_nodes_by_compatible_view("brcm,bcm2712-pcie", &mut |view, _name| {
        pcie::init_pcie_with_node(view)
    });
    match result {
        Ok(ControlFlow::Continue(())) | Ok(ControlFlow::Break(())) => {}
        Err(WalkError::Dtb(err)) => panic!("{}", err),
        Err(WalkError::User(())) => panic!("pcie: unexpected user error"),
    }

    // check rp1
    let rp1 = bcm2712::init_rp1(&dtb).unwrap_or_else(|err| panic!("rp1 init failed: {:?}", err));
    println!(
        "RP1: Peripheral addr: 0x{:x} SRAM addr: 0x{:x}",
        rp1.peripheral_addr.unwrap().0,
        rp1.shared_sram_addr.unwrap().0
    );

    let rp1_map = Rp1PeripheralMap::from_config(&rp1)
        .unwrap_or_else(|err| panic!("rp1 map failed: {:?}", err));
    let rp1_gpio = Rp1GpioBank0::from_map(&rp1_map)
        .unwrap_or_else(|err| panic!("rp1 gpio map failed: {:?}", err));

    println!(
        "RP1 UART0 pinmux before: gpio14 ctrl=0x{:08x} pad=0x{:08x}; gpio15 ctrl=0x{:08x} pad=0x{:08x}",
        rp1_gpio.gpio_ctrl(14),
        rp1_gpio.pad_ctrl(14),
        rp1_gpio.gpio_ctrl(15),
        rp1_gpio.pad_ctrl(15),
    );
    rp1_gpio.configure_uart0_14_15_like_linux();
    println!(
        "RP1 UART0 pinmux after: gpio14 ctrl=0x{:08x} pad=0x{:08x}; gpio15 ctrl=0x{:08x} pad=0x{:08x}",
        rp1_gpio.gpio_ctrl(14),
        rp1_gpio.pad_ctrl(14),
        rp1_gpio.gpio_ctrl(15),
        rp1_gpio.pad_ctrl(15),
    );

    println!("setup vgic...");
    vgic::init(gicv2, &gic_info, Some(uart_irq)).unwrap();
    vgic::set_pirq_hook(Some(bcm2712::PIRQ_LIFECYCLE_HOOK)).unwrap();
    println!("vgic setup success!!!");

    // Arm the physical RP1 UART SPI only after the RP1 hook and vGIC passthrough are ready.
    gicv2
        .configure_spi(
            uart_irq.pintid,
            arch_hal::gic::IrqGroup::Group1,
            0x80,
            // RP1 translates the PL011 level source into an MSI-X message, so the host GIC sees
            // an edge even though the guest DT describes the UART interrupt as level-sensitive.
            arch_hal::gic::TriggerMode::Edge,
            arch_hal::gic::SpiRoute::Specific(cpu::get_current_core_id()),
            arch_hal::gic::EnableOp::Enable,
        )
        .unwrap();

    apply_guest_timer_policy_for_current_cpu();

    println!("gicv2 setup success!!!");

    unsafe {
        core::arch::asm!("msr daifclr, #3", options(nostack, preserves_flags)); // enable irq
    }

    multicore::setup_multicore(stack_top);

    let mut modified: Box<[MaybeUninit<u8>]> = Box::new_uninit_slice(dtb.get_size());
    unsafe {
        core::ptr::copy_nonoverlapping(
            DTB_PTR as *const u8,
            modified.as_mut_ptr() as *mut u8,
            dtb.get_size(),
        )
    };
    let modified = unsafe { modified.assume_init() };
    let dtb_modified = DtbParser::init(modified.as_ptr() as usize).unwrap();
    println!("set up linux data");

    let virtio_blk_backend = match bcm2712::sdhc::init_from_dtb(&dtb_modified) {
        Ok(backend) => backend,
        Err(err) => panic!("virtio-blk: sdhc init failed: {:?}", err),
    };
    print_sd_boot_config(virtio_blk_backend);
    virtio_blk::init_with_backend(virtio_blk_backend)
        .unwrap_or_else(|err| panic!("virtio-blk: backend install failed: {err}"));
    println!("virtio-blk: enabled with bcm2712 sdhc backend");

    let mut reserved_memory = GLOBAL_ALLOCATOR.trim_for_boot(0x1000 * 0x1000 * 1).unwrap();
    println!("allocator closed");
    let mut allocator_regions = Vec::new();
    GLOBAL_ALLOCATOR
        .for_each_free_region(|addr, size| allocator_regions.push((addr, size)))
        .unwrap();
    reserved_memory.extend_from_slice(&allocator_regions);
    reserved_memory.push((program_start, program_end - program_start));
    reserved_memory.push((DTB_PTR, dtb.get_size()));

    let dtb_box =
        dtb::build_guest_dtb(&dtb_modified, &reserved_memory, &gic_info, uart_irq).unwrap();
    log_guest_virtio_blk_dtb(&dtb_box);
    unsafe { *DTB_ADDR.get() = dtb_box.as_ptr() as usize };
    cpu::clean_dcache_poc(dtb_box.as_ptr() as usize, dtb_box.len());
    core::mem::forget(dtb_box);

    println!("jumping linux...\njump addr: 0x{:X}", jump_addr as usize);
    let el1_main = el1_main as *const fn() as usize as u64;
    let stack_addr =
        unsafe { alloc::alloc::alloc(Layout::from_size_align_unchecked(0x1000, 0x1000)) } as usize
            + 0x1000;
    println!(
        "el1_main addr: 0x{:X}\nsp_el1 addr: 0x{:X}",
        el1_main, stack_addr
    );

    // Stop EL2-side debug UART interrupt ownership before guest entry.
    debug_uart::detach_for_guest_passthrough();

    // Install an EL1 vector table so that early guest faults are captured.
    exceptions::setup_el1_exception();

    cpu::clean_dcache_poc(LINUX_ADDR.get() as usize, size_of::<usize>());
    cpu::clean_dcache_poc(DTB_ADDR.get() as usize, size_of::<usize>());

    cpu::invalidate_icache_all();
    // SAFETY: The EL2 boot path is about to transfer control to the prepared EL1 entry point
    // with a freshly allocated EL1 stack and the expected saved program state.
    unsafe {
        core::arch::asm!("msr spsr_el2, {}", in(reg) SPSR_EL2_M_EL1H);
        core::arch::asm!("msr elr_el2, {}", in(reg) el1_main);
        core::arch::asm!("msr sp_el1, {}", in(reg) stack_addr);
        cpu::isb();
        core::arch::asm!("eret", options(noreturn));
    }
}

extern "C" fn el1_main() {
    // jump linux
    unsafe {
        core::arch::asm!("msr daifset, #0xf", options(nostack, preserves_flags));

        core::mem::transmute::<usize, extern "C" fn(usize, usize, usize, usize)>(*LINUX_ADDR.get())(
            *DTB_ADDR.get(),
            0,
            0,
            0,
        );
    }
}

pub(crate) fn apply_guest_timer_policy_for_current_cpu() {
    const CNTHCTL_EL2_EL1PCTEN: u64 = 1 << 0;
    const CNTHCTL_EL2_EL1PCEN: u64 = 1 << 1;
    const LOWER_EL_TIMER_CTL_IMASK: u64 = 1 << 1;
    const MDCR_EL2_TPMCR: u64 = 1 << 5;
    const MDCR_EL2_TPM: u64 = 1 << 6;

    // Hide EL1 physical counter/timer access from the guest.
    // This assumes non-VHE (E2H=0); with E2H=1, the CNTHCTL_EL2 field positions shift by 10.
    let mut cnthctl_el2 = cpu::get_cnthctl_el2();
    cnthctl_el2 &= !(CNTHCTL_EL2_EL1PCTEN | CNTHCTL_EL2_EL1PCEN);
    cpu::set_cnthctl_el2(cnthctl_el2);

    // Keep timer offset policy explicit and consistent across BSP/APs.
    cpu::set_cntvoff_el2(0);

    // Sanitize stale lower-EL timer control state before guest IRQs are unmasked.
    cpu::set_cntv_ctl_el0(LOWER_EL_TIMER_CTL_IMASK);
    cpu::set_cntp_ctl_el0(LOWER_EL_TIMER_CTL_IMASK);

    // Leave PMU counters control (HPMN, etc.) unchanged, but let EL1/EL0 PMU sysregs run
    // without trapping if PMU overflow PPI 23 is enabled/passed through.
    let mut mdcr_el2 = cpu::get_mdcr_el2();
    mdcr_el2 &= !(MDCR_EL2_TPM | MDCR_EL2_TPMCR);
    cpu::set_mdcr_el2(mdcr_el2);

    cpu::isb();
}

fn find_gicv2_info(dtb: &DtbParser) -> Result<Gicv2Info, &'static str> {
    const COMPATS: [&str; 13] = [
        "arm,arm1176jzf-devchip-gic",
        "arm,arm11mp-gic",
        "arm,cortex-a15-gic",
        "arm,cortex-a7-gic",
        "arm,cortex-a9-gic",
        "arm,eb11mp-gic",
        "arm,gic-400",
        "arm,pl390",
        "arm,tc11mp-gic",
        "brcm,brahma-b15-gic",
        "nvidia,tegra210-agic",
        "qcom,msm-8660-qgic",
        "qcom,msm-qgic2",
    ];
    let mut found: Option<Gicv2Info> = None;
    for compat in COMPATS {
        let result = dtb.find_nodes_by_compatible_view(compat, &mut |view,
                                                                     _name|
         -> Result<
            ControlFlow<()>,
            WalkError<()>,
        > {
            println!("found GICv2 node: {}", compat);
            let mut regs = view.reg_iter().map_err(WalkError::Dtb)?;
            let Some(Ok((dist_base, _dist_size))) = regs.next() else {
                return Ok(ControlFlow::Continue(()));
            };
            let Some(Ok((cpu_base, _cpu_size))) = regs.next() else {
                return Ok(ControlFlow::Continue(()));
            };
            let gich = regs
                .next()
                .and_then(|r| r.ok())
                .map(|(base, _size)| MmioRegion { base, size: 0x1000 });
            let gicv = regs
                .next()
                .and_then(|r| r.ok())
                .map(|(base, _size)| MmioRegion { base, size: 0x2000 });
            let mut maintenance_intid = None;
            let _ = view.for_each_interrupt_specifier(&mut |cells| -> Result<
                ControlFlow<()>,
                WalkError<()>,
            > {
                if maintenance_intid.is_some() {
                    return Ok(ControlFlow::Break(()));
                }
                let decoded = dt_irq::decode_dt_irq(cells)
                    .map_err(|_| WalkError::Dtb("gic: invalid maintenance interrupt"))?;
                maintenance_intid = Some(decoded.intid);
                Ok(ControlFlow::Break(()))
            })?;

            found = Some(Gicv2Info {
                dist: MmioRegion {
                    base: dist_base,
                    size: 0x1000,
                },
                cpu: MmioRegion {
                    base: cpu_base,
                    size: 0x2000,
                },
                gich,
                gicv,
                maintenance_intid,
            });
            Ok(ControlFlow::Break(()))
        });
        match result {
            Ok(ControlFlow::Continue(())) | Ok(ControlFlow::Break(())) => {}
            Err(WalkError::Dtb(err)) => return Err(err),
            Err(WalkError::User(())) => return Err("gic: unexpected user error"),
        }
        if found.is_some() {
            break;
        }
    }
    found.ok_or("gic: missing GICv2 node")
}

#[cfg(not(all(test, target_arch = "aarch64")))]
static ALREADY_PANICKED: RawAtomicPod<bool> = unsafe { RawAtomicPod::new_raw_unchecked(false) };

#[cfg(not(all(test, target_arch = "aarch64")))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    if ALREADY_PANICKED.swap(true, core::sync::atomic::Ordering::AcqRel) {
        loop {}
    }
    let mut debug_uart =
        Pl011Uart::new(HYPERVISOR_PL011_UART1_ADDR.0, HYPERVISOR_PL011_UART1_ADDR.1);
    debug_uart.init(115200);
    debug_uart.write("\r\n\r\n=================================\r\n");
    debug_uart.write("kernel panicked!!!\r\n\r\n\r\n");
    if let Some(core_id) = tls::cpu_if_maybe_uninit() {
        let _ = debug_uart.write("core ");
        // # Safety rpi5 core_id must be 0~3
        let _ = debug_uart.write_char((core_id + b'0') as char);
        let _ = debug_uart.write_fmt(format_args!(
            "({:?
            }): ",
            cpu::get_current_core_id()
        ));
    } else {
        debug_uart.write("core ?: ");
    }
    debug_uart.write("panicked!!!\r\n");
    let _ = debug_uart.write_fmt(format_args!("PANIC: {}", info));
    debug_uart.write("\r\n=================================\r\n");
    loop {}
}
