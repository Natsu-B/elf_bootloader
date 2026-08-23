//! Serial I/O traits for UART-like devices.

/// Byte-level serial I/O interface.
pub trait SerialByteIo {
    /// Error type for I/O operations.
    type Error;

    /// Attempts to read a byte without blocking.
    fn try_read_byte(&self) -> Option<u8>;

    /// Attempts to write a byte without blocking.
    fn try_write_byte(&self, byte: u8) -> bool;

    /// Flushes any pending output.
    fn flush(&self) -> Result<(), Self::Error>;
}

/// Serial receive interrupt handler.
pub trait SerialRxIrq {
    /// Handles receive interrupts, invoking `on_byte` for each received byte.
    fn handle_rx_irq(&self, on_byte: &mut dyn FnMut(u8));
}

/// Serial interrupt control.
pub trait SerialIrqCtrl {
    /// Enables or disables receive interrupts.
    fn set_rx_irq_enabled(&self, enabled: bool);

    /// Enables or disables transmit interrupts.
    fn set_tx_irq_enabled(&self, enabled: bool);
}
