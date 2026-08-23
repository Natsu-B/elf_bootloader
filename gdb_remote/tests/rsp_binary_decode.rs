use core::convert::Infallible;
use gdb_remote::GdbServer;
use gdb_remote::Target;
use gdb_remote::TargetCapabilities;
use gdb_remote::TargetError;
use gdb_remote::decode_rsp_binary;

#[path = "support/rsp.rs"]
mod rsp;

use rsp::drain_tx;
use rsp::encode_packet;
use rsp::feed_bytes;
use rsp::next_payload;

#[test]
fn rsp_binary_decodes_escapes() {
    // '}' escape + XOR 0x20 yields '#', '$', and '}'.
    let encoded = [b'A', b'}', 0x03, b'}', 0x04, b'}', 0x5d, b'Z'];
    let mut out = [0u8; 8];
    let len = decode_rsp_binary(&encoded, &mut out).expect("decode should succeed");
    assert_eq!(&out[..len], b"A#$}Z");
}

#[test]
fn rsp_binary_treats_star_as_data() {
    let encoded = [b'0', b'*', b' '];
    let mut out = [0u8; 8];
    let len = decode_rsp_binary(&encoded, &mut out).expect("decode should succeed");
    assert_eq!(&out[..len], b"0* ");
}

#[test]
fn rsp_binary_rejects_invalid_sequences() {
    let mut out = [0u8; 8];
    assert!(decode_rsp_binary(b"}", &mut out).is_err());
}

#[test]
fn rsp_binary_len_mismatch_returns_error() {
    let mut server: GdbServer<64, 128> = GdbServer::new();
    let mut target = DummyTarget::new();

    let payload = b"X0,4:abc";
    let mut packet = [0u8; 64];
    let packet_len = encode_packet(&mut packet, payload).expect("packet buffer should fit");
    assert!(feed_bytes(&mut server, &mut target, &packet[..packet_len]));

    let mut tx = [0u8; 128];
    let tx_len = drain_tx(&mut server, &mut tx);
    let mut payload_buf = [0u8; 16];
    let Some(reply_len) = next_payload(&tx[..tx_len], &mut 0, &mut payload_buf) else {
        panic!("missing reply payload");
    };

    assert_eq!(&payload_buf[..reply_len], b"E03");
    assert_eq!(target.write_calls, 0);
}

#[test]
fn rsp_binary_x_write_ok_and_idempotent() {
    let mut server: GdbServer<64, 128> = GdbServer::new();
    let mut target = DummyTarget::new();

    let encoded = [b'}', 0x03, b'}', 0x04, b'}', 0x5d, b'*'];
    let mut payload = [0u8; 32];
    let mut idx = 0usize;
    payload[idx..idx + 5].copy_from_slice(b"X0,4:");
    idx += 5;
    payload[idx..idx + encoded.len()].copy_from_slice(&encoded);
    idx += encoded.len();

    let mut packet = [0u8; 64];
    let packet_len = encode_packet(&mut packet, &payload[..idx]).expect("packet buffer should fit");
    assert!(feed_bytes(&mut server, &mut target, &packet[..packet_len]));

    assert_eq!(target.write_calls, 1);
    assert_eq!(target.last_addr, 0);
    assert_eq!(target.last_len, 4);
    assert_eq!(&target.last_data[..target.last_len], b"#$}*");

    let mut tx = [0u8; 128];
    let tx_len = drain_tx(&mut server, &mut tx);
    let mut payload_buf = [0u8; 16];
    let Some(reply_len) = next_payload(&tx[..tx_len], &mut 0, &mut payload_buf) else {
        panic!("missing reply payload");
    };
    assert_eq!(&payload_buf[..reply_len], b"OK");

    assert!(feed_bytes(&mut server, &mut target, &packet[..packet_len]));
    assert_eq!(target.write_calls, 2);
    assert_eq!(&target.last_data[..target.last_len], b"#$}*");
}

struct DummyTarget {
    write_calls: usize,
    last_addr: u64,
    last_len: usize,
    last_data: [u8; 16],
}

impl DummyTarget {
    fn new() -> Self {
        Self {
            write_calls: 0,
            last_addr: 0,
            last_len: 0,
            last_data: [0u8; 16],
        }
    }
}

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

    fn write_memory(&mut self, addr: u64, src: &[u8]) -> Result<(), DummyError> {
        self.write_calls += 1;
        self.last_addr = addr;
        let to_copy = self.last_data.len().min(src.len());
        self.last_data[..to_copy].copy_from_slice(&src[..to_copy]);
        self.last_len = to_copy;
        Ok(())
    }

    fn insert_sw_breakpoint(&mut self, _addr: u64) -> Result<(), DummyError> {
        Ok(())
    }

    fn remove_sw_breakpoint(&mut self, _addr: u64) -> Result<(), DummyError> {
        Ok(())
    }
}
