#![no_std]

use core::fmt;

use typestate::ReadOnly;
use typestate::ReadWrite;
use typestate::Readable;
use typestate::Writable;
use typestate::WriteOnly;
use typestate_macro::RawReg;

#[repr(C)]
#[derive(Debug)]
pub struct Pl011Peripherals {
    pub data: ReadWrite<UARTDR>,                     // 0x0000
    pub error_status: ReadWrite<u32>,                // 0x0004
    _reserved0008: [u8; 0x10],                       // 0x0008..0x0018
    pub flags: ReadOnly<UARTFR>,                     // 0x0018
    _reserved001c: [u8; 0x04],                       // 0x001C..0x0020
    pub irda_low_power_counter: ReadWrite<u32>,      // 0x0020
    pub integer_baud_rate: ReadWrite<u32>,           // 0x0024
    pub fractional_baud_rate: ReadWrite<u32>,        // 0x0028
    pub line_control: ReadWrite<UARTLCR>,            // 0x002C
    pub control: ReadWrite<UARTCR>,                  // 0x0030
    pub interrupt_fifo_level_select: ReadWrite<u32>, // 0x0034
    pub interrupt_mask_set_clear: ReadWrite<u32>,    // 0x0038
    pub raw_interrupt_status: ReadOnly<u32>,         // 0x003C
    pub masked_interrupt_status: ReadOnly<u32>,      // 0x0040
    pub interrupt_clear: WriteOnly<UARTICR>,         // 0x0044
    pub dma_control: ReadWrite<u32>,                 // 0x0048
    _reserved004c: [u8; 3988],                       // 0x004C..0x0FE0
    pub peripheral_id: [ReadOnly<u32>; 4],           // 0x0FE0..0x0FF0
    pub pcell_id: [ReadOnly<u32>; 4],                // 0x0FF0..0x1000
                                                     // @END (0x1000)
}

#[inline(always)]
const fn mask(width: u32) -> u32 {
    (1u32 << width) - 1
}

/// UART Data Register
#[repr(transparent)]
#[derive(Clone, Copy, RawReg, PartialEq, Eq, Debug)]
pub struct UARTDR(pub u32);

impl UARTDR {
    pub const DATA_OFFSET: u32 = 0; // DATA OFFSET(0) NUMBITS(8)
    pub const DATA_MASK: Self = Self(mask(8) << Self::DATA_OFFSET);

    pub const FE_OFFSET: u32 = 8; // framing error
    pub const FE_MASK: Self = Self(mask(1) << Self::FE_OFFSET);

    pub const PE_OFFSET: u32 = 9; // parity error
    pub const PE_MASK: Self = Self(mask(1) << Self::PE_OFFSET);

    pub const BE_OFFSET: u32 = 10; // break error
    pub const BE_MASK: Self = Self(mask(1) << Self::BE_OFFSET);

    pub const OE_OFFSET: u32 = 11; // overrun error
    pub const OE_MASK: Self = Self(mask(1) << Self::OE_OFFSET);
}

/// UART Flag Register
#[repr(transparent)]
#[derive(Clone, Copy, RawReg, PartialEq, Eq, Debug)]
pub struct UARTFR(pub u32);

impl UARTFR {
    pub const CTS_OFFSET: u32 = 0; // clear to send
    pub const CTS_MASK: Self = Self(1 << Self::CTS_OFFSET);

    pub const DSR_OFFSET: u32 = 1; // data set ready
    pub const DSR_MASK: Self = Self(1 << Self::DSR_OFFSET);

    pub const DCD_OFFSET: u32 = 2; // data carrier detect
    pub const DCD_MASK: Self = Self(1 << Self::DCD_OFFSET);

    pub const BUSY_OFFSET: u32 = 3; // UART busy
    pub const BUSY_MASK: Self = Self(1 << Self::BUSY_OFFSET);

    pub const RXFE_OFFSET: u32 = 4; // receive FIFO empty
    pub const RXFE_MASK: Self = Self(1 << Self::RXFE_OFFSET);

    pub const TXFF_OFFSET: u32 = 5; // transmit FIFO full
    pub const TXFF_MASK: Self = Self(1 << Self::TXFF_OFFSET);

    pub const RXFF_OFFSET: u32 = 6; // receive FIFO full
    pub const RXFF_MASK: Self = Self(1 << Self::RXFF_OFFSET);

    pub const TXFE_OFFSET: u32 = 7; // transmit FIFO empty
    pub const TXFE_MASK: Self = Self(1 << Self::TXFE_OFFSET);

    pub const RI_OFFSET: u32 = 8; // ring indicator
    pub const RI_MASK: Self = Self(1 << Self::RI_OFFSET);
}

/// UART Line Control Register
#[repr(transparent)]
#[derive(Clone, Copy, RawReg, PartialEq, Eq, Debug)]
pub struct UARTLCR(pub u32);

impl UARTLCR {
    pub const BRK_OFFSET: u32 = 0; // send break
    pub const BRK_MASK: Self = Self(1 << Self::BRK_OFFSET);

    pub const PEN_OFFSET: u32 = 1; // parity enable
    pub const PEN_MASK: Self = Self(1 << Self::PEN_OFFSET);

    pub const EPS_OFFSET: u32 = 2; // parity select
    pub const EPS_MASK: Self = Self(1 << Self::EPS_OFFSET);

    pub const STP2_OFFSET: u32 = 3; // two stop bits select
    pub const STP2_MASK: Self = Self(1 << Self::STP2_OFFSET);

    pub const FEN_OFFSET: u32 = 4; // enable FIFO
    pub const FEN_MASK: Self = Self(1 << Self::FEN_OFFSET);

    pub const WLEN_OFFSET: u32 = 5; // word length NUMBITS(2)
    pub const WLEN_MASK: Self = Self(mask(2) << Self::WLEN_OFFSET);

    pub const SPS_OFFSET: u32 = 7; // enable stick parity
    pub const SPS_MASK: Self = Self(1 << Self::SPS_OFFSET);

    // WLEN values
    pub const WLEN_BIT8: u32 = 0b11;
    pub const WLEN_BIT8_MASK: Self = Self(Self::WLEN_BIT8 << Self::WLEN_OFFSET);
    pub const WLEN_BIT7: u32 = 0b10;
    pub const WLEN_BIT7_MASK: Self = Self(Self::WLEN_BIT7 << Self::WLEN_OFFSET);
    pub const WLEN_BIT6: u32 = 0b01;
    pub const WLEN_BIT6_MASK: Self = Self(Self::WLEN_BIT6 << Self::WLEN_OFFSET);
    pub const WLEN_BIT5: u32 = 0b00;
    pub const WLEN_BIT5_MASK: Self = Self(Self::WLEN_BIT5 << Self::WLEN_OFFSET);
}

/// UART Control Register
#[repr(transparent)]
#[derive(Clone, Copy, RawReg, PartialEq, Eq, Debug)]
pub struct UARTCR(pub u32);

impl UARTCR {
    pub const UARTEN_OFFSET: u32 = 0; // enable UART
    pub const UARTEN_MASK: Self = Self(1 << Self::UARTEN_OFFSET);

    pub const SIREN_OFFSET: u32 = 1; // enable SIR
    pub const SIREN_MASK: Self = Self(1 << Self::SIREN_OFFSET);

    pub const SIRLP_OFFSET: u32 = 2; // enable SIR low power
    pub const SIRLP_MASK: Self = Self(1 << Self::SIRLP_OFFSET);

    pub const LBE_OFFSET: u32 = 7; // loopback enable
    pub const LBE_MASK: Self = Self(1 << Self::LBE_OFFSET);

    pub const TXE_OFFSET: u32 = 8; // transmit enable
    pub const TXE_MASK: Self = Self(1 << Self::TXE_OFFSET);

    pub const RXE_OFFSET: u32 = 9; // receive enable
    pub const RXE_MASK: Self = Self(1 << Self::RXE_OFFSET);

    pub const DTR_OFFSET: u32 = 10; // data transmit ready
    pub const DTR_MASK: Self = Self(1 << Self::DTR_OFFSET);

    pub const RTS_OFFSET: u32 = 11; // request to send (旧コードでは LTS)
    pub const RTS_MASK: Self = Self(1 << Self::RTS_OFFSET);

    pub const OUT1_OFFSET: u32 = 12;
    pub const OUT1_MASK: Self = Self(1 << Self::OUT1_OFFSET);

    pub const OUT2_OFFSET: u32 = 13;
    pub const OUT2_MASK: Self = Self(1 << Self::OUT2_OFFSET);

    pub const RTSEN_OFFSET: u32 = 14; // RTS hardware flow control enable
    pub const RTSEN_MASK: Self = Self(1 << Self::RTSEN_OFFSET);

    pub const CTSEN_OFFSET: u32 = 15; // CTS hardware flow control enable
    pub const CTSEN_MASK: Self = Self(1 << Self::CTSEN_OFFSET);
}

/// UART Interrupt Clear Register
#[repr(transparent)]
#[derive(Clone, Copy, RawReg, PartialEq, Eq, Debug)]
pub struct UARTICR(pub u32);

impl UARTICR {
    pub const RIMIC_OFFSET: u32 = 0; // nUARTRI modem interrupt clear
    pub const RIMIC_MASK: Self = Self(1 << Self::RIMIC_OFFSET);

    pub const CTSMIC_OFFSET: u32 = 1; // nUARTCTS modem interrupt clear
    pub const CTSMIC_MASK: Self = Self(1 << Self::CTSMIC_OFFSET);

    pub const DCDMIC_OFFSET: u32 = 2; // nUARTDCD modem interrupt clear
    pub const DCDMIC_MASK: Self = Self(1 << Self::DCDMIC_OFFSET);

    pub const DSRMIC_OFFSET: u32 = 3; // nUARTDSR modem interrupt clear
    pub const DSRMIC_MASK: Self = Self(1 << Self::DSRMIC_OFFSET);

    pub const RXIC_OFFSET: u32 = 4; // Receive interrupt clear
    pub const RXIC_MASK: Self = Self(1 << Self::RXIC_OFFSET);

    pub const TXIC_OFFSET: u32 = 5; // Transmit interrupt clear
    pub const TXIC_MASK: Self = Self(1 << Self::TXIC_OFFSET);

    pub const RTIC_OFFSET: u32 = 6; // Receive timeout interrupt clear
    pub const RTIC_MASK: Self = Self(1 << Self::RTIC_OFFSET);

    pub const FEIC_OFFSET: u32 = 7; // Framing error interrupt clear
    pub const FEIC_MASK: Self = Self(1 << Self::FEIC_OFFSET);

    pub const PEIC_OFFSET: u32 = 8; // Parity error interrupt clear
    pub const PEIC_MASK: Self = Self(1 << Self::PEIC_OFFSET);

    pub const BEIC_OFFSET: u32 = 9; // Break error interrupt clear
    pub const BEIC_MASK: Self = Self(1 << Self::BEIC_OFFSET);

    pub const OEIC_OFFSET: u32 = 10; // Overrun error interrupt clear
    pub const OEIC_MASK: Self = Self(1 << Self::OEIC_OFFSET);

    pub const ALL_OFFSET: u32 = 0; // all interrupt clear
    pub const ALL_MASK: Self = Self(mask(11) << Self::ALL_OFFSET);
}

#[derive(Debug)]
pub struct Pl011Uart {
    registers: &'static Pl011Peripherals,
}

impl Pl011Uart {
    pub fn new(base_address: usize) -> Self {
        Self {
            registers: unsafe { &mut *(base_address as *mut Pl011Peripherals) },
        }
    }

    pub fn flush(&self) {
        while (self.registers.flags.read() & UARTFR::BUSY_MASK) != UARTFR(0) {
            core::hint::spin_loop();
        }
        while (self.registers.flags.read() & UARTFR::TXFE_MASK) != UARTFR(0) {
            core::hint::spin_loop();
        }
    }

    pub fn disabled(&self) {
        self.flush();
        // disable pl011
        self.registers
            .control
            .clear_bits(UARTCR::UARTEN_MASK + UARTCR::TXE_MASK + UARTCR::RXE_MASK);
        // clear all interrupt (write all bits 1)
        self.registers.interrupt_clear.write(UARTICR::ALL_MASK);
        // flush FIFO
        self.registers.line_control.clear_bits(UARTLCR::FEN_MASK);
        self.registers.interrupt_fifo_level_select.write(0);
        self.registers.interrupt_mask_set_clear.write(0);
    }

    pub fn init(&self, uart_clk: u32, baudrate: u32) {
        self.disabled();

        assert!(uart_clk > 368_6400); // UART_CLK > 3.6864MHz is required
        let div_x64 = ((4u64 * uart_clk as u64) + (baudrate as u64 / 2)) / baudrate as u64; // round(64*BAUDDIV)
        let divisor_i = (div_x64 / 64) as u32; // integer part(16bit)
        let divisor_f = (div_x64 % 64) as u32; // fractional part(6 bit)
        self.registers.integer_baud_rate.write(divisor_i);
        self.registers.fractional_baud_rate.write(divisor_f);
        // enable fifo
        self.registers
            .line_control
            .write(UARTLCR::WLEN_BIT8_MASK + UARTLCR::FEN_MASK);
        // turn on UART
        self.registers
            .control
            .set_bits(UARTCR::UARTEN_MASK + UARTCR::TXE_MASK + UARTCR::RXE_MASK);
    }

    fn pushb(&self, ch: u32) {
        while self.registers.flags.read() & UARTFR::TXFF_MASK != UARTFR(0) {
            core::hint::spin_loop();
        }
        self.registers.data.write(UARTDR(ch));
    }

    pub fn write(&self, char: &str) {
        for &i in char.as_bytes() {
            if i == b'\n' {
                self.pushb('\r' as u32);
            }
            self.pushb(i as u32);
        }
    }

    pub fn read_char(&self) -> u8 {
        while self.registers.flags.read() & UARTFR::RXFE_MASK != UARTFR(0) {
            core::hint::spin_loop();
        }
        let read = self.registers.data.read();
        if (read & (UARTDR::FE_MASK + UARTDR::PE_MASK + UARTDR::BE_MASK + UARTDR::OE_MASK))
            != UARTDR(0)
        {
            self.registers.error_status.write(0);
        }
        (read & UARTDR::DATA_MASK).0 as u8
    }
}

impl fmt::Write for Pl011Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write(s);
        Ok(())
    }
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        fmt::write(self, args)
    }
}

unsafe impl Send for Pl011Uart {}
