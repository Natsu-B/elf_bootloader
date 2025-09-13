use crate::interfaces::pl011::Pl011Uart;
use core::cell::OnceCell;
use core::cell::UnsafeCell;
use core::fmt::Write;
use core::fmt::{self};

// A wrapper for UnsafeCell that is !Send but is Sync.
// This is safe to use in a single-threaded environment for statics.
pub struct NonSyncUnsafeCell<T> {
    data: UnsafeCell<T>,
}

// This is safe because in our bare-metal, single-threaded context,
// we are responsible for ensuring that we don't have data races.
// By implementing Sync, we are telling the compiler that it's safe
// to have multiple references to this data across threads, but since
// we only have one thread, this is trivially true.
// We must not implement Send, as we don't want to move the cell itself.
unsafe impl<T> Sync for NonSyncUnsafeCell<T> {}

impl<T> NonSyncUnsafeCell<T> {
    // Creates a new NonSyncUnsafeCell.
    pub const fn new(data: T) -> Self {
        Self {
            data: UnsafeCell::new(data),
        }
    }

    // Gets a mutable pointer to the inner data.
    // This is unsafe because it bypasses the borrow checker.
    pub fn get(&self) -> *mut T {
        self.data.get()
    }
}

pub static DEBUG_UART: NonSyncUnsafeCell<OnceCell<Pl011Uart>> =
    NonSyncUnsafeCell::new(OnceCell::new());

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::print::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("
"));
    ($fmt:expr, $($arg:tt)+) => ($crate::print!(concat!($fmt, "
"), $($arg)*));
    ($fmt:expr) => ($crate::print!(concat!($fmt, "
")));
}

pub fn _print(args: fmt::Arguments) {
    let debug_uart_cell = unsafe { &mut *DEBUG_UART.get() };
    let uart = debug_uart_cell.get_mut().unwrap();
    uart.write_fmt(args).unwrap();
}
