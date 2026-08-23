use arch_hal::aarch64_mutex::RawSpinLockIrqSave;
use core::mem::MaybeUninit;
use core::sync::atomic::Ordering;
use io_api::ethernet::EthernetFrameIo;
use io_api::ethernet::MacAddr;
use mutex::pod::RawAtomicPod;
use net_proto::Ipv4Addr;

const GDB_DST_PORT: u16 = 10000;
const DEBUG_DST_PORT: u16 = 10001;
const RX_FRAME_BUF_SIZE: usize = 2048;
const TX_FRAME_BUF_SIZE: usize = 2048;
const UDP_TX_PAYLOAD_CAP: usize = 1200;
const UDP_HEADER_OVERHEAD: usize = net_proto::eth::HEADER_LEN
    + net_proto::ipv4::HEADER_LEN_NO_OPTIONS
    + net_proto::udp::HEADER_LEN;

#[derive(Clone, Copy, Debug)]
struct PeerEndpoint {
    peer_mac: MacAddr,
    peer_ip: Ipv4Addr,
    peer_src_port: u16,
}

struct UdpUartState {
    eth: *mut dyn EthernetFrameIo,
    local_mac: MacAddr,
    local_ip: Ipv4Addr,
    rx_frame: [u8; RX_FRAME_BUF_SIZE],
    tx_frame: [u8; TX_FRAME_BUF_SIZE],
    gdb_peer: Option<PeerEndpoint>,
    debug_peer: Option<PeerEndpoint>,
    gdb_tx_payload: [u8; UDP_TX_PAYLOAD_CAP],
    gdb_tx_len: usize,
    debug_tx_payload: [u8; UDP_TX_PAYLOAD_CAP],
    debug_tx_len: usize,
}

// SAFETY: `UdpUartState` is only accessed under `UDP_UART_STATE` IRQ-save lock,
// so sending it across threads does not introduce concurrent aliasing.
unsafe impl Send for UdpUartState {}

// SAFETY: published with Release in `init` after full state initialization.
static UDP_UART_READY: RawAtomicPod<bool> = unsafe { RawAtomicPod::new_raw_unchecked(false) };
// SAFETY: updated with Release ordering from quiesce/resume control paths.
static UDP_UART_PAUSED: RawAtomicPod<bool> = unsafe { RawAtomicPod::new_raw_unchecked(false) };
// SAFETY: initialized once in `init`; all accesses are serialized by IRQ-save lock.
static UDP_UART_STATE: RawSpinLockIrqSave<MaybeUninit<UdpUartState>> =
    RawSpinLockIrqSave::new(MaybeUninit::uninit());

/// Initializes the global UDP UART multiplexer.
///
/// This must be called once before any poll/TX API is used.
pub fn init(eth: &'static mut dyn EthernetFrameIo, local_ip: Ipv4Addr) {
    if UDP_UART_READY.load(Ordering::Acquire) {
        return;
    }
    let mut guard = UDP_UART_STATE.lock_irqsave();
    if UDP_UART_READY.load(Ordering::Acquire) {
        return;
    }

    let state = UdpUartState {
        eth: eth as *mut dyn EthernetFrameIo,
        local_mac: eth.mac_addr(),
        local_ip,
        rx_frame: [0; RX_FRAME_BUF_SIZE],
        tx_frame: [0; TX_FRAME_BUF_SIZE],
        gdb_peer: None,
        debug_peer: None,
        gdb_tx_payload: [0; UDP_TX_PAYLOAD_CAP],
        gdb_tx_len: 0,
        debug_tx_payload: [0; UDP_TX_PAYLOAD_CAP],
        debug_tx_len: 0,
    };

    (&mut *guard).write(state);
    UDP_UART_READY.store(true, Ordering::Release);
}

pub fn pause() {
    UDP_UART_PAUSED.store(true, Ordering::Release);
}

pub fn resume() {
    UDP_UART_PAUSED.store(false, Ordering::Release);
}

/// Polls Ethernet RX, handles ARP, demultiplexes UDP streams, and invokes byte callbacks.
pub fn poll(mut on_gdb_byte: impl FnMut(u8), mut on_dbg_byte: impl FnMut(u8)) {
    let _ = with_state(|state| {
        loop {
            let recv_cap = max_rx_frame_len(state);
            if recv_cap == 0 {
                break;
            }
            let received = {
                let eth = state.eth;
                let recv_buf = &mut state.rx_frame[..recv_cap];
                // SAFETY: `eth` comes from `init` and access is serialized by the lock held here.
                unsafe { (&mut *eth).try_recv_frame(recv_buf) }
            };
            let Some(frame_len) = received else { break };
            if frame_len == 0 || frame_len > recv_cap {
                continue;
            }

            let mut arp_reply_peer = None;
            let mut learned_gdb = None;
            let mut learned_dbg = None;
            {
                let frame = &state.rx_frame[..frame_len];
                if let Ok(arp_req) = net_proto::parse_arp_request(frame) {
                    if arp_req.target_ip == state.local_ip {
                        arp_reply_peer = Some((arp_req.sender_mac, arp_req.sender_ip));
                    }
                }
                if let Ok(datagram) = net_proto::parse_udp_ipv4_frame(frame) {
                    if datagram.dst_ip == state.local_ip {
                        let learned = PeerEndpoint {
                            peer_mac: datagram.src_mac,
                            peer_ip: datagram.src_ip,
                            peer_src_port: datagram.src_port,
                        };
                        if datagram.dst_port == GDB_DST_PORT {
                            learned_gdb = Some(learned);
                            for &byte in datagram.payload {
                                on_gdb_byte(byte);
                            }
                        } else if datagram.dst_port == DEBUG_DST_PORT {
                            learned_dbg = Some(learned);
                            for &byte in datagram.payload {
                                on_dbg_byte(byte);
                            }
                        }
                    }
                }
            }

            if let Some(peer) = learned_gdb {
                state.gdb_peer = Some(peer);
            }
            if let Some(peer) = learned_dbg {
                state.debug_peer = Some(peer);
            }
            if let Some((peer_mac, peer_ip)) = arp_reply_peer {
                let _ = send_arp_reply(state, peer_mac, peer_ip);
            }
        }
    });
}

/// Queues one byte for GDB UDP TX and flushes as needed.
///
/// Returns `false` when no peer is learned or TX cannot proceed.
pub fn gdb_try_write_byte(byte: u8) -> bool {
    with_state(|state| {
        if state.gdb_peer.is_none() {
            return false;
        }
        let limit = max_udp_payload_len(state).min(state.gdb_tx_payload.len());
        if limit == 0 {
            return false;
        }
        if state.gdb_tx_len >= limit {
            if !flush_gdb_locked(state) {
                return false;
            }
        }
        if state.gdb_tx_len >= limit {
            return false;
        }
        state.gdb_tx_payload[state.gdb_tx_len] = byte;
        state.gdb_tx_len += 1;
        if state.gdb_tx_len >= limit {
            let _ = flush_gdb_locked(state);
        }
        true
    })
    .unwrap_or(false)
}

/// Flushes pending GDB UDP TX payload.
pub fn gdb_flush() {
    let _ = with_state(flush_gdb_locked);
}

/// Best-effort debug/log mirror output over UDP.
pub fn debug_write_str(s: &str) {
    let _ = with_state(|state| {
        if state.debug_peer.is_none() {
            return;
        }
        let limit = max_udp_payload_len(state).min(state.debug_tx_payload.len());
        if limit == 0 {
            state.debug_tx_len = 0;
            return;
        }
        for &byte in s.as_bytes() {
            if state.debug_tx_len >= limit {
                flush_debug_locked(state);
            }
            if state.debug_tx_len >= limit {
                state.debug_tx_len = 0;
                break;
            }
            state.debug_tx_payload[state.debug_tx_len] = byte;
            state.debug_tx_len += 1;
        }
    });
}

/// Flushes pending debug/log UDP payload.
pub fn debug_flush() {
    let _ = with_state(flush_debug_locked);
}

fn with_state<T>(f: impl FnOnce(&mut UdpUartState) -> T) -> Option<T> {
    if UDP_UART_PAUSED.load(Ordering::Acquire) || !UDP_UART_READY.load(Ordering::Acquire) {
        return None;
    }
    let mut guard = UDP_UART_STATE.lock_irqsave();
    // SAFETY: READY is set only after full initialization under the same lock.
    let state = unsafe { (&mut *guard).assume_init_mut() };
    Some(f(state))
}

fn eth_mut(state: &mut UdpUartState) -> &mut dyn EthernetFrameIo {
    // SAFETY: `state.eth` is initialized once from a `'static` reference in `init`,
    // and all access is serialized by `UDP_UART_STATE` IRQ-save lock.
    unsafe { &mut *state.eth }
}

fn max_rx_frame_len(state: &mut UdpUartState) -> usize {
    state.rx_frame.len().min(eth_mut(state).max_frame_len())
}

fn max_tx_frame_len(state: &mut UdpUartState) -> usize {
    state.tx_frame.len().min(eth_mut(state).max_frame_len())
}

fn max_udp_payload_len(state: &mut UdpUartState) -> usize {
    max_tx_frame_len(state).saturating_sub(UDP_HEADER_OVERHEAD)
}

fn flush_gdb_locked(state: &mut UdpUartState) -> bool {
    if state.gdb_tx_len == 0 {
        return true;
    }
    let Some(peer) = state.gdb_peer else {
        return false;
    };
    let frame_cap = max_tx_frame_len(state);
    if frame_cap < UDP_HEADER_OVERHEAD {
        return false;
    }
    let frame_len = {
        let payload = &state.gdb_tx_payload[..state.gdb_tx_len];
        let frame = &mut state.tx_frame[..frame_cap];
        let Ok(frame_len) = net_proto::encode_udp_ipv4_frame(
            frame,
            state.local_mac,
            peer.peer_mac,
            state.local_ip,
            peer.peer_ip,
            GDB_DST_PORT,
            peer.peer_src_port,
            payload,
        ) else {
            return false;
        };
        frame_len
    };
    if frame_len > frame_cap {
        return false;
    }
    let frame = &state.tx_frame[..frame_len];
    // SAFETY: `state.eth` comes from `init` and access is serialized by the lock held here.
    if unsafe { (&mut *state.eth).try_send_frame(frame) } {
        state.gdb_tx_len = 0;
        true
    } else {
        false
    }
}

fn flush_debug_locked(state: &mut UdpUartState) {
    if state.debug_tx_len == 0 {
        return;
    }
    let Some(peer) = state.debug_peer else {
        state.debug_tx_len = 0;
        return;
    };
    let frame_cap = max_tx_frame_len(state);
    if frame_cap < UDP_HEADER_OVERHEAD {
        state.debug_tx_len = 0;
        return;
    }
    let frame_len = {
        let payload = &state.debug_tx_payload[..state.debug_tx_len];
        let frame = &mut state.tx_frame[..frame_cap];
        net_proto::encode_udp_ipv4_frame(
            frame,
            state.local_mac,
            peer.peer_mac,
            state.local_ip,
            peer.peer_ip,
            DEBUG_DST_PORT,
            peer.peer_src_port,
            payload,
        )
    };
    if let Ok(frame_len) = frame_len {
        if frame_len <= frame_cap {
            let frame = &state.tx_frame[..frame_len];
            // SAFETY: `state.eth` comes from `init` and access is serialized by the lock held here.
            let _ = unsafe { (&mut *state.eth).try_send_frame(frame) };
        }
    }
    state.debug_tx_len = 0;
}

fn send_arp_reply(state: &mut UdpUartState, peer_mac: MacAddr, peer_ip: Ipv4Addr) -> bool {
    let frame_cap = max_tx_frame_len(state);
    if frame_cap < net_proto::eth::HEADER_LEN + net_proto::arp::ARP_PAYLOAD_LEN {
        return false;
    }
    let frame_len = {
        let frame = &mut state.tx_frame[..frame_cap];
        let Ok(frame_len) =
            net_proto::encode_arp_reply(frame, state.local_mac, state.local_ip, peer_mac, peer_ip)
        else {
            return false;
        };
        frame_len
    };
    if frame_len > frame_cap {
        return false;
    }
    let frame = &state.tx_frame[..frame_len];
    // SAFETY: `state.eth` comes from `init` and access is serialized by the lock held here.
    unsafe { (&mut *state.eth).try_send_frame(frame) }
}
