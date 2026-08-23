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

const TARGET_XML: &[u8] = b"<?xml version=\"1.0\"?><target version=\"1.0\"></target>";
const RX_BUF: usize = 512;
const TX_BUF: usize = 2048;
const PAYLOAD_CAP: usize = 256;
const MAX_PKT: usize = PAYLOAD_CAP;
const TX_CAP: usize = 1024;

#[unsafe(no_mangle)]
extern "C" fn efi_main() -> ! {
    let mut server: GdbServer<MAX_PKT, TX_CAP> = GdbServer::new();
    let mut target = DummyTarget;

    let mut rx = [0u8; RX_BUF];
    let len = match encode_packet(&mut rx, b"qSupported") {
        Some(len) => len,
        None => exit_failure(),
    };
    if !feed_bytes(&mut server, &mut target, &rx[..len]) {
        exit_failure();
    }

    let len = match encode_packet(&mut rx, b"qXfer:features:read:target.xml:0,400") {
        Some(len) => len,
        None => exit_failure(),
    };
    if !feed_bytes(&mut server, &mut target, &rx[..len]) {
        exit_failure();
    }

    let mut tx = [0u8; TX_BUF];
    let tx_len = drain_tx(&mut server, &mut tx);
    let mut idx = 0usize;
    let mut payload = [0u8; PAYLOAD_CAP];

    let Some(len) = next_payload(&tx[..tx_len], &mut idx, &mut payload) else {
        exit_failure();
    };
    if !contains(&payload[..len], b"qXfer:features:read+") {
        exit_failure();
    }

    let Some(len) = next_payload(&tx[..tx_len], &mut idx, &mut payload) else {
        exit_failure();
    };
    if len == 0 {
        exit_failure();
    }
    if payload[0] != b'm' && payload[0] != b'l' {
        exit_failure();
    }
    if !contains(&payload[..len], b"<target") {
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
        TargetCapabilities::SW_BREAK | TargetCapabilities::XFER_FEATURES
    }

    fn xfer_features(&mut self, annex: &str) -> Result<Option<&[u8]>, DummyError> {
        if annex == "target.xml" {
            return Ok(Some(TARGET_XML));
        }
        Ok(None)
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

    fn read_memory(&mut self, _addr: u64, _dst: &mut [u8]) -> Result<(), DummyError> {
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
