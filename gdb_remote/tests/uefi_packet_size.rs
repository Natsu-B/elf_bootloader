#![no_std]
#![no_main]

#[cfg(not(target_arch = "aarch64"))]
compile_error!("This test is intended to run on aarch64 targets only");

use aarch64_test::exit_failure;
use aarch64_test::exit_success;
use core::convert::Infallible;
use gdb_remote::GdbServer;
use gdb_remote::Target;
use gdb_remote::TargetCapabilities;
use gdb_remote::TargetError;

#[path = "support/rsp.rs"]
mod rsp;

use rsp::contains;
use rsp::drain_tx;
use rsp::encode_packet;
use rsp::feed_bytes;
use rsp::next_payload;

const RX_BUF: usize = 256;
const TX_BUF: usize = 1024;
const MAX_PKT: usize = 1024;
const TX_CAP: usize = 1024;

#[unsafe(no_mangle)]
extern "C" fn efi_main() -> ! {
    let mut server_default: GdbServer<MAX_PKT, TX_CAP> = GdbServer::new();
    let mut target = DummyTarget;

    let mut rx = [0u8; RX_BUF];
    let len = match encode_packet(&mut rx, b"qSupported") {
        Some(len) => len,
        None => exit_failure(),
    };
    if !feed_bytes(&mut server_default, &mut target, &rx[..len]) {
        exit_failure();
    }

    let mut tx = [0u8; TX_BUF];
    let tx_len = drain_tx(&mut server_default, &mut tx);
    let mut idx = 0usize;
    let mut payload = [0u8; 256];

    let Some(len) = next_payload(&tx[..tx_len], &mut idx, &mut payload) else {
        exit_failure();
    };
    if !contains(&payload[..len], b"PacketSize=400") {
        exit_failure();
    }
    if contains(&payload[..len], b"PacketSize=1024") {
        exit_failure();
    }

    let mut server_override: GdbServer<MAX_PKT, TX_CAP> = GdbServer::new_with_packet_size(256);
    let mut target = DummyTarget;
    let len = match encode_packet(&mut rx, b"qSupported") {
        Some(len) => len,
        None => exit_failure(),
    };
    if !feed_bytes(&mut server_override, &mut target, &rx[..len]) {
        exit_failure();
    }

    let tx_len = drain_tx(&mut server_override, &mut tx);
    let mut idx = 0usize;

    let Some(len) = next_payload(&tx[..tx_len], &mut idx, &mut payload) else {
        exit_failure();
    };
    if !contains(&payload[..len], b"PacketSize=100") {
        exit_failure();
    }
    if contains(&payload[..len], b"PacketSize=256") {
        exit_failure();
    }

    exit_success();
}

struct DummyTarget;

type DummyError = TargetError<Infallible, Infallible>;

impl Target for DummyTarget {
    type RecoverableError = Infallible;
    type UnrecoverableError = Infallible;

    fn capabilities(&self) -> TargetCapabilities {
        TargetCapabilities::empty()
    }

    fn read_registers(&mut self, _dst: &mut [u8]) -> Result<usize, DummyError> {
        Ok(0)
    }

    fn write_registers(&mut self, _src: &[u8]) -> Result<(), DummyError> {
        Ok(())
    }

    fn read_register(&mut self, _regno: u32, _dst: &mut [u8]) -> Result<usize, DummyError> {
        Ok(0)
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
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let _ = info;
    exit_failure();
}
