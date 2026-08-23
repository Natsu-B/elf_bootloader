//! Host and bare-metal coverage for the public ARP encoding and parsing boundary.

#![cfg_attr(target_arch = "aarch64", no_std)]
#![cfg_attr(target_arch = "aarch64", no_main)]
#![cfg_attr(target_arch = "aarch64", feature(custom_test_frameworks))]
#![cfg_attr(target_arch = "aarch64", test_runner(aarch64_unit_test::test_runner))]
#![cfg_attr(target_arch = "aarch64", reexport_test_harness_main = "test_main")]

use io_api::ethernet::MacAddr;
use net::ArpRequestView;

#[cfg(target_arch = "aarch64")]
aarch64_unit_test::uboot_unit_test_harness!(aarch64_unit_test::init_default_uart);

const LOCAL_MAC: MacAddr = MacAddr([0x02, 0, 0, 0, 0, 1]);
const PEER_MAC: MacAddr = MacAddr([0x02, 0, 0, 0, 0, 2]);
const LOCAL_IP: [u8; 4] = [192, 0, 2, 10];
const PEER_IP: [u8; 4] = [192, 0, 2, 1];

#[cfg_attr(target_arch = "aarch64", test_case)]
#[cfg_attr(not(target_arch = "aarch64"), test)]
fn arp_roundtrip() {
    let mut frame = [0; net::eth::HEADER_LEN + net::arp::ARP_PAYLOAD_LEN];
    net::encode_arp_request(&mut frame, LOCAL_MAC, LOCAL_IP, PEER_IP).unwrap();
    assert_eq!(
        net::parse_arp_request(&frame),
        Ok(ArpRequestView {
            src_mac: LOCAL_MAC,
            dst_mac: MacAddr([0xff; 6]),
            sender_mac: LOCAL_MAC,
            sender_ip: LOCAL_IP,
            target_mac: MacAddr([0; 6]),
            target_ip: PEER_IP,
        })
    );

    net::encode_arp_reply(&mut frame, PEER_MAC, PEER_IP, LOCAL_MAC, LOCAL_IP).unwrap();
    assert_eq!(
        net::parse_arp_reply(&frame, LOCAL_IP, PEER_IP),
        Ok(PEER_MAC)
    );
}
