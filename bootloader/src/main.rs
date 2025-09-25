#![feature(once_cell_get_mut)]
#![no_std]
#![no_main]
#![recursion_limit = "256"]

extern crate alloc;

#[macro_use]
pub mod print;
pub mod interfaces;
mod systimer;
use crate::interfaces::pl011::Pl011Uart;
use crate::interfaces::pl011::UartNum;
use crate::print::DEBUG_UART;
use crate::print::NonSyncUnsafeCell;
use crate::systimer::SystemTimer;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::arch::asm;
use core::arch::naked_asm;
use core::ffi::CStr;
use core::ffi::c_char;
use core::fmt::Write;
use core::ops::ControlFlow;
use core::panic::PanicInfo;
use core::ptr;
use core::slice;
use core::time::Duration;
use dtb::DtbParser;
use elf::Elf64;
use file::StorageDevice;
use heapless::String;

unsafe extern "C" {
    static mut _BSS_START: usize;
    static mut _BSS_END: usize;
    static mut _PROGRAM_START: usize;
    static mut _PROGRAM_END: usize;
    static mut _STACK_TOP: usize;
}

static PL011_UART_ADDR: NonSyncUnsafeCell<usize> = NonSyncUnsafeCell::new(0x900_0000);

#[unsafe(naked)]
#[unsafe(no_mangle)]
extern "C" fn _start() {
    naked_asm!("ldr x0, =_STACK_TOP\n", "mov sp, x0\n", "b main\n",)
}

#[unsafe(no_mangle)]
extern "C" fn main(argc: usize, argv: *const *const u8) -> ! {
    let program_start = unsafe { &raw mut _PROGRAM_START } as *const _ as usize;
    let stack_start = unsafe { &raw mut _STACK_TOP } as *const _ as usize;

    let args = unsafe { slice::from_raw_parts(argv, argc) };
    let dtb = DtbParser::init(unsafe {
        str_to_usize(CStr::from_ptr(args[0] as *const c_char).to_str().unwrap()).unwrap()
    })
    .unwrap();
    let debug_uart_cell = unsafe { &mut *DEBUG_UART.get() };
    dtb.find_node(None, Some("arm,pl011"), &mut |addr, _size| {
        unsafe { *PL011_UART_ADDR.get() = addr };
        let _ = debug_uart_cell.set(Pl011Uart::new(addr as *const u32));
        ControlFlow::Break(())
    })
    .unwrap();
    let debug_uart = debug_uart_cell.get_mut().unwrap();
    debug_uart.init(UartNum::Debug, 115200);
    debug_uart.write("debug uart starting...\r\n");
    let mut systimer = SystemTimer::new();
    systimer.init();
    println!("setup allocator");
    allocator::init();
    dtb.find_node(Some("memory"), None, &mut |addr, size| {
        allocator::add_available_region(addr, size).unwrap();
        ControlFlow::Continue(())
    })
    .unwrap();
    dtb.find_memory_reservation_block(&mut |addr, size| {
        allocator::add_reserved_region(addr, size).unwrap();
        ControlFlow::Continue(())
    });
    dtb.find_reserved_memory_node(
        &mut |addr, size| {
            allocator::add_reserved_region(addr, size).unwrap();
            ControlFlow::Continue(())
        },
        &mut |size, align, alloc_range| -> Result<ControlFlow<()>, ()> {
            if allocator::allocate_dynamic_reserved_region(size, align, alloc_range)
                .unwrap()
                .is_some()
            {
                Ok(ControlFlow::Continue(()))
            } else {
                Err(())
            }
        },
    )
    .unwrap();
    println!("0x{:X}, 0x{:X}", program_start, stack_start);
    allocator::add_reserved_region(program_start, stack_start - program_start).unwrap();
    allocator::finalize().unwrap();
    println!("allocator setup success!!!");
    let mut file_driver = None;
    dtb.find_node(None, Some("virtio,mmio"), &mut |addr, size| {
        if let Ok(driver) = StorageDevice::new_virtio(addr) {
            file_driver = Some(driver);
            // workaround
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })
    .unwrap();
    let file_driver = file_driver.unwrap();
    let elf = file_driver
        .open(0, "/elf-hypervisor.elf", &file::OpenOptions::Read)
        .unwrap();
    println!("hoge");
    let data = elf.read(8).unwrap();
    println!("read success!!!");
    let elf = unsafe { Elf64::new(&data).unwrap() };
    println!("elf parser success!!!");
    loop {}
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

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let tmp = unsafe { &mut *PL011_UART_ADDR.get() };
    let debug_uart = Pl011Uart::new(*tmp as *const u32);
    debug_uart.init(UartNum::Debug, 115200);
    debug_uart.write("core 0 panicked!!!\r\n");
    let mut s: String<100> = String::new();
    let _ = write!(s, "panicked: {}", info);
    debug_uart.write(&s);
    loop {
        unsafe { asm!("wfi") };
    }
}
