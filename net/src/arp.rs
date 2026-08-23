//! ARP protocol parsing and encoding.

use io_api::ethernet::MacAddr;

use super::EncodeError;
use super::Ipv4Addr;
use super::ParseError;
use super::eth;
use super::read_be_u16;
use super::write_be_u16;

/// ARP payload size for Ethernet/IPv4.
pub const ARP_PAYLOAD_LEN: usize = 28;
const ARP_HWTYPE_ETHERNET: u16 = 1;
const ARP_OPERATION_REQUEST: u16 = 1;
const ARP_OPERATION_REPLY: u16 = 2;
const ARP_HLEN_ETHERNET: u8 = 6;
const ARP_PLEN_IPV4: u8 = 4;

/// A parsed ARP request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArpRequestView {
    /// Source MAC from Ethernet header.
    pub src_mac: MacAddr,
    /// Destination MAC from Ethernet header.
    pub dst_mac: MacAddr,
    /// Sender hardware address.
    pub sender_mac: MacAddr,
    /// Sender protocol address.
    pub sender_ip: Ipv4Addr,
    /// Target hardware address.
    pub target_mac: MacAddr,
    /// Target protocol address.
    pub target_ip: Ipv4Addr,
}

fn parse_arp_frame(frame: &[u8]) -> Result<(eth::EthernetHeader, &[u8]), ParseError> {
    let (header, payload) = eth::parse_ethernet_ii(frame)?;
    if header.ethertype != eth::ETHERTYPE_ARP {
        return Err(ParseError::UnsupportedEthertype);
    }
    if payload.len() < ARP_PAYLOAD_LEN {
        return Err(ParseError::ArpPacketTooShort);
    }
    if read_be_u16(payload, 0) != ARP_HWTYPE_ETHERNET
        || read_be_u16(payload, 2) != eth::ETHERTYPE_IPV4
        || payload[4] != ARP_HLEN_ETHERNET
        || payload[5] != ARP_PLEN_IPV4
    {
        return Err(ParseError::ArpUnsupportedFormat);
    }
    Ok((header, payload))
}

fn arp_mac(payload: &[u8], offset: usize) -> MacAddr {
    // ARP payload length is validated before fixed-width address fields are read.
    MacAddr(payload[offset..offset + 6].try_into().unwrap())
}

/// Parses an ARP request frame for Ethernet/IPv4 (`htype=1`, `ptype=0x0800`).
pub fn parse_arp_request(frame: &[u8]) -> Result<ArpRequestView, ParseError> {
    let (eth_header, payload) = parse_arp_frame(frame)?;
    let operation = read_be_u16(payload, 6);
    if operation != ARP_OPERATION_REQUEST {
        return Err(ParseError::ArpNotRequest);
    }

    let sender_mac = arp_mac(payload, 8);
    let sender_ip = [payload[14], payload[15], payload[16], payload[17]];
    let target_mac = arp_mac(payload, 18);
    let target_ip = [payload[24], payload[25], payload[26], payload[27]];

    Ok(ArpRequestView {
        src_mac: eth_header.src_mac,
        dst_mac: eth_header.dst_mac,
        sender_mac,
        sender_ip,
        target_mac,
        target_ip,
    })
}

/// Encodes an Ethernet/IPv4 ARP reply frame into `buf`.
///
/// Returns the final frame length on success.
pub fn encode_arp_reply(
    buf: &mut [u8],
    local_mac: MacAddr,
    local_ip: Ipv4Addr,
    peer_mac: MacAddr,
    peer_ip: Ipv4Addr,
) -> Result<usize, EncodeError> {
    let frame_len = eth::HEADER_LEN + ARP_PAYLOAD_LEN;
    if buf.len() < frame_len {
        return Err(EncodeError::BufferTooShort);
    }

    let _ = eth::encode_ethernet_ii(buf, peer_mac, local_mac, eth::ETHERTYPE_ARP)?;
    let payload = &mut buf[eth::HEADER_LEN..frame_len];

    write_be_u16(payload, 0, ARP_HWTYPE_ETHERNET);
    write_be_u16(payload, 2, eth::ETHERTYPE_IPV4);
    payload[4] = ARP_HLEN_ETHERNET;
    payload[5] = ARP_PLEN_IPV4;
    write_be_u16(payload, 6, ARP_OPERATION_REPLY);
    payload[8..14].copy_from_slice(&local_mac.0);
    payload[14..18].copy_from_slice(&local_ip);
    payload[18..24].copy_from_slice(&peer_mac.0);
    payload[24..28].copy_from_slice(&peer_ip);
    Ok(frame_len)
}

/// Encodes an Ethernet/IPv4 ARP request frame into `buf`.
///
/// The Ethernet and ARP target hardware addresses are both broadcast, as
/// required for a request before the peer MAC address is known.
pub fn encode_arp_request(
    buf: &mut [u8],
    local_mac: MacAddr,
    local_ip: Ipv4Addr,
    target_ip: Ipv4Addr,
) -> Result<usize, EncodeError> {
    let frame_len = eth::HEADER_LEN + ARP_PAYLOAD_LEN;
    if buf.len() < frame_len {
        return Err(EncodeError::BufferTooShort);
    }

    let broadcast = MacAddr([0xff; 6]);
    let _ = eth::encode_ethernet_ii(buf, broadcast, local_mac, eth::ETHERTYPE_ARP)?;
    let payload = &mut buf[eth::HEADER_LEN..frame_len];
    write_be_u16(payload, 0, ARP_HWTYPE_ETHERNET);
    write_be_u16(payload, 2, eth::ETHERTYPE_IPV4);
    payload[4] = ARP_HLEN_ETHERNET;
    payload[5] = ARP_PLEN_IPV4;
    write_be_u16(payload, 6, ARP_OPERATION_REQUEST);
    payload[8..14].copy_from_slice(&local_mac.0);
    payload[14..18].copy_from_slice(&local_ip);
    payload[18..24].copy_from_slice(&[0; 6]);
    payload[24..28].copy_from_slice(&target_ip);
    Ok(frame_len)
}

/// Parses an Ethernet/IPv4 ARP reply for `local_ip` from `requested_ip`.
///
/// The returned MAC is safe to use as a unicast destination: zero and broadcast
/// sender addresses are rejected, and the Ethernet source must agree with the
/// sender hardware address in the ARP payload.
pub fn parse_arp_reply(
    frame: &[u8],
    local_ip: Ipv4Addr,
    requested_ip: Ipv4Addr,
) -> Result<MacAddr, ParseError> {
    let (eth_header, payload) = parse_arp_frame(frame)?;
    if read_be_u16(payload, 6) != ARP_OPERATION_REPLY {
        return Err(ParseError::ArpNotReply);
    }

    let sender_mac = arp_mac(payload, 8);
    let sender_ip = [payload[14], payload[15], payload[16], payload[17]];
    let target_ip = [payload[24], payload[25], payload[26], payload[27]];
    if sender_ip != requested_ip || target_ip != local_ip {
        return Err(ParseError::ArpUnsupportedFormat);
    }
    if sender_mac.0 == [0; 6] || sender_mac.0 == [0xff; 6] {
        return Err(ParseError::ArpInvalidSenderMac);
    }
    if eth_header.src_mac != sender_mac {
        return Err(ParseError::ArpSenderMacMismatch);
    }
    Ok(sender_mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL_MAC: MacAddr = MacAddr([0x02, 0, 0, 0, 0, 1]);
    const SERVER_MAC: MacAddr = MacAddr([0x02, 0, 0, 0, 0, 2]);
    const LOCAL_IP: Ipv4Addr = [192, 0, 2, 10];
    const SERVER_IP: Ipv4Addr = [192, 0, 2, 1];

    #[test]
    fn arp_request_encoding() {
        let mut frame = [0u8; eth::HEADER_LEN + ARP_PAYLOAD_LEN];
        let len = encode_arp_request(&mut frame, LOCAL_MAC, LOCAL_IP, SERVER_IP).unwrap();
        assert_eq!(len, frame.len());
        assert_eq!(&frame[..6], &[0xff; 6]);
        assert_eq!(
            read_be_u16(&frame, eth::HEADER_LEN + 6),
            ARP_OPERATION_REQUEST
        );
        assert_eq!(
            &frame[eth::HEADER_LEN + 14..eth::HEADER_LEN + 18],
            &LOCAL_IP
        );
        assert_eq!(
            &frame[eth::HEADER_LEN + 24..eth::HEADER_LEN + 28],
            &SERVER_IP
        );
        assert_eq!(
            parse_arp_request(&frame),
            Ok(ArpRequestView {
                src_mac: LOCAL_MAC,
                dst_mac: MacAddr([0xff; 6]),
                sender_mac: LOCAL_MAC,
                sender_ip: LOCAL_IP,
                target_mac: MacAddr([0; 6]),
                target_ip: SERVER_IP,
            })
        );
    }

    #[test]
    fn arp_reply_parses_expected_peer() {
        let mut frame = [0u8; eth::HEADER_LEN + ARP_PAYLOAD_LEN];
        encode_arp_reply(&mut frame, SERVER_MAC, SERVER_IP, LOCAL_MAC, LOCAL_IP).unwrap();
        assert_eq!(parse_arp_reply(&frame, LOCAL_IP, SERVER_IP), Ok(SERVER_MAC));
    }

    #[test]
    fn arp_reply_rejects_wrong_sender_ip() {
        let mut frame = [0u8; eth::HEADER_LEN + ARP_PAYLOAD_LEN];
        encode_arp_reply(&mut frame, SERVER_MAC, [192, 0, 2, 99], LOCAL_MAC, LOCAL_IP).unwrap();
        assert_eq!(
            parse_arp_reply(&frame, LOCAL_IP, SERVER_IP),
            Err(ParseError::ArpUnsupportedFormat)
        );
    }

    #[test]
    fn arp_reply_rejects_wrong_target_ip() {
        let mut frame = [0u8; eth::HEADER_LEN + ARP_PAYLOAD_LEN];
        encode_arp_reply(
            &mut frame,
            SERVER_MAC,
            SERVER_IP,
            LOCAL_MAC,
            [192, 0, 2, 99],
        )
        .unwrap();
        assert_eq!(
            parse_arp_reply(&frame, LOCAL_IP, SERVER_IP),
            Err(ParseError::ArpUnsupportedFormat)
        );
    }
}
