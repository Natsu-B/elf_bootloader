use gdb_remote::GdbServer;
use gdb_remote::ProcessResult;
use gdb_remote::Target;

const HEX: &[u8; 16] = b"0123456789abcdef";

pub(super) fn encode_packet(buf: &mut [u8], payload: &[u8]) -> Option<usize> {
    let len = payload.len();
    let packet_len = len.saturating_add(4);
    if packet_len > buf.len() {
        return None;
    }

    buf[0] = b'$';
    buf[1..len + 1].copy_from_slice(payload);
    buf[len + 1] = b'#';

    // RSP checksums wrap at eight bits.
    let sum = payload
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    buf[len + 2] = HEX[(sum >> 4) as usize];
    buf[len + 3] = HEX[(sum & 0xF) as usize];
    Some(packet_len)
}

pub(super) fn feed_bytes<T: Target, const MAX: usize, const TX: usize>(
    server: &mut GdbServer<MAX, TX>,
    target: &mut T,
    bytes: &[u8],
) -> bool {
    bytes
        .iter()
        .all(|&byte| matches!(server.on_rx_byte_irq(target, byte), Ok(ProcessResult::None)))
}

pub(super) fn drain_tx<const MAX: usize, const TX: usize>(
    server: &mut GdbServer<MAX, TX>,
    out: &mut [u8],
) -> usize {
    let mut idx = 0;
    while let Some(byte) = server.pop_tx_byte_irq() {
        if idx >= out.len() {
            break;
        }
        out[idx] = byte;
        idx += 1;
    }
    idx
}

pub(super) fn next_payload(tx: &[u8], idx: &mut usize, out: &mut [u8]) -> Option<usize> {
    // Skip ACK/NACK and transport noise before the next packet marker.
    *idx += tx.get(*idx..)?.iter().position(|&byte| byte == b'$')? + 1;
    let payload = tx.get(*idx..)?;
    let len = payload.iter().position(|&byte| byte == b'#')?;
    let copied = len.min(out.len());
    out[..copied].copy_from_slice(&payload[..copied]);
    *idx = (*idx + len + 3).min(tx.len());
    Some(copied)
}

#[cfg(target_arch = "aarch64")]
pub(super) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
