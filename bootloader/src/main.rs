#![feature(once_cell_get_mut)]
#![no_std]
#![no_main]
#![recursion_limit = "256"]

#[macro_use]
pub mod print;
pub mod interfaces;
mod systimer;
use crate::interfaces::pl011::Pl011Uart;
use crate::interfaces::pl011::UartNum;
use crate::print::DEBUG_UART;
use crate::print::NonSyncUnsafeCell;
use core::arch::asm;
use core::arch::global_asm;
use core::cell::OnceCell;
use core::ffi::CStr;
use core::fmt::Write;
use core::iter::Once;
use core::ops::ControlFlow;
use core::panic::PanicInfo;
use core::ptr::slice_from_raw_parts;
use core::slice;
use dtb::DtbParser;
use dtb::{self};
use heapless::String;
use systimer::SystemTimer;
use tock_registers::debug;

unsafe extern "C" {
    static mut _BSS_START: usize;
    static mut _BSS_END: usize;
    static mut _STACK_TOP: usize;
}

static PL011_UART_ADDR: NonSyncUnsafeCell<usize> = NonSyncUnsafeCell::new(0x900_0000);

#[unsafe(no_mangle)]
extern "C" fn main(argc: usize, argv: *const *const u8) -> usize {
    let args = unsafe { slice::from_raw_parts(argv, argc) };
    let dtb = DtbParser::init(unsafe {
        str_to_usize(CStr::from_ptr(args[0]).to_str().unwrap()).unwrap()
    })
    .unwrap();
    let debug_uart_cell = unsafe { &mut *DEBUG_UART.get() };
    dtb.find_node(None, Some("arm,pl011"), &mut |(addr, _size)| {
        unsafe { *PL011_UART_ADDR.get() = addr };
        debug_uart_cell.set(Pl011Uart::new(addr as *const u32));
        ControlFlow::Break(())
    })
    .unwrap();
    let debug_uart = debug_uart_cell.get_mut().unwrap();
    debug_uart.init(UartNum::Debug, 115200);
    debug_uart.write("debug uart starting...\r\n");
    let mut s: String<256> = String::new();
    let mut is_device_name = false;
    println!("Hello println!!!");
    loop {
        s.clear();
        println!("Search by device_name(d) or compatible_name(c)?");
        loop {
            print!("d/c: ");
            let char = debug_uart.read_char();
            debug_uart.write_char(char as char);
            match char {
                b'd' => {
                    is_device_name = true;
                    break;
                }
                b'c' => {
                    is_device_name = false;
                    break;
                }
                _ => println!("\nSorry, try again."),
            }
        }
        println!("\nEnter the search string:");
        loop {
            let char = debug_uart.read_char();
            match char {
                b'\r' | b'\n' => break,
                08 => {
                    s.pop();
                } // backspace
                x => {
                    if !x.is_ascii() || x.is_ascii_control() {
                        continue;
                    } else {
                        s.push(x as char).unwrap();
                    }
                }
            }
            debug_uart.write_char(char as char); // echo back
        }
        if is_device_name {
            println!("\nSearching for device_name: '{}' node...", s);
        } else {
            println!("\nSearching for compatible_name: '{}' node...", s);
        }
        match dtb.find_node(
            if is_device_name { Some(&s) } else { None },
            if is_device_name { None } else { Some(&s) },
            &mut |(addr, size)| {
                println!("address: 0x{:X}, size: 0x{:X}", addr, size);
                ControlFlow::Continue(())
            },
        ) {
            Ok(_) => println!("DTB parsing finished.\nSuccess!!!"),
            Err(x) => println!("An error occurred while parsing the DTB.\nError: {}", x),
        }
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

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let tmp = unsafe { &mut *PL011_UART_ADDR.get() };
    let debug_uart = Pl011Uart::new(*tmp as *const u32);
    debug_uart.init(UartNum::Debug, 115200);
    debug_uart.write("core 0 panicked!!!\r\n");
    let mut s: String<10000> = String::new();
    write!(s, "panicked: {}", info);
    debug_uart.write(&s);
    loop {
        unsafe { asm!("wfi") };
    }
}
