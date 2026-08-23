use core::convert::Infallible;
use core::fmt;
use io_api::serial::SerialByteIo;
use io_api::serial::SerialIrqCtrl;
use io_api::serial::SerialRxIrq;

use typestate::ReadOnly;
use typestate::ReadWrite;
use typestate::Readable;
use typestate::Writable;
use typestate::WriteOnly;
use typestate::bitregs;

bitregs! {
    /// UART Data Register (UARTDR)
    pub struct UARTDR: u32 {
        pub data@[7:0],
        pub fe@[8:8],
        pub pe@[9:9],
        pub be@[10:10],
        pub oe@[11:11],
        reserved@[31:12] [ignore],
    }
}

bitregs! {
    /// UART Flag Register (UARTFR)
    pub struct UARTFR: u32 {
        pub cts@[0:0],
        pub dsr@[1:1],
        pub dcd@[2:2],
        pub busy@[3:3],
        pub rxfe@[4:4],
        pub txff@[5:5],
        pub rxff@[6:6],
        pub txfe@[7:7],
        pub ri@[8:8],
        reserved@[31:9] [ignore],
    }
}

bitregs! {
    /// UART Line Control Register (UARTLCR_H)
    pub struct UARTLCR: u32 {
        pub brk@[0:0],
        pub pen@[1:1],
        pub eps@[2:2],
        pub stp2@[3:3],
        pub fen@[4:4],
        pub wlen@[6:5] as WordLen {
            Bits5 = 0b00,
            Bits6 = 0b01,
            Bits7 = 0b10,
            Bits8 = 0b11,
        },
        pub sps@[7:7],
        reserved@[31:8] [ignore],
    }
}

bitregs! {
    /// UART Control Register (UARTCR)
    pub struct UARTCR: u32 {
        pub uart_en@[0:0],
        pub sir_en@[1:1],
        pub sir_lp@[2:2],
        reserved@[6:3] [ignore],
        pub lbe@[7:7],
        pub txe@[8:8],
        pub rxe@[9:9],
        pub dtr@[10:10],
        pub rts@[11:11],
        pub out1@[12:12],
        pub out2@[13:13],
        pub rtsen@[14:14],
        pub ctsen@[15:15],
        reserved@[31:16] [ignore],
    }
}

bitregs! {
    /// UART Interrupt FIFO Level Select Register (UARTIFLS)
    pub struct UARTIFLS: u32 {
        pub txiflsel@[2:0],
        pub rxiflsel@[5:3],
        reserved@[31:6] [ignore],
    }
}

bitregs! {
    /// UART Interrupt Mask Set/Clear Register (UARTIMSC)
    pub struct UARTIMSC: u32 {
        pub rimim@[0:0],
        pub ctsmim@[1:1],
        pub dcdmim@[2:2],
        pub dsrmim@[3:3],
        pub rxim@[4:4],
        pub txim@[5:5],
        pub rtim@[6:6],
        pub feim@[7:7],
        pub peim@[8:8],
        pub beim@[9:9],
        pub oeim@[10:10],
        reserved@[31:11] [ignore],
    }
}

bitregs! {
    /// UART Raw Interrupt Status Register (UARTRIS)
    pub struct UARTRIS: u32 {
        pub rirmis@[0:0],
        pub ctsrmis@[1:1],
        pub dcdrmis@[2:2],
        pub dsrrmis@[3:3],
        pub rxris@[4:4],
        pub txris@[5:5],
        pub rtris@[6:6],
        pub feris@[7:7],
        pub peris@[8:8],
        pub beris@[9:9],
        pub oeris@[10:10],
        reserved@[31:11] [ignore],
    }
}

bitregs! {
    /// UART Masked Interrupt Status Register (UARTMIS)
    pub struct UARTMIS: u32 {
        pub rimmis@[0:0],
        pub ctsmmis@[1:1],
        pub dcdmmis@[2:2],
        pub dsrmmis@[3:3],
        pub rxmis@[4:4],
        pub txmis@[5:5],
        pub rtmis@[6:6],
        pub femis@[7:7],
        pub pemis@[8:8],
        pub bemis@[9:9],
        pub oemis@[10:10],
        reserved@[31:11] [ignore],
    }
}

bitregs! {
    /// UART Interrupt Clear Register (UARTICR)
    pub struct UARTICR: u32 {
        pub rimic@[0:0],
        pub ctsmic@[1:1],
        pub dcdmic@[2:2],
        pub dsrmic@[3:3],
        pub rxic@[4:4],
        pub txic@[5:5],
        pub rtic@[6:6],
        pub feic@[7:7],
        pub peic@[8:8],
        pub beic@[9:9],
        pub oeic@[10:10],
        reserved@[31:11] [ignore],
    }
}

impl UARTICR {
    pub fn all() -> Self {
        Self::new()
            .set(Self::rimic, 1)
            .set(Self::ctsmic, 1)
            .set(Self::dcdmic, 1)
            .set(Self::dsrmic, 1)
            .set(Self::rxic, 1)
            .set(Self::txic, 1)
            .set(Self::rtic, 1)
            .set(Self::feic, 1)
            .set(Self::peic, 1)
            .set(Self::beic, 1)
            .set(Self::oeic, 1)
    }
}

/// FIFO interrupt trigger level selector for UARTIFLS.{rxiflsel,txiflsel}.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum FifoLevel {
    OneEighth = 0b000,
    OneQuarter = 0b001,
    OneHalf = 0b010,
    ThreeQuarters = 0b011,
    SevenEighths = 0b100,
}

impl From<FifoLevel> for u32 {
    fn from(v: FifoLevel) -> Self {
        v as u32
    }
}

impl TryFrom<u32> for FifoLevel {
    type Error = ();

    fn try_from(v: u32) -> Result<Self, Self::Error> {
        match v & 0b111 {
            0b000 => Ok(FifoLevel::OneEighth),
            0b001 => Ok(FifoLevel::OneQuarter),
            0b010 => Ok(FifoLevel::OneHalf),
            0b011 => Ok(FifoLevel::ThreeQuarters),
            0b100 => Ok(FifoLevel::SevenEighths),
            _ => Err(()),
        }
    }
}

#[repr(C)]
struct Pl011Peripherals {
    data: ReadWrite<UARTDR>,
    error_status: ReadWrite<u32>,
    reserved0: [u32; 4],
    flag: ReadOnly<UARTFR>,
    reserved1: [u32; 2],
    integer_baud_rate_divisor: ReadWrite<u32>,
    fractional_baud_rate_divisor: ReadWrite<u32>,
    line_control: ReadWrite<UARTLCR>,
    control: ReadWrite<UARTCR>,

    interrupt_fifo_level_select: ReadWrite<UARTIFLS>,
    interrupt_mask_set_clear: ReadWrite<UARTIMSC>,
    raw_interrupt_status: ReadOnly<UARTRIS>,
    masked_interrupt_status: ReadOnly<UARTMIS>,
    interrupt_clear: WriteOnly<UARTICR>,
}

#[allow(dead_code)]
pub struct Pl011Uart {
    registers: &'static mut Pl011Peripherals,
    pub uart_clk: u64,
}

impl Pl011Uart {
    pub fn new(uart_peripherals: usize, uart_clk: u64) -> Pl011Uart {
        let pl011_uart =
            unsafe { &mut *(uart_peripherals as *mut Pl011Peripherals).cast::<Pl011Peripherals>() };

        Self {
            registers: pl011_uart,
            uart_clk,
        }
    }

    pub fn flush(&self) {
        self.registers
            .control
            .set_bits(UARTCR::new().set(UARTCR::uart_en, 1).set(UARTCR::txe, 1));
        self.registers
            .control
            .clear_bits(UARTCR::new().set(UARTCR::ctsen, 1));
        self.registers
            .line_control
            .clear_bits(UARTLCR::new().set(UARTLCR::brk, 1));

        while self.registers.flag.read().get(UARTFR::busy) != 0 {}
    }

    pub fn disabled(&self) {
        self.registers
            .control
            .clear_bits(UARTCR::new().set(UARTCR::uart_en, 1));
        self.registers
            .interrupt_mask_set_clear
            .write(UARTIMSC::new());
        self.registers.interrupt_clear.write(UARTICR::all());
    }

    pub fn init(&mut self, baud_rate: u32) {
        self.flush();
        self.disabled();

        assert!(self.uart_clk > 368_6400);
        let divisor = self.uart_clk * 4 / baud_rate as u64;
        let integer_divisor = divisor / 64;
        let fractional_divisor = divisor % 64;

        self.registers
            .integer_baud_rate_divisor
            .write(integer_divisor as u32);
        self.registers
            .fractional_baud_rate_divisor
            .write(fractional_divisor as u32);

        self.registers.line_control.write(
            UARTLCR::new()
                .set_enum(UARTLCR::wlen, WordLen::Bits8)
                .set(UARTLCR::fen, 1),
        );

        self.registers.control.write(
            UARTCR::new()
                .set(UARTCR::uart_en, 1)
                .set(UARTCR::txe, 1)
                .set(UARTCR::rxe, 1),
        );

        self.registers
            .interrupt_fifo_level_select
            .write(UARTIFLS::new());
        self.registers
            .interrupt_mask_set_clear
            .write(UARTIMSC::new());
        self.registers.interrupt_clear.write(UARTICR::all());
    }

    pub fn write(&mut self, string: &str) {
        for ch in string.bytes() {
            if ch == b'\n' {
                self.pushb(b'\r');
            }
            self.pushb(ch);
        }
    }

    pub fn write_byte(&self, byte: u8) {
        self.pushb(byte);
    }

    pub fn try_write_byte(&self, byte: u8) -> bool {
        if self.registers.flag.read().get(UARTFR::txff) != 0 {
            return false;
        }
        self.registers
            .data
            .write(UARTDR::new().set(UARTDR::data, byte as u32));
        true
    }

    pub fn drain_rx(&self) {
        while self.registers.flag.read().get(UARTFR::rxfe) == 0 {
            let _ = self.read_char();
        }
    }

    fn pushb(&self, ch: u8) {
        while self.registers.flag.read().get(UARTFR::txff) != 0 {}
        self.registers
            .data
            .write(UARTDR::new().set(UARTDR::data, ch as u32));
    }

    pub fn read_char(&self) -> u8 {
        while self.registers.flag.read().get(UARTFR::rxfe) != 0 {}

        let read = self.registers.data.read();
        let has_err = read.get(UARTDR::fe) != 0
            || read.get(UARTDR::pe) != 0
            || read.get(UARTDR::be) != 0
            || read.get(UARTDR::oe) != 0;

        if has_err {
            self.registers.error_status.write(0);
        }

        read.get(UARTDR::data) as u8
    }

    pub fn try_read_byte(&self) -> Option<u8> {
        if self.registers.flag.read().get(UARTFR::rxfe) != 0 {
            return None;
        }

        let read = self.registers.data.read();
        let has_err = read.get(UARTDR::fe) != 0
            || read.get(UARTDR::pe) != 0
            || read.get(UARTDR::be) != 0
            || read.get(UARTDR::oe) != 0;

        if has_err {
            self.registers.error_status.write(0);
        }

        Some(read.get(UARTDR::data) as u8)
    }

    /// Configure FIFO interrupt trigger levels.
    pub fn set_fifo_irq_levels(&self, rx: FifoLevel, tx: FifoLevel) {
        self.registers.interrupt_fifo_level_select.write(
            UARTIFLS::new()
                .set_enum(UARTIFLS::rxiflsel, rx)
                .set_enum(UARTIFLS::txiflsel, tx),
        );
    }

    /// Enable RX-related interrupts (RX FIFO + optional timeout).
    pub fn enable_rx_interrupts(&self, rx_level: FifoLevel, enable_timeout: bool) {
        self.set_fifo_irq_levels(rx_level, FifoLevel::OneEighth);
        self.registers.interrupt_clear.write(UARTICR::all());

        let mut mask = UARTIMSC::new().set(UARTIMSC::rxim, 1);
        if enable_timeout {
            mask = mask.set(UARTIMSC::rtim, 1);
        }
        self.clear_interrupts(UARTICR::all());
        self.enable_interrupts(mask);
    }

    /// Stop EL2-side UART interrupt ownership while leaving the UART configured.
    pub fn detach_for_guest_passthrough(&self) {
        self.set_rx_irq_enabled(false);
        self.set_tx_irq_enabled(false);
        self.clear_interrupts(UARTICR::all());
    }

    /// Enable selected interrupts by unmasking bits in UARTIMSC.
    pub fn enable_interrupts(&self, mask: UARTIMSC) {
        self.registers.interrupt_mask_set_clear.set_bits(mask);
    }

    /// Disable selected interrupts by masking bits in UARTIMSC.
    pub fn disable_interrupts(&self, mask: UARTIMSC) {
        self.registers.interrupt_mask_set_clear.clear_bits(mask);
    }

    pub fn masked_interrupt_status(&self) -> UARTMIS {
        self.registers.masked_interrupt_status.read()
    }

    pub fn raw_interrupt_status(&self) -> UARTRIS {
        self.registers.raw_interrupt_status.read()
    }

    pub fn clear_interrupts(&self, mask: UARTICR) {
        self.registers.interrupt_clear.write(mask);
    }

    /// Handle RX/RT interrupts: drain RX FIFO and clear relevant interrupt sources.
    pub fn handle_rx_irq(&self, on_byte: &mut impl FnMut(u8)) {
        let mis = self.masked_interrupt_status();

        let need_rx = mis.get(UARTMIS::rxmis) != 0 || mis.get(UARTMIS::rtmis) != 0;
        if need_rx {
            while let Some(b) = self.try_read_byte() {
                on_byte(b);
            }
        }

        // Clear the sources that are commonly asserted for RX handling.
        // For level interrupts, draining RX FIFO usually deasserts the line, but clearing is still
        // recommended per TRM interrupt clear semantics.
        // (UARTICR is WO; write-1-to-clear for each bit.)
        let mut icr = UARTICR::new();

        if mis.get(UARTMIS::rxmis) != 0 {
            icr = icr.set(UARTICR::rxic, 1);
        }
        if mis.get(UARTMIS::rtmis) != 0 {
            icr = icr.set(UARTICR::rtic, 1);
        }
        if mis.get(UARTMIS::femis) != 0 {
            icr = icr.set(UARTICR::feic, 1);
        }
        if mis.get(UARTMIS::pemis) != 0 {
            icr = icr.set(UARTICR::peic, 1);
        }
        if mis.get(UARTMIS::bemis) != 0 {
            icr = icr.set(UARTICR::beic, 1);
        }
        if mis.get(UARTMIS::oemis) != 0 {
            icr = icr.set(UARTICR::oeic, 1);
        }

        if icr.bits() != 0 {
            self.clear_interrupts(icr);
        }
    }
}

impl fmt::Write for Pl011Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write(s);
        Ok(())
    }
}

unsafe impl Send for Pl011Uart {}
// SAFETY: Pl011Uart provides interior mutability over MMIO registers; callers must ensure
// any concurrent access is synchronized at a higher level (e.g. by masking IRQs or locks).
unsafe impl Sync for Pl011Uart {}

impl SerialByteIo for Pl011Uart {
    type Error = Infallible;

    fn try_read_byte(&self) -> Option<u8> {
        Pl011Uart::try_read_byte(self)
    }

    fn try_write_byte(&self, byte: u8) -> bool {
        Pl011Uart::try_write_byte(self, byte)
    }

    fn flush(&self) -> Result<(), Self::Error> {
        Pl011Uart::flush(self);
        Ok(())
    }
}

impl SerialRxIrq for Pl011Uart {
    fn handle_rx_irq(&self, on_byte: &mut dyn FnMut(u8)) {
        Pl011Uart::handle_rx_irq(self, &mut |byte| on_byte(byte));
    }
}

impl SerialIrqCtrl for Pl011Uart {
    fn set_rx_irq_enabled(&self, enabled: bool) {
        let mask = UARTIMSC::new()
            .set(UARTIMSC::rxim, 1)
            .set(UARTIMSC::rtim, 1);
        if enabled {
            self.enable_interrupts(mask);
        } else {
            self.disable_interrupts(mask);
        }
    }

    fn set_tx_irq_enabled(&self, enabled: bool) {
        let mask = UARTIMSC::new().set(UARTIMSC::txim, 1);
        if enabled {
            self.enable_interrupts(mask);
        } else {
            self.disable_interrupts(mask);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::boxed::Box;
    use typestate::Readable;
    use typestate::Writable;

    use super::Pl011Peripherals;
    use super::Pl011Uart;
    use super::UARTICR;
    use super::UARTIMSC;

    struct TestPl011 {
        registers: *mut Pl011Peripherals,
        uart: Pl011Uart,
    }

    impl TestPl011 {
        fn new() -> Self {
            // SAFETY: A zeroed register block is sufficient for host-side MMIO tests because
            // the register wrappers are plain volatile storage and every field accepts zero.
            let registers =
                Box::into_raw(Box::new(unsafe { core::mem::zeroed::<Pl011Peripherals>() }));
            let uart = Pl011Uart::new(registers as usize, 48_000_000);

            Self { registers, uart }
        }

        fn interrupt_mask(&self) -> UARTIMSC {
            unsafe { (*self.registers).interrupt_mask_set_clear.read() }
        }

        fn interrupt_clear(&self) -> UARTICR {
            unsafe { core::ptr::read_volatile((*self.registers).interrupt_clear.as_mut_ptr()) }
        }
    }

    impl Drop for TestPl011 {
        fn drop(&mut self) {
            unsafe {
                drop(Box::from_raw(self.registers));
            }
        }
    }

    #[test]
    fn detach_for_guest_passthrough_masks_rx_tx_irqs_and_clears_pending_state() {
        let fixture = TestPl011::new();

        unsafe {
            (*fixture.registers).interrupt_mask_set_clear.write(
                UARTIMSC::new()
                    .set(UARTIMSC::rxim, 1)
                    .set(UARTIMSC::rtim, 1)
                    .set(UARTIMSC::txim, 1)
                    .set(UARTIMSC::feim, 1),
            );
        }

        fixture.uart.detach_for_guest_passthrough();

        let mask = fixture.interrupt_mask();
        assert_eq!(mask.get(UARTIMSC::rxim), 0);
        assert_eq!(mask.get(UARTIMSC::rtim), 0);
        assert_eq!(mask.get(UARTIMSC::txim), 0);
        assert_eq!(mask.get(UARTIMSC::feim), 1);
        assert_eq!(fixture.interrupt_clear().bits(), UARTICR::all().bits());
    }
}
