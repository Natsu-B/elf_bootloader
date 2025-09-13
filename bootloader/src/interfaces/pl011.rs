use core::fmt;

use tock_registers::interfaces::ReadWriteable;
use tock_registers::interfaces::Readable;
use tock_registers::interfaces::Writeable;
use tock_registers::register_bitfields;
use tock_registers::register_structs;
use tock_registers::registers::ReadOnly;
use tock_registers::registers::ReadWrite;
use tock_registers::registers::WriteOnly;

#[derive(PartialEq)]
pub enum UartNum {
    Debug,
    Rp1 { device_num: u8 },
}

// PL011 specification r1p5

register_structs! {
    pub Pl011Peripherals {
    (0x0000 => pub data: ReadWrite<u32, UARTDR::Register>),
    (0x0004 => pub error_status: ReadWrite<u32>),
    (0x0008 => _reserved0008),
    (0x0018 => pub flags: ReadOnly<u32, UARTFR::Register>),
    (0x001C => _reserved001c),
    (0x0020 => pub irdr_low_power_counter: ReadWrite<u32>),
    (0x0024 => pub integer_baud_rate: ReadWrite<u32>),
    (0x0028 => pub fractional_baud_rate: ReadWrite<u32>),
    (0x002C => pub line_control: ReadWrite<u32, UARTLCR::Register>),
    (0x0030 => pub control: ReadWrite<u32, UARTCR::Register>),
    (0x0034 => pub interrupt_fifo_level_select: ReadWrite<u32>),
    (0x0038 => pub interrput_mask_set_clear: ReadWrite<u32>),
    (0x003C => pub raw_interrput_status: ReadOnly<u32>),
    (0x0040 => pub masked_interrput_status: ReadOnly<u32>),
    (0x0044 => pub interrput_clear: WriteOnly<u32, UARTICR::Register>),
    (0x0048 => pub dma_control: ReadWrite<u32>),
    (0x004C => _reserved004c),
    (0x0FE0 => pub peripheral_id: [ReadOnly<u32>; 4]),
    (0x0FF0 => pub pcell_id:[ReadOnly<u32>; 4]),
    (0x1000 => @END),
    }
}

register_bitfields![
    u32,
    pub UARTDR [
        DATA OFFSET(0) NUMBITS(8) [],
        FE OFFSET(8) NUMBITS(1) [], // framing error
        PE OFFSET(9) NUMBITS(1) [], // priority error
        BE OFFSET(10) NUMBITS(1) [], // break error
        OE OFFSET(11) NUMBITS(1) [], // overrun error
    ],
    pub UARTFR [
        CTS OFFSET(0) NUMBITS(1) [], // clear to send
        DSR OFFSET(1) NUMBITS(1) [], // data set ready
        DCD OFFSET(2) NUMBITS(1) [], //data carrier detect
        BUSY OFFSET(3) NUMBITS(1) [], // UART busy
        RXFE OFFSET(4) NUMBITS(1) [], // receive FIFO empty
        TXFF OFFSET(5) NUMBITS(1) [], // transmit FIFO full
        RXFF OFFSET(6) NUMBITS(1) [], // receive FIFO full
        TXFE OFFSET(7) NUMBITS(1) [], // transmit FIFO empty
        RI OFFSET(8) NUMBITS(1) [], // ring indicator
    ],
    pub UARTLCR [
        BRK OFFSET(0) NUMBITS(1) [], // send break for normal use this bit must be cleared to 0
        PEN OFFSET(1) NUMBITS(1) [
            DISABLE = 0b0,
            ENABLE = 0b1,
        ], // parity enable
        EPS OFFSET(2) NUMBITS(1) [
            ODD =0b0, // odd parity
            EVEN = 0b1, // even parity
        ],
        STP2 OFFSET(3) NUMBITS(1) [], // two stop bits select
        FEN OFFSET(4) NUMBITS(1) [], // enable FIFO if disabled(0), FIFO become 1 byte register
        WLEN OFFSET(5) NUMBITS(2) [
            BIT8 = 0b11,
            BIT7 = 0b10,
            BIT6 = 0b01,
            BIT5 = 0b00,
        ], // word length
        SPS OFFSET(7) NUMBITS(1) [], // enable stack parity
    ],
    pub UARTCR [
        UARTEN OFFSET(0) NUMBITS(1) [], // enable UART
        SIREN OFFSET(1) NUMBITS(1) [], // enable SIR
        SIRLP OFFSET(2) NUMBITS(1) [], // enable SIR low power IrDA mode
        LBE OFFSET(7) NUMBITS(1) [], // enable loop back
        TXE OFFSET(8) NUMBITS(1) [], // enable transmit
        RXE OFFSET(9) NUMBITS(1) [], // enable receive
        DTR OFFSET(10) NUMBITS(1) [], // data transmit ready
        LTS OFFSET(11) NUMBITS(1) [], // request to send
        OUT1 OFFSET(12) NUMBITS(1) [],
        OUT2 OFFSET(13) NUMBITS(1) [],
        RTSEn OFFSET(14) NUMBITS(1) [], // enable RTS hardware flow control
        CTSEn OFFSET(15) NUMBITS(1) [], // enable CTS hardware flow control
    ],
    pub UARTICR [
        RIMIC OFFSET(0) NUMBITS(1) [], // nUARTRI modem interrupt clear
        CTSMIC OFFSET(1) NUMBITS(1) [], // nUARTCTS modem interrupt clear
        DCDMIC OFFSET(2) NUMBITS(1) [], // nUARTDCD modem interrupt clear
        DSRMIC OFFSET(3) NUMBITS(1) [], // nUARTDSR modem interrupt clear
        RXIC OFFSET(4) NUMBITS(1) [], // Receive interrupt clear
        TXIC OFFSET(5) NUMBITS(1) [], // Transmit interrupt clear
        RTIC OFFSET(6) NUMBITS(1) [], // Receive timeout interrupt clear
        FEIC OFFSET(7) NUMBITS(1) [], // Framing error interrupt clear
        PEIC OFFSET(8) NUMBITS(1) [], // Parity error interrupt clear
        BEIC OFFSET(9) NUMBITS(1) [], // Break error interrupt clear
        OEIC OFFSET(10) NUMBITS(1) [], // Overrun error interrupt clear
        ALL OFFSET(0) NUMBITS(11) [], // all interrupt clear
    ],
];

pub struct Pl011Uart {
    registers: &'static mut Pl011Peripherals,
}

impl Pl011Uart {
    pub fn new(base_address: *const u32) -> Self {
        Self {
            registers: unsafe { &mut *(base_address as *mut Pl011Peripherals) },
        }
    }

    fn flush(&self) {
        while self.registers.flags.matches_all(UARTFR::BUSY::SET) {
            core::hint::spin_loop();
        }
    }

    fn disabled(&self) {
        // disable PL011
        self.registers.control.modify(UARTCR::UARTEN::CLEAR);
        // clear all interrupt (write all bits 1)
        self.registers.interrput_clear.write(UARTICR::ALL::SET);
        // flash FIFO
        self.registers.line_control.modify(UARTLCR::FEN::CLEAR);
    }

    pub fn init(&self, uart_kind: UartNum, baudrate: u32) {
        self.flush();
        self.disabled();

        // calculate clock divisor
        let uart_clk = if uart_kind == UartNum::Debug {
            4400_0000
        } else {
            4800_0000
        };
        assert!(uart_clk > 368_6400); // UART_CLK > 3.6864MHz is required
        let divisor_i = uart_clk / baudrate / 16; // integer part(16bit)
        let divisor_f = (uart_clk * 8 / baudrate).div_ceil(2) & 0b11_1111; // fractional part(6 bit)
        self.registers.integer_baud_rate.set(divisor_i);
        self.registers.fractional_baud_rate.set(divisor_f);
        // enable fifo
        self.registers
            .line_control
            .write(UARTLCR::WLEN::BIT8 + UARTLCR::FEN::SET);
        // turn on UART
        self.registers
            .control
            .write(UARTCR::UARTEN::SET + UARTCR::TXE::SET + UARTCR::RXE::SET);
    }

    pub fn write(&self, char: &str) {
        for i in char.chars() {
            if i == '\n' {
                while self.registers.flags.is_set(UARTFR::TXFF) {
                    core::hint::spin_loop();
                }
                self.registers.data.set('\r' as u32);
            }
            while self.registers.flags.is_set(UARTFR::TXFF) {
                core::hint::spin_loop();
            }
            self.registers.data.set(i as u32);
        }
    }

    pub fn read_char(&self) -> u8 {
        while self.registers.flags.is_set(UARTFR::RXFE) {
            core::hint::spin_loop();
        }
        self.registers.data.get() as u8
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
