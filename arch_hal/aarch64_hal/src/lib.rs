#![no_std]

use core::cell::OnceCell;
use core::fmt::Write;
use core::fmt::{self};

#[cfg(feature = "uefi-test")]
pub use aarch64_test::*;

pub use cpu;
pub use paging;
pub use pl011;

use mutex::SpinLock;
use pl011::Pl011Uart;

pub static DEBUG_UART: SpinLock<OnceCell<Pl011Uart>> = SpinLock::new(OnceCell::new());

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($fmt:expr, $($arg:tt)+) => ($crate::print!(concat!($fmt, "\n"), $($arg)*));
    ($fmt:expr) => ($crate::print!(concat!($fmt, "\n")));
}

#[macro_export]
macro_rules! pr_trace {
    () => {
        $crate::print!("{}:{}\n", file!(), line!())
    };
}

pub mod debug_uart {
    use pl011::Pl011Uart;

    use crate::DEBUG_UART;

    pub fn init(base_address: usize) {
        let uart = Pl011Uart::new(base_address);
        let debug_uart = DEBUG_UART.lock();
        debug_uart.set(uart).unwrap();
    }
}

pub fn _print(args: fmt::Arguments) {
    let mut debug_uart = DEBUG_UART.lock();
    let uart = debug_uart.get_mut().unwrap();
    uart.write_fmt(args).unwrap();
}
