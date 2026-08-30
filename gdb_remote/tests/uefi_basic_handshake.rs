#![no_std]
#![no_main]

#[cfg(not(target_arch = "aarch64"))]
compile_error!("This test is intended to run on aarch64 targets only");

use aarch64_test::exit_failure;
use aarch64_test::exit_success;
use aarch64_test::semihost_write0;
use core::convert::Infallible;
use core::mem::MaybeUninit;
use gdb_remote::GdbServer;
use gdb_remote::Target;
use gdb_remote::TargetCapabilities;
use gdb_remote::TargetError;
use print::pl011::Pl011Uart;
use print::stream::Pl011Stream;

// Use the DT-selected console PL011: recent EDK2 maps only this UART for UEFI apps.
// The runner waits for READY before attaching GDB, after firmware console output is complete.
const UART_BASE: usize = 0x900_0000;
// QEMU virt PL011 UARTs run at 24MHz.
const UART_CLOCK_HZ: u32 = 24 * 1_000_000;

// AArch64 core regset (x0-x30, sp, pc, cpsr).
const CORE_REG_BYTES: usize = 31 * 8 + 8 + 8 + 4;

#[unsafe(no_mangle)]
extern "C" fn efi_main() -> ! {
    let mut uart = Pl011Uart::new(UART_BASE, UART_CLOCK_HZ as u64);
    uart.init(115200);
    uart.drain_rx();
    let mut server = MaybeUninit::<GdbServer<2048, 4096>>::uninit();
    GdbServer::init_in_place(&mut server);
    // SAFETY: init_in_place initialized every field, and this stack slot remains
    // alive and exclusively borrowed until the GDB session finishes.
    let server = unsafe { server.assume_init_mut() };
    let mut target = DummyTarget;
    let mut stream = Pl011Stream::new(&uart);

    semihost_write0(b"GDB_REMOTE_READY\n\0".as_ptr());

    if server
        .run_until_monitor_exit(&mut stream, &mut target)
        .is_err()
    {
        exit_failure();
    }

    exit_success();
}

struct DummyTarget;

type DummyError = TargetError<Infallible, Infallible>;

const DUMMY_REG: u64 = 0x0000_0000_4000_0000;

impl Target for DummyTarget {
    type RecoverableError = Infallible;
    type UnrecoverableError = Infallible;

    fn capabilities(&self) -> TargetCapabilities {
        TargetCapabilities::SW_BREAK | TargetCapabilities::VCONT
    }

    fn read_registers(&mut self, dst: &mut [u8]) -> Result<usize, DummyError> {
        if dst.len() < CORE_REG_BYTES {
            return Err(TargetError::NotSupported);
        }
        // Fill a minimal AArch64 core regset:
        // x0..x30, sp, pc are 64-bit; cpsr is 32-bit.
        let r64 = DUMMY_REG.to_le_bytes();
        let mut off = 0usize;
        // 33 x 64-bit regs: x0..x30 (31) + sp (1) + pc (1)
        for _ in 0..=32 {
            dst[off..off + 8].copy_from_slice(&r64);
            off += 8;
        }
        // cpsr (32-bit)
        dst[off..off + 4].copy_from_slice(&(DUMMY_REG as u32).to_le_bytes());
        Ok(CORE_REG_BYTES)
    }

    fn write_registers(&mut self, _src: &[u8]) -> Result<(), DummyError> {
        Ok(())
    }

    fn read_register(&mut self, _regno: u32, dst: &mut [u8]) -> Result<usize, DummyError> {
        // Match the same minimal AArch64 core regset:
        // 0..=30: x0..x30 (8 bytes)
        // 31: sp (8 bytes)
        // 32: pc (8 bytes)
        // 33: cpsr (4 bytes)
        let need = match _regno {
            0..=32 => 8,
            33 => 4,
            _ => return Err(TargetError::NotSupported),
        };
        if dst.len() < need {
            return Err(TargetError::NotSupported);
        }
        let r64 = DUMMY_REG.to_le_bytes();
        dst[..need].copy_from_slice(&r64[..need]);
        Ok(need)
    }

    fn write_register(&mut self, _regno: u32, _src: &[u8]) -> Result<(), DummyError> {
        Ok(())
    }

    fn read_memory(&mut self, _addr: u64, dst: &mut [u8]) -> Result<(), DummyError> {
        dst.fill(0);
        Ok(())
    }

    fn write_memory(&mut self, _addr: u64, _src: &[u8]) -> Result<(), DummyError> {
        Ok(())
    }

    fn insert_sw_breakpoint(&mut self, _addr: u64) -> Result<(), DummyError> {
        Ok(())
    }

    fn remove_sw_breakpoint(&mut self, _addr: u64) -> Result<(), DummyError> {
        Ok(())
    }

    fn insert_hw_breakpoint(&mut self, _addr: u64, _kind: u64) -> Result<(), DummyError> {
        Err(TargetError::NotSupported)
    }

    fn remove_hw_breakpoint(&mut self, _addr: u64, _kind: u64) -> Result<(), DummyError> {
        Err(TargetError::NotSupported)
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let _ = info;
    exit_failure();
}
