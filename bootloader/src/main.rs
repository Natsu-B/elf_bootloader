//! Hypervisor bootloader binary entrypoint and platform bring-up flow.

#![feature(once_cell_get_mut)]
#![feature(sync_unsafe_cell)]
#![feature(generic_const_exprs)]
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
#![recursion_limit = "256"]

extern crate alloc;

#[cfg(all(feature = "rpi4_net", feature = "virtio_net"))]
compile_error!("rpi4_net and virtio_net cannot be enabled together");
mod debug;
mod gdb_stream;
#[cfg(not(any(feature = "rpi4_net", feature = "virtio_net")))]
mod gdb_uart;
#[cfg(any(feature = "rpi4_net", feature = "virtio_net"))]
#[path = "gdb_uart_udp.rs"]
mod gdb_uart;
mod build_info {
    include!(concat!(env!("OUT_DIR"), "/build_info.rs"));
}

#[cfg(all(feature = "rpi4", feature = "rpi4_genet_loopback_selftest"))]
mod genet_selftest;
mod handler;
mod irq_decode;
mod irq_monitor;
mod monitor;
#[cfg(any(feature = "rpi4_net", feature = "virtio_net"))]
mod net;
mod softirq;
mod vbar;
mod vbar_watch;
mod vgic;

#[cfg(all(test, target_arch = "aarch64"))]
aarch64_unit_test::uboot_unit_test_harness!(aarch64_unit_test::init_default_uart);
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use allocator::define_global_allocator;
use arch_hal::cpu;
use arch_hal::debug_uart;
use arch_hal::exceptions;
use arch_hal::gic;
use arch_hal::gic::BinaryPoint;
use arch_hal::gic::EnableOp;
use arch_hal::gic::EoiMode;
use arch_hal::gic::GicCpuConfig;
use arch_hal::gic::GicCpuInterface;
use arch_hal::gic::GicDistributor;
use arch_hal::gic::GicPpi;
use arch_hal::gic::IrqGroup;
use arch_hal::gic::SpiRoute;
use arch_hal::gic::TriggerMode;
use arch_hal::paging::Stage2AccessPermission;
use arch_hal::paging::Stage2PageTypes;
use arch_hal::paging::Stage2Paging;
use arch_hal::paging::Stage2PagingSetting;
use arch_hal::paging::stage1::EL2Stage1PageTypes;
use arch_hal::paging::stage1::EL2Stage1Paging;
use arch_hal::paging::stage1::EL2Stage1PagingSetting;
use arch_hal::pl011::Pl011Uart;
use arch_hal::println;
#[cfg(feature = "rpi4_net")]
use arch_hal::soc::bcm2711::genet::Bcm2711GenetV5;
#[cfg(all(
    feature = "rpi4",
    not(any(feature = "rpi4_net", feature = "virtio_net"))
))]
use arch_hal::soc::bcm2711::gpio::Bcm2711Gpio;
#[cfg(all(
    feature = "rpi4",
    not(any(feature = "rpi4_net", feature = "virtio_net"))
))]
use arch_hal::soc::bcm2711::gpio::Bcm2711GpioError;
#[cfg(all(
    feature = "rpi4",
    not(any(feature = "rpi4_net", feature = "virtio_net"))
))]
use arch_hal::soc::bcm2711::gpio::Pull;
#[cfg(all(
    feature = "rpi4",
    not(any(feature = "rpi4_net", feature = "virtio_net"))
))]
use arch_hal::soc::bcm2711::gpio::gpio_mmio_from_dtb;
use arch_hal::timer;
use arch_hal::timer::SystemTimer;
use arch_hal::tls;
use core::arch::naked_asm;
use core::cell::SyncUnsafeCell;
use core::ffi::CStr;
use core::ffi::c_char;
use core::fmt::Write;
use core::mem::MaybeUninit;
use core::ops::ControlFlow;
use core::panic::PanicInfo;
use core::ptr::NonNull;
use core::ptr::slice_from_raw_parts;
use core::slice;
#[cfg(feature = "rpi4_net")]
use core::time::Duration;
use core::usize;
use dtb::DeviceTree;
use dtb::DeviceTreeEditExt;
use dtb::DeviceTreeQueryExt;
use dtb::DtbNodeView;
use dtb::DtbParser;
use dtb::NameRef;
use dtb::NodeEditExt;
use dtb::NodeQueryExt;
use dtb::ValueRef;
use dtb::WalkError;
use file::AlignedSliceBox;

unsafe extern "C" {
    static mut _BSS_START: usize;
    static mut _BSS_END: usize;
    static mut _PROGRAM_START: usize;
    static mut _PROGRAM_END: usize;
    static mut _STACK_TOP: usize;
    static mut __el2_tls_bsp_start: usize;
    static mut __el2_tls_bsp_end: usize;
}

pub(crate) const SPSR_EL2_M_EL1H: u64 = 0b0101; // EL1 with SP_EL1(EL1h)
static DTB_ADDR: SyncUnsafeCell<usize> = SyncUnsafeCell::new(0);
pub(crate) static GUEST_UART: SyncUnsafeCell<Option<UartNode>> = SyncUnsafeCell::new(None);
pub(crate) static GDB_UART: SyncUnsafeCell<Option<UartNode>> = SyncUnsafeCell::new(None);
static DEBUG_UART_ADDR: SyncUnsafeCell<Option<usize>> = SyncUnsafeCell::new(None);

const MAX_MEM_REGIONS: usize = 8;
const PAGE_SIZE: usize = 0x1000;
const EL1_STACK_BYTES: usize = 0x4000; // 16 KiB EL1 stack
static MEM_REGION_COUNT: SyncUnsafeCell<usize> = SyncUnsafeCell::new(0);
static MEM_REGIONS: SyncUnsafeCell<[MemoryRegion; MAX_MEM_REGIONS]> =
    SyncUnsafeCell::new([MemoryRegion { base: 0, size: 0 }; MAX_MEM_REGIONS]);
static GUEST_MMIO_ALLOWLIST: SyncUnsafeCell<Option<Box<[GuestMmioRange]>>> =
    SyncUnsafeCell::new(None);

#[cfg(not(feature = "rpi4"))]
const PL011_UART_ADDR: usize = 0x900_0000;
#[cfg(feature = "rpi4")]
const PL011_UART_ADDR: usize = 0xFE20_1000;
#[cfg(all(
    feature = "rpi4",
    not(any(feature = "rpi4_net", feature = "virtio_net"))
))]
const GDB_UART_ADDR: usize = 0xFE20_1400;
const UART_CLOCK_HZ: u64 = 48 * 1_000_000;
const UART_BAUD: u32 = 115_200;
const EL2_TIMER_PPI_PRIORITY: u8 = 0x80; // Priority for the timeout monitor tick.

#[derive(Copy, Clone, Debug)]
pub(crate) struct UartNode {
    base: usize,
    size: usize,
    irq: Option<u32 /* intid */>,
}

#[derive(Copy, Clone, Debug)]
struct Pl011Candidate {
    uart_node: UartNode,
    name: &'static str,
    parsed_index: Option<u32>,
}

fn parse_decimal_prefix_u32(input: &str) -> Option<u32> {
    let bytes = input.as_bytes();
    let mut value: u32 = 0;
    let mut saw_digit = false;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            break;
        }
        saw_digit = true;
        value = value.checked_mul(10)?.checked_add((byte - b'0') as u32)?;
    }
    if saw_digit { Some(value) } else { None }
}

fn parse_uart_index_from_name(name: &str) -> Option<u32> {
    if let Some(rest) = name.strip_prefix("uart") {
        return parse_decimal_prefix_u32(rest);
    }
    if let Some(rest) = name.strip_prefix("serial") {
        return parse_decimal_prefix_u32(rest);
    }
    None
}

fn pl011_candidate_key(candidate: &Pl011Candidate) -> (u32, usize) {
    (
        candidate.parsed_index.unwrap_or(u32::MAX),
        candidate.uart_node.base,
    )
}

fn pl011_same_region(a: &Pl011Candidate, b: &Pl011Candidate) -> bool {
    a.uart_node.base == b.uart_node.base && a.uart_node.size == b.uart_node.size
}

fn update_best_two_pl011_candidates(
    best: &mut Option<Pl011Candidate>,
    second: &mut Option<Pl011Candidate>,
    candidate: Pl011Candidate,
) {
    if let Some(current_best) = best {
        if pl011_same_region(current_best, &candidate) {
            return;
        }
    } else {
        *best = Some(candidate);
        return;
    }

    if pl011_candidate_key(&candidate) < pl011_candidate_key(best.as_ref().unwrap()) {
        *second = *best;
        *best = Some(candidate);
        return;
    }

    if let Some(current_second) = second {
        if pl011_same_region(current_second, &candidate) {
            return;
        }
        if pl011_candidate_key(&candidate) < pl011_candidate_key(current_second) {
            *second = Some(candidate);
        }
    } else {
        *second = Some(candidate);
    }
}

fn log_selected_uart(role: &str, candidate: Pl011Candidate) {
    match candidate.parsed_index {
        Some(index) => println!(
            "{} uart selected: {} (idx {}) @ 0x{:X}",
            role, candidate.name, index, candidate.uart_node.base
        ),
        None => println!(
            "{} uart selected: {} (idx none) @ 0x{:X}",
            role, candidate.name, candidate.uart_node.base
        ),
    }
}

#[cfg(any(feature = "rpi4_net", feature = "virtio_net"))]
fn sync_pending_async_serror() {
    cpu::dsb_sy();
    cpu::isb();
    // SAFETY: `hint #16` is the architectural ESB encoding and is used here to synchronize
    // pending asynchronous SError to this exact boot transition point.
    unsafe {
        core::arch::asm!("hint #16", options(nostack, preserves_flags));
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct MemoryRegion {
    base: usize,
    size: usize,
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct GuestMmioRange {
    base: usize,
    size: usize,
}

impl GuestMmioRange {
    fn end(&self) -> usize {
        self.base.checked_add(self.size).unwrap_or(usize::MAX)
    }

    fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.end()
    }
}

#[derive(Copy, Clone, Debug)]
struct Gicv2Info {
    dist: gic::MmioRegion,
    cpu: gic::MmioRegion,
    gich: Option<gic::MmioRegion>,
    gicv: Option<gic::MmioRegion>,
    maintenance_intid: Option<u32>,
}

#[cfg(not(all(test, target_arch = "aarch64")))]
define_global_allocator!(GLOBAL_ALLOCATOR, 4096);

#[cfg(all(test, target_arch = "aarch64"))]
static GLOBAL_ALLOCATOR: allocator::MemoryAllocator<4096, { allocator::levels!(4096) }> =
    allocator::MemoryAllocator::new();

#[unsafe(naked)]
#[cfg(not(all(test, target_arch = "aarch64")))]
#[unsafe(no_mangle)]
extern "C" fn _start() {
    naked_asm!(
        r#"
        msr spsel, #1
        isb
        ldr x9, =_STACK_TOP
        mov sp, x9
    clear_bss:
        ldr x9, =_BSS_START
        ldr x10, =_BSS_END
    clear_bss_loop:
        cmp x9, x10
        beq clear_bss_end
        str xzr, [x9], #8
        b clear_bss_loop
    clear_bss_end:
        bl main
    loop:
        wfe
        b loop
        "#
    )
}

#[cfg(not(all(test, target_arch = "aarch64")))]
#[unsafe(no_mangle)]
extern "C" fn main(argc: usize, argv: *const *const u8) -> ! {
    let sp_el1 = {
        let program_start = &raw mut _PROGRAM_START as *const _ as usize;
        let stack_start = &raw mut _STACK_TOP as *const _ as usize;
        let el2_tls_bsp_start = &raw mut __el2_tls_bsp_start as *const _ as usize;
        let el2_tls_bsp_end = &raw mut __el2_tls_bsp_end as *const _ as usize;

        let dtb_ptr = if cfg!(feature = "rpi4") {
            0x2000_0000
        } else {
            let args = unsafe { slice::from_raw_parts(argv, argc) };
            str_to_usize(unsafe { CStr::from_ptr(args[0] as *const c_char).to_str().unwrap() })
                .unwrap()
        };

        let dtb = DtbParser::init(dtb_ptr).unwrap();
        let mut best: Option<Pl011Candidate> = None;
        let mut second: Option<Pl011Candidate> = None;
        let _ = dtb
            .find_nodes_by_compatible_view("arm,pl011", &mut |view,
                                                              name|
             -> Result<
                ControlFlow<()>,
                WalkError<()>,
            > {
                let reg = view.reg_iter().unwrap().next().unwrap().unwrap();
                let mut irq = None;
                let _ = view
                    .for_each_interrupt_specifier(&mut |cells| -> Result<
                        ControlFlow<()>,
                        WalkError<()>,
                    > {
                        irq =
                            Some(irq_decode::dt_irq_to_pintid(cells).expect("uart: bad IRQ spec"));
                        Ok(ControlFlow::Break(()))
                    })
                    .unwrap();
                update_best_two_pl011_candidates(
                    &mut best,
                    &mut second,
                    Pl011Candidate {
                        uart_node: UartNode {
                            base: reg.0,
                            size: reg.1,
                            irq,
                        },
                        name,
                        parsed_index: parse_uart_index_from_name(name),
                    },
                );
                Ok(ControlFlow::Continue(()))
            })
            .unwrap();
        let best = best.unwrap_or_else(|| panic!("uart selection: no arm,pl011 nodes found"));
        #[cfg(any(feature = "rpi4_net", feature = "virtio_net"))]
        let _ = second;
        #[cfg(not(any(feature = "rpi4_net", feature = "virtio_net")))]
        let gdb_candidate = Some(second.unwrap_or_else(|| {
            panic!("uart selection: need at least two distinct arm,pl011 nodes")
        }));
        #[cfg(any(feature = "rpi4_net", feature = "virtio_net"))]
        let gdb_candidate: Option<Pl011Candidate> = None;
        // SAFETY: early boot UART globals are initialized once on the BSP before secondary cores/interrupts.
        unsafe {
            *GUEST_UART.get() = Some(best.uart_node);
            *GDB_UART.get() = gdb_candidate.map(|candidate| candidate.uart_node);
            *DEBUG_UART_ADDR.get() = Some(best.uart_node.base);
        }
        let guest_uart = unsafe { (*GUEST_UART.get()).expect("guest uart is not initialized") };
        let gdb_uart = unsafe { *GDB_UART.get() };
        #[cfg(all(
            feature = "rpi4",
            not(any(feature = "rpi4_net", feature = "virtio_net"))
        ))]
        {
            let gdb_uart = gdb_uart.expect("gdb uart is not initialized");
            assert_eq!(
                guest_uart.base, PL011_UART_ADDR,
                "selected guest UART does not match expected PL011 address"
            );
            assert_eq!(
                gdb_uart.base, GDB_UART_ADDR,
                "selected gdb UART does not match expected PL011 address"
            );
        }
        #[cfg(all(feature = "rpi4", any(feature = "rpi4_net", feature = "virtio_net")))]
        {
            assert_eq!(
                guest_uart.base, PL011_UART_ADDR,
                "selected guest UART does not match expected PL011 address"
            );
        }
        debug_uart::init(guest_uart.base, UART_CLOCK_HZ, UART_BAUD);
        log_selected_uart("guest", best);
        exceptions::setup_el1_exception();
        println!("paging success!!!");
        println!("setup exception");
        exceptions::setup_exception();
        unsafe {
            cpu::write_daif(cpu::read_daif() & !(0b1 << 8) /* SError */)
        };

        if let Some(gdb_candidate) = gdb_candidate {
            log_selected_uart("gdb", gdb_candidate);
        }
        #[cfg(all(
            feature = "rpi4",
            feature = "rpi4_genet_loopback_selftest",
            not(any(feature = "rpi4_net", feature = "virtio_net"))
        ))]
        {
            genet_selftest::run_from_dtb(&dtb);
        }
        let tls_result = unsafe {
            tls::init_current_cpu(
                NonNull::new(el2_tls_bsp_start as *mut u8).unwrap(),
                el2_tls_bsp_end.checked_sub(el2_tls_bsp_start).unwrap(),
            )
        };
        if let Err(err) = tls_result {
            println!("tls init failed: {:?}", err);
            panic!("tls init failed");
        }

        #[cfg(not(any(feature = "rpi4_net", feature = "virtio_net")))]
        {
            let gdb_uart = gdb_uart.expect("gdb uart is not initialized");
            // enable gdb_uart gpio
            #[cfg(feature = "rpi4")]
            enable_uart2_gpio(&dtb);
            gdb_uart::init(gdb_uart.base, UART_CLOCK_HZ, UART_BAUD);
        }

        debug::init_gdb_stub();
        println!(
            "debug uart starting (guest console @ 0x{:X})...\r\n",
            guest_uart.base
        );
        let gic_info = find_gicv2_info(&dtb).unwrap();

        assert_eq!(cpu::get_current_el(), 2);

        let mut systimer = SystemTimer::new();
        systimer.init();
        println!(
            "system counter frequency: {}Hz",
            systimer.counter_frequency_hz()
        );
        println!("setup allocator");
        GLOBAL_ALLOCATOR.init();
        let _ = dtb
            .find_node(Some("memory"), None, &mut |addr,
                                                   size|
             -> Result<
                ControlFlow<()>,
                WalkError<()>,
            > {
                GLOBAL_ALLOCATOR.add_available_region(addr, size).unwrap();
                record_memory_region(addr, size);
                Ok(ControlFlow::Continue(()))
            })
            .unwrap();
        dtb.find_memory_reservation_block(&mut |addr, size| {
            GLOBAL_ALLOCATOR.add_reserved_region(addr, size).unwrap();
            ControlFlow::Continue(())
        });
        let result = dtb.find_reserved_memory_node(
            &mut |addr, size| {
                GLOBAL_ALLOCATOR
                    .add_reserved_region(addr, size)
                    .map_err(|_| WalkError::User(()))?;
                Ok(ControlFlow::Continue(()))
            },
            &mut |size, align, alloc_range| {
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
        GLOBAL_ALLOCATOR
            .add_reserved_region(program_start, stack_start - program_start)
            .unwrap();
        GLOBAL_ALLOCATOR
            .add_reserved_region(dtb_ptr, dtb.get_size())
            .unwrap();
        GLOBAL_ALLOCATOR.finalize().unwrap();
        println!("allocator setup success!!!");
        record_guest_mmio_allowlist_from_dtb(&dtb, &guest_uart, gdb_uart.as_ref(), &gic_info);
        dump_guest_mmio_allowlist();

        #[cfg(feature = "virtio_net")]
        {
            let vnet = net::virtio::init_from_dtb(&dtb);
            let eth = vnet as &'static mut dyn io_api::ethernet::EthernetFrameIo;
            net::udp_uart::init(eth, net::config::LOCAL_IP);
            arch_hal::set_mirror(Some(arch_hal::MirrorOps {
                write: net::udp_uart::debug_write_str,
                flush: net::udp_uart::debug_flush,
            }));
            gdb_uart::init(0, UART_CLOCK_HZ, UART_BAUD);
        }

        #[cfg(feature = "virtio_net")]
        {
            net::udp_uart::pause();
            arch_hal::set_mirror(None);
            sync_pending_async_serror();
        }

        println!("setup EL2 Stage-1 paging with stack guard...");
        let stage1_settings = build_stage1_el2_map();
        EL2Stage1Paging::init_stage1paging(&stage1_settings)
            .expect("EL2 Stage-1 paging initialization failed");
        #[cfg(any(feature = "rpi4_net", feature = "virtio_net"))]
        {
            sync_pending_async_serror();
        }
        println!("EL2 Stage-1 paging enabled");

        // SAFETY: emergency stack is initialized after Stage-1 is enabled.
        // The stack is mapped as Normal memory with identity mapping.
        unsafe {
            arch_hal::init_emergency_stack();
        }
        println!("Emergency stack initialized");

        #[cfg(feature = "rpi4_net")]
        let genet_ptr: *mut Bcm2711GenetV5 = {
            let genet =
                net::rpi4::init_genet_from_dtb_with_link_wait(&dtb, Some(Duration::from_secs(30)));
            #[cfg(all(feature = "rpi4", feature = "rpi4_genet_loopback_selftest"))]
            genet_selftest::run_with_driver(genet);
            let genet_ptr: *mut Bcm2711GenetV5 = genet;
            let eth = genet as &'static mut dyn io_api::ethernet::EthernetFrameIo;
            net::udp_uart::init(eth, net::config::LOCAL_IP);
            arch_hal::set_mirror(Some(arch_hal::MirrorOps {
                write: net::udp_uart::debug_write_str,
                flush: net::udp_uart::debug_flush,
            }));
            gdb_uart::init(0, UART_CLOCK_HZ, UART_BAUD);
            genet_ptr
        };

        #[cfg(feature = "rpi4_net")]
        {
            net::udp_uart::pause();
            arch_hal::set_mirror(None);
            assert!(
                !genet_ptr.is_null(),
                "rpi4_net: genet pointer must be initialized before quiesce"
            );
            // SAFETY: IRQs are still disabled at this point in boot flow, and UDP UART traffic
            // has been paused immediately above, so there is no concurrent access to `genet_ptr`.
            unsafe {
                (&mut *genet_ptr)
                    .quiesce_for_paging()
                    .expect("genet quiesce failed");
            }
            sync_pending_async_serror();
        }

        // setup paging
        println!("start paging...");
        println!(
            "guest uart addr: 0x{:X}, size: 0x{:X}",
            guest_uart.base, guest_uart.size
        );
        if let Some(gdb_uart) = gdb_uart {
            println!(
                "gdb uart addr: 0x{:X}, size: 0x{:X}",
                gdb_uart.base, gdb_uart.size
            );
        }
        let (paging_data, guest_window) = build_stage2_guest_map();
        let (guest_ipa_base, guest_ipa_size) = guest_window.expect("stage2: no guest RAM window");
        debug::set_guest_ipa_window(guest_ipa_base as u64, guest_ipa_size as u64);

        let guest_ipa_end = guest_ipa_base
            .checked_add(guest_ipa_size)
            .expect("stage2: guest_ipa_base + guest_ipa_size overflow");
        let sp_el1 = align_down(guest_ipa_end, 16);
        assert!(
            sp_el1 > guest_ipa_base,
            "stage2: sp_el1 ({:#X}) must be above guest_ipa_base ({:#X})",
            sp_el1,
            guest_ipa_base
        );
        assert!(
            sp_el1 <= guest_ipa_end,
            "stage2: sp_el1 ({:#X}) must be at or below guest_ipa_end ({:#X})",
            sp_el1,
            guest_ipa_end
        );
        assert!(
            sp_el1.checked_sub(0x20).unwrap_or(0) >= guest_ipa_base,
            "stage2: sp_el1 - 0x20 ({:#X}) underflows guest_ipa_base ({:#X})",
            sp_el1.wrapping_sub(0x20),
            guest_ipa_base
        );

        let el1_stack_base = align_down(
            sp_el1
                .checked_sub(EL1_STACK_BYTES)
                .expect("stage2: sp_el1 - EL1_STACK_BYTES underflow"),
            PAGE_SIZE,
        );
        assert!(
            el1_stack_base >= guest_ipa_base,
            "stage2: el1_stack_base ({:#X}) below guest_ipa_base ({:#X})",
            el1_stack_base,
            guest_ipa_base
        );
        assert!(
            el1_stack_base
                .checked_add(EL1_STACK_BYTES)
                .expect("stage2: el1_stack_base + EL1_STACK_BYTES overflow")
                <= guest_ipa_end,
            "stage2: el1 stack region exceeds guest_ipa_end"
        );

        if guest_ipa_size != 0 {
            let mut rom_ranges = [(0u64, 0u64); 1];
            let mut rom_count = 0usize;
            if let Some(mem_base) = lowest_memory_base() {
                if guest_ipa_base > mem_base {
                    rom_ranges[0] = (mem_base as u64, (guest_ipa_base - mem_base) as u64);
                    rom_count = 1;
                }
            }

            let mut io_ranges: Vec<(u64, u64)> = Vec::new();
            push_debug_io_range(&mut io_ranges, guest_uart.base, guest_uart.size);
            for range in guest_mmio_allowlist_slice() {
                if range.size == 0 {
                    continue;
                }
                push_debug_io_range(&mut io_ranges, range.base, range.size);
            }
            push_debug_io_range(&mut io_ranges, gic_info.dist.base, gic_info.dist.size);
            push_debug_io_range(&mut io_ranges, gic_info.cpu.base, gic_info.cpu.size);
            if let Some(gich) = gic_info.gich {
                push_debug_io_range(&mut io_ranges, gich.base, gich.size);
            }
            if let Some(gicv) = gic_info.gicv {
                push_debug_io_range(&mut io_ranges, gicv.base, gicv.size);
            }

            debug::set_memory_map(
                guest_ipa_base as u64,
                guest_ipa_size as u64,
                &rom_ranges[..rom_count],
                io_ranges.as_slice(),
            );
        } else {
            debug::set_memory_map(0, 0, &[], &[]);
        }
        if paging_data.is_empty() {
            panic!("stage2: no guest RAM to map");
        }
        println!("init stage2 paging...\npaging_data: {:?}", paging_data);
        Stage2Paging::init_stage2paging(&paging_data, &GLOBAL_ALLOCATOR).unwrap();
        Stage2Paging::enable_stage2_translation(true, true);
        #[cfg(any(feature = "rpi4_net", feature = "virtio_net"))]
        {
            #[cfg(feature = "rpi4_net")]
            {
                assert!(
                    !genet_ptr.is_null(),
                    "rpi4_net: genet pointer must be initialized before resume"
                );
                // SAFETY: IRQs are enabled later in boot flow; UDP UART stays paused until the
                // hardware resume below completes, so dereferencing `genet_ptr` is not concurrent.
                unsafe {
                    (&mut *genet_ptr).resume_after_paging();
                }
            }
            net::udp_uart::resume();
            arch_hal::set_mirror(Some(arch_hal::MirrorOps {
                write: net::udp_uart::debug_write_str,
                flush: net::udp_uart::debug_flush,
            }));
        }
        cpu::set_tpidr_el1(guest_uart.base as u64);
        handler::setup_handler();
        vbar_watch::init_vbar_watch();
        let (gic, gdb_uart_intid) = init_gicv2(&gic_info, gdb_uart).unwrap();
        gic.configure_ppi(
            timer::SBSA_EL2_PHYSICAL_TIMER_INTID,
            IrqGroup::Group1,
            EL2_TIMER_PPI_PRIORITY,
            TriggerMode::Level,
            EnableOp::Enable,
        )
        .map_err(|_| "gic: configure el2 timer ppi")
        .unwrap();
        irq_monitor::init_physical_timer_poll();
        handler::register_gic(gic, gdb_uart_intid);
        let gic = handler::gic().unwrap();
        vgic::init(gic, &gic_info, guest_uart, gdb_uart_intid).unwrap();
        {
            let mdcr = cpu::get_mdcr_el2();
            // Trap debug exceptions from lower EL to EL2 (MDCR_EL2.TDE).
            cpu::set_mdcr_el2(mdcr | (1 << 8));
        }
        cpu::enable_irq();
        cpu::enable_debug_exceptions();
        let modified = {
            let mut dtb_bytes = AlignedSliceBox::new_uninit_with_align(dtb.get_size(), 32).unwrap();
            dtb_bytes.copy_from_slice(unsafe {
                &*slice_from_raw_parts(dtb_ptr as *const MaybeUninit<u8>, dtb.get_size())
            });
            unsafe { dtb_bytes.assume_init() }
        };
        let mut dtb_tree = DeviceTree::from_dtb(&modified).unwrap().to_owned();
        apply_guest_dt_edits(
            &mut dtb_tree,
            unsafe { &*GUEST_UART.get() }.unwrap().base,
            &gic_info,
        )
        .unwrap();

        let mut reserved_memory = GLOBAL_ALLOCATOR.trim_for_boot(0x1000 * 0x1000 * 1).unwrap();
        println!("allocator closed");
        reserved_memory.push((program_start, stack_start));
        reserved_memory.push((el1_stack_base, EL1_STACK_BYTES));

        for (addr, size) in &reserved_memory {
            dtb_tree.mem_reserve.push(dtb::MemReserve {
                address: *addr as u64,
                size: *size as u64,
            });
        }
        let dtb_box = dtb_tree.into_dtb_box().unwrap();
        let (dtb_ptr, _dtb_len, _dtb_align) = allocator::AlignedSliceBox::into_raw_parts(dtb_box);
        // SAFETY: the DTB allocation is intentionally leaked so the guest can access it.
        unsafe {
            *DTB_ADDR.get() = dtb_ptr as usize;
        }
        println!("jumping linux...");

        unsafe {
            core::arch::asm!("isb");
            core::arch::asm!("dsb sy");
        }
        sp_el1
    };
    let el1_main = el1_main as *const fn() as usize as u64;
    println!(
        "el1_main addr: 0x{:X}\nsp_el1 addr: 0x{:X}",
        el1_main, sp_el1
    );

    // Print HyprProbe ascii art
    println!(
        "\n\n\n\n\n\n\n\n\n _   _                  ____            _          \n| | | |_   _ _ __  _ __|  _ \\ _ __ ___ | |__   ___ \n| |_| | | | | '_ \\| '__| |_) | '__/ _ \\| '_ \\ / _ \\\n|  _  | |_| | |_) | |  |  __/| | | (_) | |_) |  __/\n|_| |_|\\__, | .__/|_|  |_|   |_|  \\___/|_.__/ \\___|\n       |___/|_|        \n\n"
    );
    {
        // Print git build info early (after UART init).
        let dirty = if build_info::BUILD_GIT_DIRTY {
            "-dirty"
        } else {
            ""
        };
        println!(
            "HyprProbe build: git={}{}",
            build_info::BUILD_GIT_SHA_SHORT,
            dirty
        );
        println!("HyprProbe git full: {}\n\n", build_info::BUILD_GIT_SHA);
    }

    unsafe {
        core::arch::asm!("msr spsr_el2, {}", in(reg) SPSR_EL2_M_EL1H);
        core::arch::asm!("msr elr_el2, {}", in(reg) el1_main);
        core::arch::asm!("msr sp_el1, {}", in(reg) sp_el1);
        core::arch::asm!("msr sctlr_el2, {0:x}", in(reg) 0);
        cpu::isb();
        core::arch::asm!("eret", options(noreturn));
    }
}

extern "C" fn el1_main() -> ! {
    unsafe {
        core::arch::asm!("mov x1, {}", in(reg) *DTB_ADDR.get());
    }
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

fn str_to_usize(s: &str) -> Option<usize> {
    let radix;
    let start;
    match s.get(0..2) {
        Some("0x") => {
            radix = 16;
            start = s.get(2..);
        }
        Some("0o") => {
            radix = 8;
            start = s.get(2..);
        }
        Some("0b") => {
            radix = 2;
            start = s.get(2..);
        }
        _ => {
            radix = 10;
            start = Some(s);
        }
    }
    usize::from_str_radix(start?, radix).ok()
}

fn record_memory_region(base: usize, size: usize) {
    if size == 0 {
        return;
    }
    // SAFETY: early boot records memory regions before secondary cores or interrupts are enabled.
    unsafe {
        let count = &mut *MEM_REGION_COUNT.get();
        if *count >= MAX_MEM_REGIONS {
            return;
        }
        let regions = &mut *MEM_REGIONS.get();
        regions[*count] = MemoryRegion { base, size };
        *count += 1;
    }
}

fn lowest_memory_base() -> Option<usize> {
    let mut min_base: Option<usize> = None;
    // SAFETY: early boot records memory regions before secondary cores or interrupts are enabled.
    unsafe {
        let count = (*MEM_REGION_COUNT.get()).min(MAX_MEM_REGIONS);
        let regions = &*MEM_REGIONS.get();
        for idx in 0..count {
            let region = regions[idx];
            if region.size == 0 {
                continue;
            }
            min_base = Some(match min_base {
                Some(current) => current.min(region.base),
                None => region.base,
            });
        }
    }
    min_base
}

fn guest_mmio_allowlist_slice() -> &'static [GuestMmioRange] {
    // SAFETY: allowlist is populated before the guest is started and then read-only.
    unsafe { (*GUEST_MMIO_ALLOWLIST.get()).as_deref().unwrap_or(&[]) }
}

pub(crate) fn guest_mmio_allowlist_contains(addr: usize) -> bool {
    guest_mmio_allowlist_contains_range(addr, 1)
}

pub(crate) fn guest_mmio_allowlist_contains_range(addr: usize, size: usize) -> bool {
    if size == 0 {
        return false;
    }
    let end = match addr.checked_add(size) {
        Some(end) => end,
        None => return false,
    };
    for range in guest_mmio_allowlist_slice() {
        if range.size == 0 {
            continue;
        }
        if addr >= range.base && end <= range.end() {
            return true;
        }
    }
    false
}

fn normalize_guest_mmio_range(base: usize, size: usize) -> Option<GuestMmioRange> {
    if size == 0 {
        return None;
    }
    let end = base.checked_add(size)?;
    let base_aligned = align_down(base, PAGE_SIZE);
    let end_aligned = align_up(end, PAGE_SIZE);
    if end_aligned <= base_aligned {
        return None;
    }
    Some(GuestMmioRange {
        base: base_aligned,
        size: end_aligned - base_aligned,
    })
}

fn pa_bits_from_parange(p: arch_hal::cpu::registers::PARange) -> u32 {
    match p {
        arch_hal::cpu::registers::PARange::PA32bits4GB => 32,
        arch_hal::cpu::registers::PARange::PA36bits64GB => 36,
        arch_hal::cpu::registers::PARange::PA40bits1TB => 40,
        arch_hal::cpu::registers::PARange::PA42bits4TB => 42,
        arch_hal::cpu::registers::PARange::PA44bits16TB => 44,
        arch_hal::cpu::registers::PARange::PA48bits256TB => 48,
        arch_hal::cpu::registers::PARange::PA52bits4PB => 52,
        arch_hal::cpu::registers::PARange::PA56bits64PB => 56,
    }
}

fn pa_limit_exclusive(pa_bits: u32) -> Option<usize> {
    if pa_bits >= usize::BITS {
        return None;
    }
    Some(1usize << pa_bits)
}

fn range_within_limit(base: usize, size: usize, limit_excl: usize) -> bool {
    if size == 0 {
        return false;
    }
    let Some(end) = base.checked_add(size) else {
        return false;
    };
    base < limit_excl && end <= limit_excl
}

fn normalize_guest_mmio_allowlist(ranges: &mut Vec<GuestMmioRange>) {
    ranges.retain(|range| range.size != 0);
    ranges.sort_by(|a, b| a.base.cmp(&b.base));

    let mut write = 0usize;
    let total = ranges.len();
    for idx in 0..total {
        let range = ranges[idx];
        if range.size == 0 {
            continue;
        }
        if write == 0 {
            ranges[0] = range;
            write = 1;
            continue;
        }
        let last = &mut ranges[write - 1];
        let last_end = last.end();
        if last_end >= range.base {
            let merged_end = last_end.max(range.end());
            last.size = merged_end.saturating_sub(last.base);
        } else {
            ranges[write] = range;
            write += 1;
        }
    }
    ranges.truncate(write);
    debug_assert!(
        ranges
            .iter()
            .all(|range| (range.base | range.size) & (PAGE_SIZE - 1) == 0)
    );
}

fn validate_guest_mmio_allowlist(ranges: &[GuestMmioRange], pa_limit_excl: usize) {
    const LARGE_RANGE_WARN_THRESHOLD: usize = 256 * 1024 * 1024;

    let mut prev: Option<GuestMmioRange> = None;
    for (idx, &range) in ranges.iter().enumerate() {
        if !range_within_limit(range.base, range.size, pa_limit_excl) {
            println!(
                "error: guest MMIO allowlist entry outside PA limit: idx={} base=0x{:X} size=0x{:X} pa_limit_excl=0x{:X}",
                idx, range.base, range.size, pa_limit_excl
            );
            panic!("invalid guest MMIO allowlist: PA limit violation");
        }

        if range.size >= LARGE_RANGE_WARN_THRESHOLD {
            println!(
                "warning: guest MMIO allowlist large range: idx={} base=0x{:X} size=0x{:X}",
                idx, range.base, range.size
            );
        }

        if let Some(prev_range) = prev {
            let Some(prev_end) = prev_range.base.checked_add(prev_range.size) else {
                println!(
                    "error: guest MMIO allowlist previous entry overflowed: prev_base=0x{:X} prev_size=0x{:X}",
                    prev_range.base, prev_range.size
                );
                panic!("invalid guest MMIO allowlist: previous range overflow");
            };
            if prev_range.base > range.base {
                println!(
                    "error: guest MMIO allowlist is not sorted: prev_base=0x{:X} prev_size=0x{:X} next_base=0x{:X} next_size=0x{:X}",
                    prev_range.base, prev_range.size, range.base, range.size
                );
                panic!("invalid guest MMIO allowlist: unsorted ranges");
            }
            if prev_end > range.base {
                println!(
                    "error: guest MMIO allowlist overlap: prev_base=0x{:X} prev_size=0x{:X} next_base=0x{:X} next_size=0x{:X}",
                    prev_range.base, prev_range.size, range.base, range.size
                );
                panic!("invalid guest MMIO allowlist: overlapping ranges");
            }
        }

        prev = Some(range);
    }
}

fn set_guest_mmio_allowlist(ranges: Vec<GuestMmioRange>) {
    // SAFETY: allowlist is populated during single-core boot before interrupts are enabled.
    unsafe {
        let slot = &mut *GUEST_MMIO_ALLOWLIST.get();
        if slot.is_some() {
            println!("warning: guest MMIO allowlist already initialized");
            return;
        }
        *slot = Some(ranges.into_boxed_slice());
    }
}

fn ranges_overlap(a: GuestMmioRange, b: GuestMmioRange) -> bool {
    a.base < b.end() && b.base < a.end()
}

#[derive(Debug)]
struct GuestMmioDenyList {
    gdb_range: Option<GuestMmioRange>,
    deny: Vec<GuestMmioRange>,
}

impl GuestMmioDenyList {
    fn contains_overlap(&self, range: GuestMmioRange) -> bool {
        self.deny
            .iter()
            .copied()
            .any(|deny| ranges_overlap(deny, range))
    }

    fn push_deny(&mut self, base: usize, size: usize) {
        if let Some(range) = normalize_guest_mmio_range(base, size) {
            self.deny.push(range);
        }
    }

    fn finalize(&mut self) {
        normalize_guest_mmio_allowlist(&mut self.deny);
    }
}

#[derive(Default, Debug)]
struct GuestMmioScanStats {
    candidates: usize,
    dropped_reserved: usize,
    dropped_cpu: usize,
    dropped_deny: usize,
    dropped_gdb: usize,
    dropped_out_of_pa: usize,
    dropped_reg_iter_err: usize,
    dropped_reg_entry_err: usize,
    dropped_ranges_iter_err: usize,
    dropped_ranges_entry_err: usize,
}

fn node_property_eq(node: &DtbNodeView<'_, '_>, key: &str, value: &str) -> bool {
    match node.property_bytes(key) {
        Ok(Some(bytes)) => decode_cstr(bytes).is_some_and(|entry| entry == value),
        _ => false,
    }
}

fn node_is_memory(node: &DtbNodeView<'_, '_>) -> bool {
    if node.name().starts_with("memory") {
        return true;
    }
    node_property_eq(node, "device_type", "memory")
}

fn node_is_disabled(node: &DtbNodeView<'_, '_>) -> bool {
    node_property_eq(node, "status", "disabled")
}

fn node_has_no_map(node: &DtbNodeView<'_, '_>) -> bool {
    matches!(node.property_bytes("no-map"), Ok(Some(_)))
}

fn node_is_cpu(node: &DtbNodeView<'_, '_>) -> bool {
    if node_property_eq(node, "device_type", "cpu") {
        return true;
    }
    node.name().starts_with("cpu@")
}

fn push_guest_mmio_range(
    ranges: &mut Vec<GuestMmioRange>,
    denylist: &GuestMmioDenyList,
    base: usize,
    size: usize,
    node_name: &str,
    source: &str,
    pa_bits: u32,
    pa_limit_excl: usize,
    stats: &mut GuestMmioScanStats,
) {
    let Some(range) = normalize_guest_mmio_range(base, size) else {
        return;
    };
    if !range_within_limit(range.base, range.size, pa_limit_excl) {
        stats.dropped_out_of_pa += 1;
        println!(
            "warning: dropped MMIO candidate outside PA range from node '{}' source={} base=0x{:X} size=0x{:X} pa_bits={}",
            node_name, source, range.base, range.size, pa_bits
        );
        return;
    }
    if let Some(gdb_range) = denylist.gdb_range {
        if ranges_overlap(range, gdb_range) {
            stats.dropped_gdb += 1;
            return;
        }
    }
    if denylist.contains_overlap(range) {
        stats.dropped_deny += 1;
        return;
    }
    ranges.push(range);
}

fn record_guest_mmio_allowlist_from_dtb(
    dtb: &DtbParser,
    guest_uart: &UartNode,
    gdb_uart: Option<&UartNode>,
    gic_info: &Gicv2Info,
) {
    let pa_bits = cpu::get_parange().map(pa_bits_from_parange).unwrap_or(48);
    let pa_limit_excl = pa_limit_exclusive(pa_bits).unwrap_or(usize::MAX);

    let guest_range = normalize_guest_mmio_range(guest_uart.base, guest_uart.size);
    let mut gdb_range = gdb_uart.and_then(|node| normalize_guest_mmio_range(node.base, node.size));
    if let (Some(guest_range), Some(current_gdb_range)) = (guest_range, gdb_range) {
        if ranges_overlap(guest_range, current_gdb_range) {
            println!(
                "warning: guest/gdb UART ranges overlap within normalized MMIO pages; not denying GDB range"
            );
            gdb_range = None;
        }
    }
    let mut denylist = GuestMmioDenyList {
        gdb_range,
        deny: Vec::new(),
    };
    denylist.push_deny(gic_info.dist.base, gic_info.dist.size);
    denylist.push_deny(gic_info.cpu.base, gic_info.cpu.size);
    if let Some(gich) = gic_info.gich {
        denylist.push_deny(gich.base, gich.size);
    }
    // SAFETY: memory regions are recorded during single-core boot before interrupts are enabled.
    unsafe {
        let count = (*MEM_REGION_COUNT.get()).min(MAX_MEM_REGIONS);
        let regions = &*MEM_REGIONS.get();
        for idx in 0..count {
            let region = regions[idx];
            if region.size == 0 {
                continue;
            }
            denylist.push_deny(region.base, region.size);
        }
    }
    let _ = GLOBAL_ALLOCATOR.for_each_reserved_region(|base, size| {
        if size == 0 {
            return;
        }
        denylist.push_deny(base, size);
    });
    denylist.finalize();

    let mut ranges: Vec<GuestMmioRange> = Vec::new();
    let mut stats = GuestMmioScanStats::default();

    fn walk<'dtb, 's>(
        node: DtbNodeView<'dtb, 's>,
        in_reserved: bool,
        in_cpus: bool,
        denylist: &GuestMmioDenyList,
        ranges: &mut Vec<GuestMmioRange>,
        pa_bits: u32,
        pa_limit_excl: usize,
        stats: &mut GuestMmioScanStats,
    ) -> Result<ControlFlow<()>, WalkError<&'static str>> {
        let name = node.name();
        let in_reserved = in_reserved || name == "reserved-memory";
        if in_reserved {
            stats.dropped_reserved += 1;
            return Ok(ControlFlow::Continue(()));
        }
        let in_cpus = in_cpus || name == "cpus";
        if in_cpus {
            stats.dropped_cpu += 1;
            return Ok(ControlFlow::Continue(()));
        }
        if node_is_cpu(&node) {
            stats.dropped_cpu += 1;
            return Ok(ControlFlow::Continue(()));
        }

        let skip_collect =
            node_is_memory(&node) || node_is_disabled(&node) || node_has_no_map(&node);
        if !skip_collect {
            match node.reg_iter() {
                Ok(mut regs) => {
                    while let Some(entry) = regs.next() {
                        let (base, size) = match entry {
                            Ok(entry) => entry,
                            Err(err) => {
                                stats.dropped_reg_entry_err += 1;
                                println!(
                                    "warning: reg entry parse failed at node '{}': {}",
                                    name, err
                                );
                                break;
                            }
                        };
                        if size == 0 {
                            continue;
                        }
                        stats.candidates += 1;
                        push_guest_mmio_range(
                            ranges,
                            denylist,
                            base,
                            size,
                            name,
                            "reg",
                            pa_bits,
                            pa_limit_excl,
                            stats,
                        );
                    }
                }
                Err(err) => {
                    stats.dropped_reg_iter_err += 1;
                    println!("warning: reg_iter failed at node '{}': {}", name, err);
                }
            }

            match node.ranges_iter() {
                Ok(Some(mut iter)) => {
                    while let Some(entry) = iter.next() {
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(err) => {
                                stats.dropped_ranges_entry_err += 1;
                                println!(
                                    "warning: ranges entry parse failed at node '{}': {}",
                                    name, err
                                );
                                break;
                            }
                        };
                        if entry.len == 0 {
                            continue;
                        }
                        stats.candidates += 1;
                        push_guest_mmio_range(
                            ranges,
                            denylist,
                            entry.parent_base,
                            entry.len,
                            name,
                            "ranges",
                            pa_bits,
                            pa_limit_excl,
                            stats,
                        );
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    stats.dropped_ranges_iter_err += 1;
                    println!("warning: ranges_iter failed at node '{}': {}", name, err);
                }
            }
        }

        node.for_each_child_view(&mut |child| {
            walk(
                child,
                in_reserved,
                in_cpus,
                denylist,
                ranges,
                pa_bits,
                pa_limit_excl,
                stats,
            )
        })
    }

    let result = (|| -> Result<(), &'static str> {
        let root = dtb.root_node_view()?;
        let result = root.for_each_child_view(&mut |child| {
            walk(
                child,
                false,
                false,
                &denylist,
                &mut ranges,
                pa_bits,
                pa_limit_excl,
                &mut stats,
            )
        });
        match result {
            Ok(ControlFlow::Continue(())) | Ok(ControlFlow::Break(())) => Ok(()),
            Err(WalkError::Dtb(err)) => Err(err),
            Err(WalkError::User(err)) => Err(err),
        }
    })();

    if let Err(err) = result {
        println!("warning: guest MMIO allowlist scan failed: {}", err);
    }
    println!(
        "mmio scan: candidates={} dropped reserved-subtree={} dropped cpu-subtree={} dropped deny-overlap={} dropped gdb-overlap={} dropped out-of-pa={} dropped reg-iter-err={} dropped reg-entry-err={} dropped ranges-iter-err={} dropped ranges-entry-err={}",
        stats.candidates,
        stats.dropped_reserved,
        stats.dropped_cpu,
        stats.dropped_deny,
        stats.dropped_gdb,
        stats.dropped_out_of_pa,
        stats.dropped_reg_iter_err,
        stats.dropped_reg_entry_err,
        stats.dropped_ranges_iter_err,
        stats.dropped_ranges_entry_err
    );

    normalize_guest_mmio_allowlist(&mut ranges);
    validate_guest_mmio_allowlist(&ranges, pa_limit_excl);
    set_guest_mmio_allowlist(ranges);
}

fn dump_guest_mmio_allowlist() {
    for (idx, range) in guest_mmio_allowlist_slice().iter().enumerate() {
        if range.size == 0 {
            continue;
        }
        println!(
            "guest mmio allowlist[{}]: base=0x{:X} size=0x{:X}",
            idx, range.base, range.size
        );
    }
}

fn push_debug_io_range(io_ranges: &mut Vec<(u64, u64)>, base: usize, size: usize) {
    if size == 0 {
        return;
    }
    let entry = (base as u64, size as u64);
    if io_ranges.iter().any(|&existing| existing == entry) {
        return;
    }
    io_ranges.push(entry);
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
                .map(|(base, _size)| gic::MmioRegion { base, size: 0x1000 });
            let gicv = regs
                .next()
                .and_then(|r| r.ok())
                .map(|(base, _size)| gic::MmioRegion { base, size: 0x2000 });
            let mut maintenance_intid = None;
            let _ = view.for_each_interrupt_specifier(&mut |cells| -> Result<
                ControlFlow<()>,
                WalkError<()>,
            > {
                if maintenance_intid.is_some() {
                    return Ok(ControlFlow::Break(()));
                }
                if let Ok(intid) = irq_decode::dt_irq_to_pintid(cells) {
                    maintenance_intid = Some(intid);
                }
                Ok(ControlFlow::Break(()))
            })?;

            found = Some(Gicv2Info {
                dist: gic::MmioRegion {
                    base: dist_base,
                    size: 0x1000,
                },
                cpu: gic::MmioRegion {
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

fn init_gicv2(
    info: &Gicv2Info,
    gdb_uart: Option<UartNode>,
) -> Result<(gic::gicv2::Gicv2, Option<u32>), &'static str> {
    let virt = match (info.gich, info.gicv, info.maintenance_intid) {
        (Some(gich), Some(gicv), Some(maint)) => Some(gic::gicv2::Gicv2VirtualizationRegion {
            gich,
            gicv,
            maintenance_interrupt_id: maint,
        }),
        _ => None,
    };
    println!("gic v2: {:?}", info);
    let gic =
        gic::gicv2::Gicv2::new(info.dist, info.cpu, virt, None).map_err(|_| "gic: init failed")?;
    gic.init_distributor().map_err(|_| "gic: init dist")?;
    let caps = gic.init_cpu_interface().map_err(|_| "gic: init cpu")?;
    let cfg = GicCpuConfig {
        priority_mask: 0xff,
        enable_group0: caps.supports_group0,
        enable_group1: true,
        binary_point: BinaryPoint::Common(caps.binary_points_min),
        eoi_mode: EoiMode::DropAndDeactivate,
    };
    gic.configure(&cfg).map_err(|_| "gic: configure")?;

    let gdb_intid = if let Some(gdb_uart) = gdb_uart {
        let gdb_intid = gdb_uart.irq.ok_or("gic: gdb uart missing IRQ")?;
        gic.configure_spi(
            gdb_intid,
            IrqGroup::Group1,
            0x80,
            TriggerMode::Level,
            SpiRoute::Specific(cpu::get_current_core_id()),
            EnableOp::Enable,
        )
        .map_err(|_| "gic: configure spi")?;
        Some(gdb_intid)
    } else {
        None
    };

    Ok((gic, gdb_intid))
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Range {
    start: usize,
    end: usize,
}

fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

fn align_up(value: usize, align: usize) -> usize {
    value.checked_add(align - 1).unwrap_or(usize::MAX) & !(align - 1)
}

/// Sorts nonempty ranges and merges overlapping or adjacent spans in place.
/// Returns the length of the normalized prefix.
fn normalize_ranges(ranges: &mut [Range]) -> usize {
    ranges.sort_unstable_by_key(|range| range.start);
    let mut count = 0;
    for index in 0..ranges.len() {
        let range = ranges[index];
        if range.end <= range.start {
            continue;
        }
        if count != 0 && range.start <= ranges[count - 1].end {
            ranges[count - 1].end = ranges[count - 1].end.max(range.end);
        } else {
            ranges[count] = range;
            count += 1;
        }
    }
    count
}

fn sort_stage2_settings(settings: &mut [Stage2PagingSetting], count: usize) {
    for i in 0..count {
        for j in i + 1..count {
            if settings[i].ipa > settings[j].ipa {
                settings.swap(i, j);
            }
        }
    }
}

fn normalize_stage2_settings(settings: &mut [Stage2PagingSetting], count: usize) -> usize {
    let mut write = 0usize;
    for idx in 0..count {
        let cur = settings[idx];
        if cur.size == 0 {
            continue;
        }
        if write == 0 {
            settings[0] = cur;
            write = 1;
            continue;
        }
        let prev = settings[write - 1];
        let prev_end = prev.ipa.saturating_add(prev.size);
        let cur_end = cur.ipa.saturating_add(cur.size);

        if cur.ipa < prev_end {
            if prev.types == cur.types && prev.pa == prev.ipa && cur.pa == cur.ipa {
                let merged_end = prev_end.max(cur_end);
                settings[write - 1].size = merged_end.saturating_sub(prev.ipa);
            } else if cur.types == Stage2PageTypes::Device {
                println!(
                    "warning: stage2 overlap 0x{:X}..0x{:X}, dropping MMIO entry",
                    cur.ipa, cur_end
                );
            } else if prev.types == Stage2PageTypes::Device {
                println!(
                    "warning: stage2 overlap 0x{:X}..0x{:X}, keeping RAM entry",
                    prev.ipa, prev_end
                );
                settings[write - 1] = cur;
            } else {
                println!(
                    "warning: stage2 overlap 0x{:X}..0x{:X}, dropping later entry",
                    cur.ipa, cur_end
                );
            }
            continue;
        }

        if cur.ipa == prev_end
            && prev.types == cur.types
            && prev.pa == prev.ipa
            && cur.pa == cur.ipa
        {
            settings[write - 1].size = cur_end.saturating_sub(prev.ipa);
            continue;
        }

        settings[write] = cur;
        write += 1;
    }
    write
}

fn push_normal_excluding_guard(
    settings: &mut Vec<EL2Stage1PagingSetting>,
    start: usize,
    end: usize,
    guard_page_start: usize,
    guard_page_end: usize,
) {
    if end <= start {
        return;
    }

    // Exclude the guard page (unmapped) region.
    if guard_page_end <= start || guard_page_start >= end {
        settings.push(EL2Stage1PagingSetting {
            va: start,
            pa: start,
            size: end - start,
            types: EL2Stage1PageTypes::Normal,
        });
        return;
    }

    if start < guard_page_start {
        settings.push(EL2Stage1PagingSetting {
            va: start,
            pa: start,
            size: guard_page_start - start,
            types: EL2Stage1PageTypes::Normal,
        });
    }
    if guard_page_end < end {
        settings.push(EL2Stage1PagingSetting {
            va: guard_page_end,
            pa: guard_page_end,
            size: end - guard_page_end,
            types: EL2Stage1PageTypes::Normal,
        });
    }
}

fn build_stage1_el2_map() -> Vec<EL2Stage1PagingSetting> {
    unsafe extern "C" {
        static _STACK_BOTTOM: usize;
    }

    let stack_bottom = &raw const _STACK_BOTTOM as usize;
    let guard_page_start = stack_bottom;
    let guard_page_end = stack_bottom + PAGE_SIZE;

    let mut settings = Vec::new();

    // SAFETY: memory regions are recorded during early boot before secondary cores start
    let mem_count = unsafe { *MEM_REGION_COUNT.get() };
    let mem_regions = unsafe { &*MEM_REGIONS.get() };

    for i in 0..mem_count.min(MAX_MEM_REGIONS) {
        let region = mem_regions[i];
        if region.size == 0 {
            continue;
        }

        let start = align_down(region.base, PAGE_SIZE);
        let end = align_up(region.base.saturating_add(region.size), PAGE_SIZE);
        if end <= start {
            continue;
        }

        push_normal_excluding_guard(&mut settings, start, end, guard_page_start, guard_page_end);
    }

    // map other memory regions as mmio
    settings.sort_by(|a, b| a.va.cmp(&b.va));

    if settings.is_empty() {
        // If nothing is mapped as Normal, map everything as Device (fallback).
        // This should not happen in normal boot, but avoids indexing panic.
        let parange = pa_bits_from_parange(cpu::get_parange().unwrap()).min(48);
        let ipa_space = 1usize << parange;
        settings.push(EL2Stage1PagingSetting {
            va: 0,
            pa: 0,
            size: ipa_space,
            types: EL2Stage1PageTypes::Device,
        });
        return settings;
    }

    if settings[0].va != 0 {
        settings.insert(
            0,
            EL2Stage1PagingSetting {
                va: 0,
                pa: 0,
                size: settings[0].va,
                types: EL2Stage1PageTypes::Device,
            },
        );
    }

    let last = settings.last().unwrap();
    let parange = pa_bits_from_parange(cpu::get_parange().unwrap()).min(48);
    let ipa_space = 1usize << parange;
    settings.push(EL2Stage1PagingSetting {
        va: last.va + last.size,
        pa: last.pa + last.size,
        size: ipa_space - (last.va + last.size),
        types: EL2Stage1PageTypes::Device,
    });

    settings
}

fn build_stage2_guest_map() -> (Vec<Stage2PagingSetting>, Option<(usize, usize)>) {
    const MAX_RESERVED_REGIONS: usize = 32;

    let mut mem_ranges = [Range { start: 0, end: 0 }; MAX_MEM_REGIONS];
    let mut mem_count = 0usize;
    // SAFETY: early boot records memory regions before secondary cores or interrupts are enabled.
    unsafe {
        let total = (*MEM_REGION_COUNT.get()).min(MAX_MEM_REGIONS);
        let regions = &*MEM_REGIONS.get();
        for idx in 0..total {
            let region = regions[idx];
            if region.size == 0 {
                continue;
            }
            let start = align_down(region.base, PAGE_SIZE);
            let end = align_up(region.base.saturating_add(region.size), PAGE_SIZE);
            if end <= start {
                continue;
            }
            mem_ranges[mem_count] = Range { start, end };
            mem_count += 1;
        }
    }
    let mem_ranges_count = normalize_ranges(&mut mem_ranges[..mem_count]);

    let mut reserved = [Range { start: 0, end: 0 }; MAX_RESERVED_REGIONS];
    let mut reserved_count = 0usize;
    GLOBAL_ALLOCATOR
        .for_each_reserved_region(|base, size| {
            if size == 0 || reserved_count >= reserved.len() {
                return;
            }
            let start = align_down(base, PAGE_SIZE);
            let end = align_up(base.saturating_add(size), PAGE_SIZE);
            if end <= start {
                return;
            }
            reserved[reserved_count] = Range { start, end };
            reserved_count += 1;
        })
        .unwrap();
    let reserved_merged_count = normalize_ranges(&mut reserved[..reserved_count]);

    let mut best: Option<Range> = None;
    for idx in 0..mem_ranges_count {
        let mem = mem_ranges[idx];
        let mut cursor = mem.start;
        for ridx in 0..reserved_merged_count {
            let r = reserved[ridx];
            if r.end <= cursor {
                continue;
            }
            if r.start >= mem.end {
                break;
            }
            let res_start = r.start.max(mem.start);
            if res_start > cursor {
                let seg = Range {
                    start: cursor,
                    end: res_start.min(mem.end),
                };
                best = match best {
                    Some(current) => {
                        if seg.end - seg.start > current.end - current.start {
                            Some(seg)
                        } else {
                            Some(current)
                        }
                    }
                    None => Some(seg),
                };
            }
            cursor = r.end.max(cursor);
            if cursor >= mem.end {
                break;
            }
        }
        if cursor < mem.end {
            let seg = Range {
                start: cursor,
                end: mem.end,
            };
            best = match best {
                Some(current) => {
                    if seg.end - seg.start > current.end - current.start {
                        Some(seg)
                    } else {
                        Some(current)
                    }
                }
                None => Some(seg),
            };
        }
    }

    unsafe extern "C" {
        static __el1_region_start: u8;
        static __el1_region_end: u8;
    }
    let el1_start = unsafe { &__el1_region_start as *const u8 as usize };
    let el1_end = unsafe { &__el1_region_end as *const u8 as usize };
    let el1_start_aligned = align_down(el1_start, PAGE_SIZE);
    let el1_end_aligned = align_up(el1_end, PAGE_SIZE);

    let mut settings: Vec<Stage2PagingSetting> = Vec::new();

    if let Some(seg) = best {
        let size = seg.end.saturating_sub(seg.start);
        let guest_window = Range {
            start: seg.start,
            end: seg.start.saturating_add(size),
        };
        let mut warned_overlap = false;
        for range in guest_mmio_allowlist_slice() {
            if range.size == 0 {
                continue;
            }
            if (range.base | range.size) & (PAGE_SIZE - 1) != 0 {
                println!(
                    "warning: stage2 MMIO range unaligned 0x{:X} size 0x{:X}",
                    range.base, range.size
                );
                continue;
            }
            if range.base < guest_window.end && range.end() > guest_window.start {
                if !warned_overlap {
                    println!(
                        "warning: stage2 MMIO overlaps guest RAM, skipping overlapping ranges"
                    );
                    warned_overlap = true;
                }
                continue;
            }
            settings.push(Stage2PagingSetting {
                ipa: range.base,
                pa: range.base,
                size: range.size,
                types: Stage2PageTypes::Device,
                perm: Stage2AccessPermission::ReadWrite,
            });
        }

        settings.push(Stage2PagingSetting {
            ipa: seg.start,
            pa: seg.start,
            size,
            types: Stage2PageTypes::Normal,
            perm: Stage2AccessPermission::ReadWrite,
        });

        if el1_end_aligned > el1_start_aligned {
            settings.push(Stage2PagingSetting {
                ipa: el1_start_aligned,
                pa: el1_start_aligned,
                size: el1_end_aligned - el1_start_aligned,
                types: Stage2PageTypes::Normal,
                perm: Stage2AccessPermission::ReadOnly,
            });
        }

        let mut count = settings.len();
        sort_stage2_settings(settings.as_mut_slice(), count);
        count = normalize_stage2_settings(settings.as_mut_slice(), count);
        settings.truncate(count);

        let mut write = 0usize;
        let total = settings.len();
        for idx in 0..total {
            let setting = settings[idx];
            if (setting.ipa | setting.pa | setting.size) & (PAGE_SIZE - 1) != 0 {
                println!(
                    "warning: stage2 setting unaligned 0x{:X} size 0x{:X}",
                    setting.ipa, setting.size
                );
                continue;
            }
            settings[write] = setting;
            write += 1;
        }
        settings.truncate(write);

        return (settings, Some((seg.start, size)));
    }

    (settings, None)
}

fn apply_guest_uart_dt_edit(
    tree: &mut DeviceTree<'static>,
    guest_uart_base: usize,
) -> Result<(), &'static str> {
    let mut guest_node = None;
    let mut disable_nodes = Vec::new();

    for id in 0..tree.nodes.len() {
        if !node_compatible_contains(tree, id, "arm,pl011")? {
            continue;
        }
        let raw_base = node_reg_base(tree, id)?;
        let translated_base = node_reg_base_translated(tree, id)?;
        if raw_base == Some(guest_uart_base) || translated_base == Some(guest_uart_base) {
            guest_node = Some(id);
        } else {
            disable_nodes.push(id);
        }
    }

    let guest_node = guest_node.ok_or("guest UART node not found in DT")?;
    for id in disable_nodes.iter().copied() {
        if let Some(node) = tree.node_mut(id) {
            node.set_property(
                NameRef::Borrowed("status"),
                ValueRef::Owned(b"disabled\0".to_vec()),
            );
        }
    }

    if let Some(chosen) = tree.find_node_by_path("/chosen") {
        if let Some(path) = node_path(tree, guest_node) {
            let mut value = Vec::with_capacity(path.len() + 1);
            value.extend_from_slice(path.as_bytes());
            value.push(0);
            if let Some(node) = tree.node_mut(chosen) {
                node.set_property(NameRef::Borrowed("stdout-path"), ValueRef::Owned(value));
            }
        }
    }

    if let Some(aliases) = tree.find_node_by_path("/aliases") {
        let mut remove: Vec<String> = Vec::new();
        for id in disable_nodes.iter().copied() {
            if let Some(path) = node_path(tree, id) {
                if let Some(node) = tree.node(aliases) {
                    for prop in &node.properties {
                        if let Some(value) = decode_cstr(prop.value.as_slice()) {
                            if value == path {
                                remove.push(String::from(prop.name.as_str()));
                            }
                        }
                    }
                }
            }
        }
        if let Some(node) = tree.node_mut(aliases) {
            for name in remove {
                node.remove_property(&name);
            }
        }
    }

    Ok(())
}

fn apply_guest_dt_edits(
    tree: &mut DeviceTree<'static>,
    guest_uart_base: usize,
    gic_info: &Gicv2Info,
) -> Result<(), &'static str> {
    apply_guest_bootargs(tree, guest_uart_base)?;
    apply_guest_uart_dt_edit(tree, guest_uart_base)?;
    if let Some(gicv) = gic_info.gicv {
        update_gicv2_cpu_interface_reg(tree, gicv)?;
    }
    Ok(())
}

fn apply_guest_bootargs(
    tree: &mut DeviceTree<'static>,
    guest_uart_base: usize,
) -> Result<(), &'static str> {
    let chosen = tree.get_or_create_node_by_path("/chosen")?;
    let bootargs = alloc::format!(
        "root=/dev/vda2 rw rootwait earlycon=pl011,0x{:08x}",
        guest_uart_base
    );
    let mut value = bootargs.into_bytes();
    value.push(0);
    if let Some(node) = tree.node_mut(chosen) {
        node.set_property(NameRef::Borrowed("bootargs"), ValueRef::Owned(value));
    }
    Ok(())
}

fn update_gicv2_cpu_interface_reg(
    tree: &mut DeviceTree<'static>,
    gicv: gic::MmioRegion,
) -> Result<(), &'static str> {
    const COMPATS: [&str; 2] = ["arm,gic-400", "arm,cortex-a15-gic"];
    let mut gic_node = None;
    for id in 0..tree.nodes.len() {
        for compat in COMPATS {
            if node_compatible_contains(tree, id, compat)? {
                gic_node = Some(id);
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
        .and_then(|n| n.parent)
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
    let base_off = stride;
    write_be_u32s(&mut bytes, base_off, addr_cells, gicv.base as u64)?;
    write_be_u32s(
        &mut bytes,
        base_off + addr_cells * 4,
        size_cells,
        gicv.size as u64,
    )?;

    if let Some(node) = tree.node_mut(node_id) {
        node.set_property(NameRef::Borrowed("reg"), ValueRef::Owned(bytes));
    }
    Ok(())
}

fn node_compatible_contains(
    tree: &DeviceTree<'static>,
    node_id: usize,
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
            .position(|&b| b == 0)
            .map(|p| start + p)
            .unwrap_or(bytes.len());
        if let Ok(entry) = core::str::from_utf8(&bytes[start..end]) {
            if entry == needle {
                return Ok(true);
            }
        }
        start = end + 1;
    }
    Ok(false)
}

fn node_reg_base(
    tree: &DeviceTree<'static>,
    node_id: usize,
) -> Result<Option<usize>, &'static str> {
    let Some((addr, _)) = node_reg_first(tree, node_id)? else {
        return Ok(None);
    };
    let base = usize::try_from(addr).map_err(|_| "reg: address overflow usize")?;
    Ok(Some(base))
}

fn node_reg_base_translated(
    tree: &DeviceTree<'static>,
    node_id: usize,
) -> Result<Option<usize>, &'static str> {
    let Some(addr_len) = node_reg_first(tree, node_id)? else {
        return Ok(None);
    };
    let mapped = match translate_address_via_ancestors(tree, node_id, addr_len) {
        Ok(mapped) => mapped,
        Err("ranges: address not covered") => return Ok(None),
        Err(e) => return Err(e),
    };
    let addr = usize::try_from(mapped.0).map_err(|_| "ranges: mapped address overflow usize")?;
    Ok(Some(addr))
}

fn node_reg_first(
    tree: &DeviceTree<'static>,
    node_id: usize,
) -> Result<Option<(u128, u128)>, &'static str> {
    let Some(node) = tree.node(node_id) else {
        return Ok(None);
    };
    let Some(prop) = node.property("reg") else {
        return Ok(None);
    };
    let parent = node.parent.unwrap_or(tree.root);
    let addr_cells = inherited_u32(tree, parent, "#address-cells")?.unwrap_or(2);
    let size_cells = inherited_u32(tree, parent, "#size-cells")?.unwrap_or(1);

    let addr_cells_usize =
        usize::try_from(addr_cells).map_err(|_| "reg: address/size cells overflow usize")?;
    let size_cells_usize =
        usize::try_from(size_cells).map_err(|_| "reg: address/size cells overflow usize")?;
    if addr_cells_usize > 4 || size_cells_usize > 4 {
        return Err("reg: address/size cells overflow u128");
    }

    let entry_cells = addr_cells_usize
        .checked_add(size_cells_usize)
        .ok_or("reg: cell count overflow")?;
    let entry_bytes = entry_cells
        .checked_mul(4)
        .ok_or("reg: byte count overflow")?;
    let bytes = prop.value.as_slice();
    if bytes.len() < entry_bytes {
        return Ok(None);
    }

    let (addr, consumed) = read_be_cells_u128(bytes, 0, addr_cells)?;
    let (len, _) = read_be_cells_u128(bytes, consumed, size_cells)?;
    Ok(Some((addr, len)))
}

fn translate_address_via_ancestors(
    tree: &DeviceTree<'static>,
    node_id: usize,
    mut addr_len: (u128, u128),
) -> Result<(u128, u128), &'static str> {
    let mut current = node_id;
    loop {
        let Some(node) = tree.node(current) else {
            return Err("ranges: invalid node");
        };
        let Some(parent) = node.parent else {
            break;
        };
        addr_len = translate_one_level_ranges(tree, parent, addr_len)?;
        current = parent;
    }
    Ok(addr_len)
}

fn translate_one_level_ranges(
    tree: &DeviceTree<'static>,
    bus_node_id: usize,
    child: (u128, u128),
) -> Result<(u128, u128), &'static str> {
    let Some(bus) = tree.node(bus_node_id) else {
        return Err("ranges: invalid bus node");
    };
    let Some(prop) = bus.property("ranges") else {
        return Ok(child);
    };
    let ranges = prop.value.as_slice();
    if ranges.is_empty() {
        return Ok(child);
    }

    let child_address_cells = inherited_u32(tree, bus_node_id, "#address-cells")?.unwrap_or(2);
    let child_size_cells = inherited_u32(tree, bus_node_id, "#size-cells")?.unwrap_or(1);
    let parent = bus.parent.ok_or("ranges: missing parent")?;
    let parent_address_cells = inherited_u32(tree, parent, "#address-cells")?.unwrap_or(2);

    let child_address_cells_usize = usize::try_from(child_address_cells)
        .map_err(|_| "ranges: address/size cells overflow usize")?;
    let child_size_cells_usize = usize::try_from(child_size_cells)
        .map_err(|_| "ranges: address/size cells overflow usize")?;
    let parent_address_cells_usize = usize::try_from(parent_address_cells)
        .map_err(|_| "ranges: address/size cells overflow usize")?;
    if child_address_cells_usize > 4 || child_size_cells_usize > 4 || parent_address_cells_usize > 4
    {
        return Err("ranges: address/size cells overflow u128");
    }

    let cell_count = child_address_cells
        .checked_add(parent_address_cells)
        .and_then(|v| v.checked_add(child_size_cells))
        .ok_or("ranges: stride overflow")?;
    let entry_stride = cell_count.checked_mul(4).ok_or("ranges: stride overflow")?;
    let entry_stride = usize::try_from(entry_stride).map_err(|_| "ranges: stride overflow")?;
    if entry_stride == 0 {
        return Err("ranges: zero stride");
    }
    if ranges.len() % entry_stride != 0 {
        return Err("ranges: length not multiple of stride");
    }

    let child_end = child
        .0
        .checked_add(child.1)
        .ok_or("ranges: child overflow")?;
    let mut consumed = 0usize;
    while consumed < ranges.len() {
        let base = consumed;
        let (child_base, c0) = read_be_cells_u128(ranges, base, child_address_cells)?;
        let off1 = base.checked_add(c0).ok_or("ranges: overrun")?;
        let (parent_base, c1) = read_be_cells_u128(ranges, off1, parent_address_cells)?;
        let off2 = off1.checked_add(c1).ok_or("ranges: overrun")?;
        let (len, c2) = read_be_cells_u128(ranges, off2, child_size_cells)?;
        let entry_consumed = c0
            .checked_add(c1)
            .and_then(|v| v.checked_add(c2))
            .ok_or("ranges: overrun")?;
        if entry_consumed != entry_stride {
            return Err("ranges: unexpected entry size");
        }
        consumed = consumed
            .checked_add(entry_consumed)
            .ok_or("ranges: overrun")?;

        let entry_end = child_base
            .checked_add(len)
            .ok_or("ranges: entry overflow")?;
        if child.0 >= child_base && child_end <= entry_end {
            let off = child.0.checked_sub(child_base).ok_or("ranges: underflow")?;
            let parent_mapped = parent_base.checked_add(off).ok_or("ranges: overflow")?;
            return Ok((parent_mapped, child.1));
        }
    }

    Err("ranges: address not covered")
}

fn inherited_u32(
    tree: &DeviceTree<'static>,
    node_id: usize,
    key: &str,
) -> Result<Option<u32>, &'static str> {
    let mut current = Some(node_id);
    while let Some(id) = current {
        if let Some(value) = property_u32(tree, id, key)? {
            return Ok(Some(value));
        }
        current = tree.node(id).and_then(|node| node.parent);
    }
    Ok(None)
}

fn read_be_cells_u128(
    bytes: &[u8],
    offset: usize,
    cells: u32,
) -> Result<(u128, usize), &'static str> {
    let cells = usize::try_from(cells).map_err(|_| "dtb: read_be_cells_u128 cell overflow")?;
    if cells > 4 {
        return Err("dtb: read_be_cells_u128 overflow u128");
    }
    let consumed = cells
        .checked_mul(4)
        .ok_or("dtb: read_be_cells_u128 byte overflow")?;
    let end = offset
        .checked_add(consumed)
        .ok_or("dtb: read_be_cells_u128 overflow")?;
    if bytes.get(offset..end).is_none() {
        return Err("dtb: read_be_cells_u128 oob");
    }

    let mut value = 0u128;
    for i in 0..cells {
        let cell_offset = offset
            .checked_add(i.checked_mul(4).ok_or("dtb: read_be_cells_u128 overflow")?)
            .ok_or("dtb: read_be_cells_u128 overflow")?;
        let cell = read_be_u32(bytes, cell_offset)?;
        value = (value << 32) | cell as u128;
    }
    Ok((value, consumed))
}

fn property_u32(
    tree: &DeviceTree<'static>,
    node_id: usize,
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
    for i in 0..cells {
        let shift = 32 * (cells - 1 - i);
        let cell = ((value >> shift) & 0xffff_ffff) as u32;
        write_be_u32(bytes, offset + i * 4, cell)?;
    }
    Ok(())
}

fn node_path(tree: &DeviceTree<'static>, node_id: usize) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut current = Some(node_id);
    while let Some(id) = current {
        let node = tree.node(id)?;
        let name = node.name.as_str();
        if !name.is_empty() && name != "/" {
            parts.push(name);
        }
        current = node.parent;
    }
    parts.reverse();
    let mut path = String::from("/");
    for (idx, part) in parts.iter().enumerate() {
        if idx > 0 {
            path.push('/');
        }
        path.push_str(part);
    }
    Some(path)
}

fn decode_cstr(bytes: &[u8]) -> Option<&str> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).ok()
}

#[cfg(all(
    feature = "rpi4",
    not(any(feature = "rpi4_net", feature = "virtio_net"))
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnableUart2GpioError {
    ResolveMmio(Bcm2711GpioError),
    ConfigurePins(Bcm2711GpioError),
}

#[cfg(all(
    feature = "rpi4",
    not(any(feature = "rpi4_net", feature = "virtio_net"))
))]
fn enable_uart2_gpio(dtb: &DtbParser) {
    match try_enable_uart2_gpio(dtb) {
        Ok(()) => {}
        Err(err) => {
            println!("uart2 gpio setup failed: {:?}", err);
            panic!("uart2 gpio setup failed");
        }
    }
}

#[cfg(all(
    feature = "rpi4",
    not(any(feature = "rpi4_net", feature = "virtio_net"))
))]
fn try_enable_uart2_gpio(dtb: &DtbParser) -> Result<(), EnableUart2GpioError> {
    let mmio = gpio_mmio_from_dtb(dtb).map_err(EnableUart2GpioError::ResolveMmio)?;
    // SAFETY: `mmio.base` comes from the DTB GPIO `reg` property for this board,
    // this code runs during single-core early init, and the mapped region is used
    // as the sole GPIO controller view in this initialization path.
    let gpio = unsafe { Bcm2711Gpio::new(mmio.base) };
    gpio.configure_uart2_pins(false, Pull::None)
        .map_err(EnableUart2GpioError::ConfigurePins)?;
    Ok(())
}

#[cfg(not(all(test, target_arch = "aarch64")))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // SAFETY: panic path uses a best-effort UART address chosen during early boot.
    let uart_addr = unsafe { *DEBUG_UART_ADDR.get() }.unwrap_or(PL011_UART_ADDR);
    let mut debug_uart = Pl011Uart::new(uart_addr, UART_CLOCK_HZ);
    debug_uart.init(UART_BAUD);
    debug_uart.write("core 0 panicked!!!\r\n");
    let _ = debug_uart.write_fmt(format_args!("PANIC: {}", info));
    loop {}
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::str;
    use core::sync::atomic::AtomicBool;
    use core::sync::atomic::Ordering;
    use gdb_remote::WatchpointKind;

    static INIT: AtomicBool = AtomicBool::new(false);

    fn init_test_env() {
        if INIT.swap(true, Ordering::AcqRel) {
            return;
        }

        let mut ranges = Vec::new();
        if let Some(range) = normalize_guest_mmio_range(0x1900_0000, 0x1000) {
            ranges.push(range);
        }
        normalize_guest_mmio_allowlist(&mut ranges);
        set_guest_mmio_allowlist(ranges);
    }

    #[test_case]
    fn memfault_invalid_reports_pending() {
        init_test_env();
        monitor::clear_memfault_pending();

        let addr = 0x1A00_0000u64;
        let info = monitor::MemfaultInfo {
            addr,
            pc: 0x1000,
            kind: WatchpointKind::Read,
            ipa: Some(addr),
            access: monitor::MemfaultAccess::Read,
            size: 4,
            esr: 0,
            far: addr,
            reg: Some(0),
        };
        let decision = monitor::record_memfault(info);
        assert!(decision.should_trap);

        let mut out = [0u8; 256];
        let Some(len) = monitor::bootloader_monitor_handler(b"hp memfault?", &mut out) else {
            panic!("memfault? returned none");
        };
        let text = str::from_utf8(&out[..len]).unwrap();
        assert!(text.starts_with("yes "));
        assert!(text.contains("class=invalid"));
        assert!(text.contains("kind=read"));

        let Some(len) = monitor::bootloader_monitor_handler(b"hp memfault?", &mut out) else {
            panic!("memfault? returned none");
        };
        let text = str::from_utf8(&out[..len]).unwrap();
        assert!(text.starts_with("no"));
    }

    #[test_case]
    fn memfault_allowlisted_is_ignored() {
        init_test_env();
        monitor::clear_memfault_pending();

        let addr = 0x1900_0000u64;
        let info = monitor::MemfaultInfo {
            addr,
            pc: 0x2000,
            kind: WatchpointKind::Read,
            ipa: Some(addr),
            access: monitor::MemfaultAccess::Read,
            size: 4,
            esr: 0,
            far: addr,
            reg: Some(1),
        };
        let decision = monitor::record_memfault(info);
        assert!(decision.ignored);
        assert!(!decision.should_trap);

        let mut out = [0u8; 128];
        let Some(len) = monitor::bootloader_monitor_handler(b"hp memfault?", &mut out) else {
            panic!("memfault? returned none");
        };
        let text = str::from_utf8(&out[..len]).unwrap();
        assert!(text.starts_with("no"));
    }

    #[test_case]
    fn range_normalization_sorts_merges_and_discards_empty() {
        let mut ranges = [
            Range { start: 8, end: 12 },
            Range { start: 0, end: 4 },
            Range { start: 5, end: 6 },
            Range { start: 3, end: 10 },
            Range { start: 12, end: 16 },
            Range { start: 20, end: 20 },
            Range { start: 24, end: 28 },
        ];
        let count = normalize_ranges(&mut ranges);
        assert_eq!(
            &ranges[..count],
            &[Range { start: 0, end: 16 }, Range { start: 24, end: 28 }]
        );
    }
}
