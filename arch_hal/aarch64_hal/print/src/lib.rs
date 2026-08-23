//! Debug UART and formatted output primitives for early bring-up.

#![no_std]

pub mod pl011;
pub mod stream;
use core::cell::OnceCell;
use core::fmt::Write;
use core::fmt::{self};

use mutex::RawSpinLock;

pub use pl011::Pl011Uart;

pub static DEBUG_UART: RawSpinLock<DebugUart> = RawSpinLock::new(DebugUart::new());

#[derive(Clone, Copy)]
pub struct MirrorOps {
    pub write: fn(&str),
    pub flush: fn(),
}

#[derive(Default)]
pub struct DebugUart {
    uart: OnceCell<Pl011Uart>,
    mirror: Option<MirrorOps>,
}

impl DebugUart {
    /// Creates an uninitialized debug UART container.
    pub const fn new() -> DebugUart {
        DebugUart {
            uart: OnceCell::new(),
            mirror: None,
        }
    }

    /// Initializes the primary PL011 debug UART.
    pub fn init(&mut self, uart_peripherals: usize, uart_clk: u64, baud_rate: u32) {
        let mut uart = Pl011Uart::new(uart_peripherals, uart_clk);
        uart.init(baud_rate);

        match self.uart.set(uart) {
            Ok(_) => (),
            Err(_) => panic!("UART already initialized"),
        }
    }

    /// Returns mutable access to the initialized UART, if present.
    pub fn get_mut(&mut self) -> Option<&mut Pl011Uart> {
        self.uart.get_mut()
    }

    /// Releases EL2-side interrupt ownership while keeping the UART configured.
    pub fn detach_for_guest_passthrough(&mut self) {
        let Some(uart) = self.get_mut() else { return };

        uart.detach_for_guest_passthrough();
    }
}

struct MirrorWriter<'a> {
    uart: Option<&'a mut Pl011Uart>,
    mirror: Option<MirrorOps>,
}

impl<'a> fmt::Write for MirrorWriter<'a> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if let Some(uart) = self.uart.as_mut() {
            uart.write(s);
        }
        if let Some(mirror) = self.mirror {
            (mirror.write)(s);
        }
        Ok(())
    }
}

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
macro_rules! print_force {
    ($($arg:tt)*) => {
        ($crate::_print_force(format_args!($($arg)*)));
    };
}

#[macro_export]
macro_rules! println_force {
    () => {$crate::print_force!("\n");};
    ($fmt:expr, $($arg:tt)+) => {
        $crate::print_force!(concat!($fmt, "\n"), $($arg)*);
    };
    ($fmt:expr) => {
        $crate::print_force!(concat!($fmt, "\n"));
    };
}

#[macro_export]
macro_rules! pr_trace {
    () => {
        $crate::print!("{}:{}\n", file!(), line!())
    };
}

/// Initializes the global debug UART instance.
pub fn init(uart_peripherals: usize, uart_clk: u64, baud_rate: u32) {
    DEBUG_UART
        .lock()
        .init(uart_peripherals, uart_clk, baud_rate);
}

/// Sets or clears an optional mirror output sink.
pub fn set_mirror(ops: Option<MirrorOps>) {
    DEBUG_UART.lock().mirror = ops;
}

/// Writes formatted output to the primary UART and optional mirror sink.
pub fn write(args: fmt::Arguments) {
    let mut guard = DEBUG_UART.lock();
    let mirror = guard.mirror;
    let uart = guard.get_mut();

    debug_assert!(
        uart.is_some() || mirror.is_some(),
        "print::write called before debug UART or mirror was initialized"
    );

    let mut writer = MirrorWriter { uart, mirror };
    writer.write_fmt(args).unwrap();
}

/// `print!` backend.
pub fn _print(args: fmt::Arguments) {
    write(args);
}

/// Lockless print path for contexts where taking the lock may deadlock.
pub fn _print_force(args: fmt::Arguments) {
    // SAFETY: Caller must ensure this is only used in contexts where taking the lock
    // would deadlock (e.g. panic path), and accept potential interleaving.
    let mut guard = unsafe { DEBUG_UART.no_lock() };
    let mirror = guard.mirror;
    let uart = guard.get_mut();

    debug_assert!(
        uart.is_some() || mirror.is_some(),
        "print::_print_force called before debug UART or mirror was initialized"
    );

    let mut writer = MirrorWriter { uart, mirror };
    writer.write_fmt(args).unwrap();
}

/// Flushes the primary UART and optional mirror sink.
pub fn flush() {
    let mut guard = DEBUG_UART.lock();
    if let Some(uart) = guard.get_mut() {
        uart.flush();
    }
    if let Some(mirror) = guard.mirror {
        (mirror.flush)();
    }
}

pub mod debug_uart {
    use super::*;

    /// Initializes the debug UART.
    pub fn init(uart_peripherals: usize, uart_clk: u64, baud_rate: u32) {
        super::init(uart_peripherals, uart_clk, baud_rate);
    }

    /// Writes a string to the debug UART and optional mirror sink.
    pub fn write(s: &str) {
        let mut guard = DEBUG_UART.lock();
        let mirror = guard.mirror;
        if let Some(uart) = guard.get_mut() {
            uart.write(s);
        }
        if let Some(mirror) = mirror {
            (mirror.write)(s);
        }
    }

    /// Lockless formatted print path for deadlock-sensitive contexts.
    pub fn print_force(args: fmt::Arguments) {
        super::_print_force(args);
    }

    /// Configure PL011 RX-related interrupts (FIFO trigger + timeout).
    ///
    /// Intended for early bring-up. This function takes the lock.
    pub fn enable_rx_interrupts(level: pl011::FifoLevel, enable_timeout: bool) {
        let mut guard = DEBUG_UART.lock();
        let Some(uart) = guard.get_mut() else { return };

        uart.enable_rx_interrupts(level, enable_timeout);
    }

    /// Releases EL2-side PL011 interrupt ownership before guest entry.
    ///
    /// EL2 may use the PL011 for early boot logs, but once guest handoff is
    /// imminent Linux owns the passthrough console input path. This only masks
    /// EL2 RX/TX interrupts and clears pending UART interrupt state.
    pub fn detach_for_guest_passthrough() {
        DEBUG_UART.lock().detach_for_guest_passthrough();
    }

    /// IRQ-context RX handler that never takes the lock.
    ///
    /// # Safety
    /// This uses `no_lock()` to avoid deadlocks if IRQs can preempt code holding the lock.
    /// The caller must accept concurrent access/interleaving on the UART MMIO.
    pub fn handle_rx_irq_force(mut on_byte: impl FnMut(u8)) {
        // SAFETY: see doc comment.
        let mut guard = unsafe { DEBUG_UART.no_lock() };
        let Some(uart) = guard.get_mut() else { return };

        uart.handle_rx_irq(&mut on_byte);
    }
}
