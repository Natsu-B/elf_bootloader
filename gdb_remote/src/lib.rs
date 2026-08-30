//! GDB Remote Serial Protocol (RSP) implementation.
//!
//! Provides a GDB stub for debugging bare-metal targets.

#![no_std]

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LastStopKind {
    Sigtrap = 0,
    WatchRead = 1,
    WatchWrite = 2,
    WatchAccess = 3,
}

#[derive(Clone, Copy, Debug)]
struct LastStop {
    kind: LastStopKind,
    signal: u8,
    addr: u64,
}

impl LastStop {
    const fn sigtrap() -> Self {
        Self {
            kind: LastStopKind::Sigtrap,
            signal: 5,
            addr: 0,
        }
    }

    const fn signal(signal: u8) -> Self {
        Self {
            kind: LastStopKind::Sigtrap,
            signal,
            addr: 0,
        }
    }
}

impl LastStopKind {
    fn from_watch(kind: target::WatchpointKind) -> Self {
        match kind {
            target::WatchpointKind::Read => LastStopKind::WatchRead,
            target::WatchpointKind::Write => LastStopKind::WatchWrite,
            target::WatchpointKind::Access => LastStopKind::WatchAccess,
        }
    }
    fn to_watch(self) -> Option<target::WatchpointKind> {
        match self {
            LastStopKind::WatchRead => Some(target::WatchpointKind::Read),
            LastStopKind::WatchWrite => Some(target::WatchpointKind::Write),
            LastStopKind::WatchAccess => Some(target::WatchpointKind::Access),
            LastStopKind::Sigtrap => None,
        }
    }
}

use core::convert::Infallible;
use core::fmt;
use core::hint::spin_loop;
use core::task::Context;
use core::task::Poll;
use core::task::Waker;
use io_api::stream::PollByteStream;

#[cfg(any(feature = "gdb_monitor_debug", test))]
use core::fmt::Write;

/// Debug print macro for GDB monitor commands.
#[macro_export]
macro_rules! gdb_debug {
    ($server:expr, $($arg:tt)*) => {
        $server.debug_console_fmt(core::format_args!($($arg)*));
    };
}

/// RSP frame parsing.
mod rsp_framing;
/// Target abstraction for debuggable systems.
mod target;

use rsp_framing::RspFrameByteKind;

pub use rsp_framing::RspFrameAssembler;
pub use rsp_framing::RspFrameEvent;
pub use target::ResumeAction;
pub use target::Target;
pub use target::TargetCapabilities;
pub use target::TargetError;
pub use target::WatchpointKind;

/// Errors that can occur while speaking the GDB Remote Serial Protocol.
#[derive(Debug)]
pub enum GdbError<R, U> {
    /// Target-specific error.
    Target(TargetError<R, U>),
    /// Received packet exceeded the provided buffer.
    PacketTooLong,
    /// Packet framing or checksum was invalid.
    MalformedPacket,
    /// TX ring was full while queueing output.
    TxOverflow,
}

/// Result of processing a single RSP packet.
pub enum ProcessResult {
    /// Remain in the stop loop.
    None,
    /// Resume execution with the provided action.
    Resume(ResumeAction),
    /// File-I/O reply packet from GDB.
    FileIoReply {
        /// Return code from the file operation.
        retcode: i64,
        /// Error number (0 on success).
        errno: i32,
        /// True if Ctrl-C was pressed.
        ctrl_c: bool,
    },
    /// Special-case monitor-exit used by the UEFI test harness.
    ///
    /// Note: This is triggered after `qRcmd "exit 0"` arms an exit request and the client
    /// subsequently sends a session-termination packet (e.g. `vKill`, `D`, or `k`).
    MonitorExit,
}
type TargetErr<T> = TargetError<<T as Target>::RecoverableError, <T as Target>::UnrecoverableError>;
type GdbServerError<T> =
    GdbError<<T as Target>::RecoverableError, <T as Target>::UnrecoverableError>;

/// IRQ-facing transport-agnostic interface for the RSP engine.
pub trait RspIrqEndpoint<T: Target> {
    /// Processes a received byte from the transport layer.
    fn on_rx_byte_irq(
        &mut self,
        target: &mut T,
        byte: u8,
    ) -> Result<ProcessResult, GdbServerError<T>>;
    /// Pops a byte from the transmit queue.
    fn pop_tx_byte_irq(&mut self) -> Option<u8>;
    /// Returns true if there are bytes pending transmission.
    fn has_tx_pending(&self) -> bool;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TxOverflow;

struct TxRing<const N: usize> {
    buf: [u8; N],
    head: usize,
    tail: usize,
    full: bool,
}

impl<const N: usize> TxRing<N> {
    const fn new() -> Self {
        Self {
            buf: [0u8; N],
            head: 0,
            tail: 0,
            full: false,
        }
    }

    fn is_empty(&self) -> bool {
        !self.full && self.head == self.tail
    }

    fn len(&self) -> usize {
        if N == 0 {
            return 0;
        }
        if self.full {
            return N;
        }
        if self.head >= self.tail {
            self.head - self.tail
        } else {
            N - (self.tail - self.head)
        }
    }

    fn available(&self) -> usize {
        if N == 0 { 0 } else { N - self.len() }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        if N == 0 {
            return None;
        }
        if offset >= self.len() {
            return None;
        }
        let idx = (self.tail + offset) % N;
        Some(self.buf[idx])
    }

    fn push(&mut self, byte: u8) -> Result<(), TxOverflow> {
        if N == 0 || self.full {
            return Err(TxOverflow);
        }
        self.buf[self.head] = byte;
        self.head = (self.head + 1) % N;
        if self.head == self.tail {
            self.full = true;
        }
        Ok(())
    }

    fn push_slice(&mut self, data: &[u8]) -> Result<(), TxOverflow> {
        if data.len() > self.available() {
            return Err(TxOverflow);
        }
        for &b in data {
            self.push(b)?;
        }
        Ok(())
    }

    fn pop(&mut self) -> Option<u8> {
        if N == 0 || self.is_empty() {
            return None;
        }
        let byte = self.buf[self.tail];
        self.tail = (self.tail + 1) % N;
        self.full = false;
        Some(byte)
    }
}

/// Minimal GDB RSP engine with IRQ-driven I/O.
pub struct GdbServer<const MAX_PKT: usize, const TX_CAP: usize> {
    rsp: RspFrameAssembler,
    rx_buf: [u8; MAX_PKT],
    scratch_a: [u8; MAX_PKT],
    scratch_b: [u8; MAX_PKT],
    #[cfg(any(feature = "gdb_monitor_debug", test))]
    debug_in: [u8; 256],
    #[cfg(any(feature = "gdb_monitor_debug", test))]
    debug_out: [u8; 1 + 256 * 2],
    rx_len: usize,
    rx_checksum: u8,
    rx_checksum_bytes: [u8; 2],
    rx_checksum_len: usize,
    rx_overflow: bool,
    tx: TxRing<TX_CAP>,
    monitor_exit_armed: bool,
    advertised_packet_size: usize,
    ack_mode: bool,
    fileio_parse_error: bool,

    // All-zero must be valid because we may be init'd via zeroing.
    last_stop: LastStop,
}

impl<const MAX_PKT: usize, const TX_CAP: usize> Default for GdbServer<MAX_PKT, TX_CAP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const MAX_PKT: usize, const TX_CAP: usize> GdbServer<MAX_PKT, TX_CAP> {
    /// Create a new server with the default advertised packet size.
    pub fn new() -> Self {
        Self::new_with_packet_size(MAX_PKT)
    }

    /// Create a new server with an explicitly advertised packet size.
    pub fn new_with_packet_size(packet_size: usize) -> Self {
        let advertised_packet_size = Self::clamp_packet_size(packet_size);
        Self {
            rsp: RspFrameAssembler::new(),
            rx_buf: [0; MAX_PKT],
            scratch_a: [0; MAX_PKT],
            scratch_b: [0; MAX_PKT],
            #[cfg(any(feature = "gdb_monitor_debug", test))]
            debug_in: [0; 256],
            #[cfg(any(feature = "gdb_monitor_debug", test))]
            debug_out: [0; 1 + 256 * 2],
            rx_len: 0,
            rx_checksum: 0,
            rx_checksum_bytes: [0u8; 2],
            rx_checksum_len: 0,
            rx_overflow: false,
            tx: TxRing::new(),
            monitor_exit_armed: false,
            advertised_packet_size,
            ack_mode: true,
            fileio_parse_error: false,
            last_stop: LastStop::sigtrap(),
        }
    }

    /// Initialize an uninitialized slot in-place without constructing large stack temporaries.
    ///
    /// NOTE: `new()` / `new_with_packet_size()` may require a large stack frame due to the
    /// internal fixed-size buffers. Prefer this API for bare-metal / tiny-stack environments.
    pub fn init_in_place(dst: &mut core::mem::MaybeUninit<Self>) {
        Self::init_in_place_with_packet_size(dst, MAX_PKT);
    }

    /// Initialize an uninitialized slot in-place with custom packet size.
    pub fn init_in_place_with_packet_size(
        dst: &mut core::mem::MaybeUninit<Self>,
        packet_size: usize,
    ) {
        let advertised_packet_size = Self::clamp_packet_size(packet_size);
        // SAFETY: caller provides an uninitialized slot which we fully initialize here.
        unsafe {
            let p = dst.as_mut_ptr();
            // Zero everything first so rings/buffers start empty and Option/bool fields are sane.
            core::ptr::write_bytes(p, 0u8, 1);
            // Re-init fields that must not rely on "all-zero" being a valid state.
            core::ptr::addr_of_mut!((*p).rsp).write(RspFrameAssembler::new());
            core::ptr::addr_of_mut!((*p).advertised_packet_size).write(advertised_packet_size);
            core::ptr::addr_of_mut!((*p).ack_mode).write(true);
            core::ptr::addr_of_mut!((*p).last_stop).write(LastStop::sigtrap());
        }
    }

    fn clamp_packet_size(packet_size: usize) -> usize {
        let mut size = packet_size;
        if size == 0 {
            size = 1;
        }
        if size > MAX_PKT {
            size = MAX_PKT;
        }
        size
    }

    fn out_payload_cap(&self) -> usize {
        self.advertised_packet_size.min(MAX_PKT)
    }
    #[cfg(any(feature = "gdb_monitor_debug", test))]
    pub fn debug_console_fmt(&mut self, args: fmt::Arguments<'_>) {
        let out_cap = self.out_payload_cap();
        let max_msg = out_cap.saturating_sub(1) / 2;
        if max_msg == 0 {
            return;
        }

        let cap = core::cmp::min(self.debug_in.len(), max_msg);
        let mut writer = DebugBufWriter::new(&mut self.debug_in, cap);
        let _ = writer.write_fmt(args);
        let msg_len = writer.len();
        if msg_len == 0 {
            return;
        }

        self.debug_out[0] = b'O';
        let hex_len = hex_encode(&self.debug_in[..msg_len], &mut self.debug_out[1..]);
        let total_len = 1usize.saturating_add(hex_len);
        if total_len > out_cap {
            return;
        }
        let _ = Self::queue_packet(&mut self.tx, &self.debug_out[..total_len]);
    }

    #[cfg(all(not(feature = "gdb_monitor_debug"), not(test)))]
    /// Writes a formatted message to the debug console (no-op when disabled).
    pub fn debug_console_fmt(&mut self, _args: fmt::Arguments<'_>) {
        let _ = _args;
    }

    fn reset_rx_buffers(&mut self) {
        self.rx_len = 0;
        self.rx_checksum = 0;
        self.rx_checksum_len = 0;
        self.rx_overflow = false;
    }

    fn reset_rx_full(&mut self) {
        self.reset_rx_buffers();
        self.rsp.reset();
    }

    /// Reset framing/receive state to wait for the next '$' packet start.
    pub fn resync(&mut self) {
        self.reset_rx_full();
    }

    /// Returns and clears the file-I/O parse error flag.
    pub fn take_fileio_parse_error(&mut self) -> bool {
        let had_error = self.fileio_parse_error;
        self.fileio_parse_error = false;
        had_error
    }

    fn push_payload_byte(&mut self, byte: u8) {
        self.rx_checksum = self.rx_checksum.wrapping_add(byte);
        if self.rx_len < self.rx_buf.len() {
            self.rx_buf[self.rx_len] = byte;
            self.rx_len = self.rx_len.saturating_add(1);
        } else {
            self.rx_overflow = true;
        }
    }

    fn push_checksum_byte(&mut self, byte: u8) {
        if self.rx_checksum_len < self.rx_checksum_bytes.len() {
            self.rx_checksum_bytes[self.rx_checksum_len] = byte;
            self.rx_checksum_len = self.rx_checksum_len.saturating_add(1);
        }
    }

    fn queue_ack(tx: &mut TxRing<TX_CAP>, ok: bool) -> Result<(), TxOverflow> {
        let byte = if ok { b'+' } else { b'-' };
        tx.push(byte)
    }

    fn queue_packet(tx: &mut TxRing<TX_CAP>, payload: &[u8]) -> Result<(), TxOverflow> {
        let needed = payload.len().saturating_add(4);
        if needed > tx.available() {
            return Err(TxOverflow);
        }

        let mut checksum: u8 = 0;
        for &b in payload {
            checksum = checksum.wrapping_add(b);
        }

        tx.push(b'$')?;
        tx.push_slice(payload)?;
        tx.push(b'#')?;
        tx.push(HEX[(checksum >> 4) as usize])?;
        tx.push(HEX[(checksum & 0xF) as usize])?;
        Ok(())
    }

    fn send<T: Target>(&mut self, payload: &[u8]) -> Result<(), GdbServerError<T>> {
        if payload.len() > self.out_payload_cap() {
            return Err(GdbError::PacketTooLong);
        }
        Self::queue_packet(&mut self.tx, payload).map_err(|_| GdbError::TxOverflow)
    }

    fn send_empty<T: Target>(&mut self) -> Result<(), GdbServerError<T>> {
        self.send::<T>(b"")
    }

    fn send_ok<T: Target>(&mut self) -> Result<(), GdbServerError<T>> {
        self.send::<T>(b"OK")
    }

    fn send_scratch_b<T: Target>(&mut self, len: usize) -> Result<(), GdbServerError<T>> {
        let payload = &self.scratch_b[..len];
        Self::queue_packet(&mut self.tx, payload).map_err(|_| GdbError::TxOverflow)
    }

    /// Queues a raw packet payload for transmission.
    pub fn queue_packet_payload(
        &mut self,
        payload: &[u8],
    ) -> Result<(), GdbError<Infallible, Infallible>> {
        if payload.len() > self.out_payload_cap() {
            return Err(GdbError::PacketTooLong);
        }
        Self::queue_packet(&mut self.tx, payload).map_err(|_| GdbError::TxOverflow)
    }

    /// Sends a stop notification with a given signal number.
    pub fn notify_stop_signal(
        &mut self,
        signal: u8,
    ) -> Result<(), GdbError<Infallible, Infallible>> {
        self.last_stop = LastStop::signal(signal);
        let out_cap = self.out_payload_cap();
        let payload = [
            b'S',
            HEX[(signal >> 4) as usize],
            HEX[(signal & 0xF) as usize],
        ];
        if payload.len() > out_cap {
            return Err(GdbError::PacketTooLong);
        }
        Self::queue_packet(&mut self.tx, &payload).map_err(|_| GdbError::TxOverflow)
    }

    /// Sends a stop notification for SIGTRAP (signal 5).
    pub fn notify_stop_sigtrap(&mut self) -> Result<(), GdbError<Infallible, Infallible>> {
        self.notify_stop_signal(5)
    }

    /// Sends a stop notification for a watchpoint hit.
    pub fn notify_stop_watch(
        &mut self,
        kind: WatchpointKind,
        addr: u64,
    ) -> Result<(), GdbError<Infallible, Infallible>> {
        self.last_stop = LastStop {
            kind: LastStopKind::from_watch(kind),
            signal: 5,
            addr,
        };
        let out_cap = self.out_payload_cap();
        let mut payload = [0u8; 32];
        let mut idx = 0usize;
        payload[idx] = b'T';
        idx += 1;
        payload[idx] = b'0';
        idx += 1;
        payload[idx] = b'5';
        idx += 1;
        let kind_bytes: &[u8] = match kind {
            WatchpointKind::Write => b"watch",
            WatchpointKind::Read => b"rwatch",
            WatchpointKind::Access => b"awatch",
        };
        append_bytes(&mut payload, &mut idx, kind_bytes);
        payload[idx] = b':';
        idx += 1;
        append_hex_u64(&mut payload, &mut idx, addr);
        payload[idx] = b';';
        idx += 1;
        if idx > out_cap {
            return Err(GdbError::PacketTooLong);
        }
        Self::queue_packet(&mut self.tx, &payload[..idx]).map_err(|_| GdbError::TxOverflow)
    }

    /// Drops pending console output packets from the transmit queue.
    pub fn drop_console_output(&mut self) {
        // Drop pending console packets to make room for stop replies.
        loop {
            if self.tx.len() < 2 {
                return;
            }
            let Some(first) = self.tx.peek(0) else {
                return;
            };
            if first != b'$' {
                return;
            }
            let Some(kind) = self.tx.peek(1) else {
                return;
            };
            if kind != b'O' {
                return;
            }
            let _ = self.tx.pop();
            let _ = self.tx.pop();
            while let Some(next) = self.tx.pop() {
                if next == b'#' {
                    let _ = self.tx.pop();
                    let _ = self.tx.pop();
                    break;
                }
            }
        }
    }

    /// Records a stop signal without sending a notification.
    pub fn set_last_stop_signal(&mut self, signal: u8) {
        self.last_stop = LastStop::signal(signal);
    }

    /// Records SIGTRAP as the last stop reason.
    pub fn set_last_stop_sigtrap(&mut self) {
        self.set_last_stop_signal(5);
    }

    /// Records a watchpoint hit as the last stop reason.
    pub fn set_last_stop_watch(&mut self, kind: WatchpointKind, addr: u64) {
        self.last_stop = LastStop {
            kind: LastStopKind::from_watch(kind),
            signal: 5,
            addr,
        };
    }

    fn send_last_stop_reply<T: Target>(&mut self) -> Result<(), GdbServerError<T>> {
        let Some(wk) = self.last_stop.kind.to_watch() else {
            let signal = self.last_stop.signal;
            let payload = [
                b'S',
                HEX[(signal >> 4) as usize],
                HEX[(signal & 0xF) as usize],
            ];
            self.send::<T>(&payload)?;
            return Ok(());
        };
        // Mirror notify_stop_watch payload format.
        let mut scratch = [0u8; 32];
        let mut len = 0usize;
        scratch[len..len + 3].copy_from_slice(b"T05");
        len += 3;
        let label: &[u8] = match wk {
            WatchpointKind::Write => b"watch:",
            WatchpointKind::Read => b"rwatch:",
            WatchpointKind::Access => b"awatch:",
        };
        scratch[len..len + label.len()].copy_from_slice(label);
        len += label.len();
        append_hex_u64(&mut scratch, &mut len, self.last_stop.addr);
        scratch[len] = b';';
        len += 1;
        self.send::<T>(&scratch[..len])?;
        Ok(())
    }

    fn finish_frame<T: Target>(
        &mut self,
        target: &mut T,
    ) -> Result<ProcessResult, GdbServerError<T>> {
        if self.rx_overflow || self.rx_len > MAX_PKT {
            if self.ack_mode {
                Self::queue_ack(&mut self.tx, false).map_err(|_| GdbError::TxOverflow)?;
            }
            return Err(GdbError::PacketTooLong);
        }

        if self.rx_checksum_len != self.rx_checksum_bytes.len() {
            if self.ack_mode {
                Self::queue_ack(&mut self.tx, false).map_err(|_| GdbError::TxOverflow)?;
            }
            return Err(GdbError::MalformedPacket);
        }

        let high = match from_hex_digit(self.rx_checksum_bytes[0]) {
            Ok(v) => v,
            Err(_) => {
                if self.ack_mode {
                    Self::queue_ack(&mut self.tx, false).map_err(|_| GdbError::TxOverflow)?;
                }
                return Err(GdbError::MalformedPacket);
            }
        };
        let low = match from_hex_digit(self.rx_checksum_bytes[1]) {
            Ok(v) => v,
            Err(_) => {
                if self.ack_mode {
                    Self::queue_ack(&mut self.tx, false).map_err(|_| GdbError::TxOverflow)?;
                }
                return Err(GdbError::MalformedPacket);
            }
        };
        let checksum_recv = (high << 4) | low;

        if self.rx_checksum != checksum_recv {
            if self.ack_mode {
                Self::queue_ack(&mut self.tx, false).map_err(|_| GdbError::TxOverflow)?;
            }
            return Err(GdbError::MalformedPacket);
        }

        if self.ack_mode {
            Self::queue_ack(&mut self.tx, true).map_err(|_| GdbError::TxOverflow)?;
        }

        let payload_len = self.rx_len;
        let payload_ptr = self.rx_buf.as_ptr();
        // SAFETY: payload_ptr points to rx_buf for payload_len bytes. dispatch_payload only
        // reads from the payload slice, and handlers use scratch_a/scratch_b for decoding and
        // replies, so rx_buf is not mutated until dispatch completes.
        let payload = unsafe { core::slice::from_raw_parts(payload_ptr, payload_len) };
        self.dispatch_payload(target, payload)
    }

    fn dispatch_payload<T: Target>(
        &mut self,
        target: &mut T,
        payload: &[u8],
    ) -> Result<ProcessResult, GdbServerError<T>> {
        gdb_debug!(
            self,
            "dispatch: payload=\"{}\"",
            debug_printable_prefix(payload)
        );

        if payload.first() == Some(&b'F') {
            if let Some((retcode, errno, ctrl_c)) = parse_fileio_reply(payload) {
                return Ok(ProcessResult::FileIoReply {
                    retcode,
                    errno,
                    ctrl_c,
                });
            }
            self.fileio_parse_error = true;
            return Ok(ProcessResult::None);
        }

        if payload == b"?" {
            // Breakpoint: '?' stop-reply path (server last_stop).
            self.send_last_stop_reply::<T>()?;
            return Ok(ProcessResult::None);
        }

        match payload.first().copied() {
            Some(b'q') => {
                gdb_debug!(
                    self,
                    "dispatch: 'q' (query) payload=\"{}\"",
                    debug_printable_prefix(payload)
                );
                self.handle_query(target, payload)
            }
            Some(b'D') => {
                // Detach. Reply OK. If a monitor-exit was armed, finish the harness.
                gdb_debug!(self, "dispatch: 'D' (detach)");
                self.send_ok::<T>()?;
                if self.monitor_exit_armed {
                    self.monitor_exit_armed = false;
                    return Ok(ProcessResult::MonitorExit);
                }
                Ok(ProcessResult::None)
            }
            Some(b'k') => {
                // Kill. Reply OK (harmless even if client ignores it).
                // If a monitor-exit was armed, finish the harness.
                gdb_debug!(self, "dispatch: 'k' (kill)");
                self.send_ok::<T>()?;
                if self.monitor_exit_armed {
                    self.monitor_exit_armed = false;
                    return Ok(ProcessResult::MonitorExit);
                }
                Ok(ProcessResult::None)
            }
            Some(b'g') => {
                gdb_debug!(self, "dispatch: 'g' (read all registers)");
                self.handle_read_all_registers(target)
            }
            Some(b'G') => {
                gdb_debug!(self, "dispatch: 'G' (write all registers)");
                self.handle_write_all_registers(target, payload)
            }
            Some(b'H') => {
                gdb_debug!(
                    self,
                    "dispatch: 'H' (set thread) payload=\"{}\"",
                    debug_printable_prefix(payload)
                );
                self.handle_set_thread::<T>(payload)
            }
            Some(b'p') => {
                gdb_debug!(
                    self,
                    "dispatch: 'p' (read single register) payload=\"{}\"",
                    debug_printable_prefix(payload)
                );
                self.handle_read_single_register(target, payload)
            }
            Some(b'P') => {
                gdb_debug!(
                    self,
                    "dispatch: 'P' (write single register) payload=\"{}\"",
                    debug_printable_prefix(payload)
                );
                self.handle_write_single_register(target, payload)
            }
            Some(b'm') => {
                gdb_debug!(
                    self,
                    "dispatch: 'm' (read memory) payload=\"{}\"",
                    debug_printable_prefix(payload)
                );
                self.handle_read_memory(target, payload)
            }
            Some(b'M') => {
                gdb_debug!(
                    self,
                    "dispatch: 'M' (write memory hex) payload=\"{}\"",
                    debug_printable_prefix(payload)
                );
                self.handle_write_memory_hex(target, payload)
            }
            Some(b'X') => {
                gdb_debug!(
                    self,
                    "dispatch: 'X' (write memory binary) payload=\"{}\"",
                    debug_printable_prefix(payload)
                );
                self.handle_write_memory_binary(target, payload)
            }
            Some(b'Z') => {
                gdb_debug!(
                    self,
                    "dispatch: 'Z' (insert breakpoint) payload=\"{}\"",
                    debug_printable_prefix(payload)
                );
                self.handle_breakpoint(target, payload, true)
            }
            Some(b'z') => {
                gdb_debug!(
                    self,
                    "dispatch: 'z' (remove breakpoint) payload=\"{}\"",
                    debug_printable_prefix(payload)
                );
                self.handle_breakpoint(target, payload, false)
            }
            Some(b'c') => {
                gdb_debug!(
                    self,
                    "dispatch: 'c' (continue) payload=\"{}\"",
                    debug_printable_prefix(payload)
                );
                self.handle_continue::<T>(payload)
            }
            Some(b's') => {
                gdb_debug!(
                    self,
                    "dispatch: 's' (step) payload=\"{}\"",
                    debug_printable_prefix(payload)
                );
                self.handle_step::<T>(payload)
            }
            Some(b'v') => {
                gdb_debug!(
                    self,
                    "dispatch: 'v' (v-packet) payload=\"{}\"",
                    debug_printable_prefix(payload)
                );
                self.handle_v_packet(target, payload)
            }
            _ => {
                gdb_debug!(
                    self,
                    "dispatch: unknown first byte {:?}, replying empty",
                    payload.first().copied()
                );
                self.send_empty::<T>()?;
                Ok(ProcessResult::None)
            }
        }
    }

    /// Accept a received byte (RX IRQ path).
    pub fn on_rx_byte_irq<T: Target>(
        &mut self,
        target: &mut T,
        byte: u8,
    ) -> Result<ProcessResult, GdbServerError<T>> {
        let (event, kind) = self.rsp.push_with_kind(byte);

        match kind {
            RspFrameByteKind::Payload => self.push_payload_byte(byte),
            RspFrameByteKind::Checksum => self.push_checksum_byte(byte),
            RspFrameByteKind::None => {}
        }

        match event {
            RspFrameEvent::Ignore | RspFrameEvent::NeedMore => Ok(ProcessResult::None),
            RspFrameEvent::Resync => {
                self.reset_rx_buffers();
                Ok(ProcessResult::None)
            }
            RspFrameEvent::CtrlC => {
                self.reset_rx_full();
                self.send::<T>(b"S05")?;
                Ok(ProcessResult::None)
            }
            RspFrameEvent::FrameComplete => {
                let result = self.finish_frame(target);
                self.reset_rx_full();
                result
            }
        }
    }

    /// Pop the next pending TX byte (TX IRQ path).
    pub fn pop_tx_byte_irq(&mut self) -> Option<u8> {
        self.tx.pop()
    }

    /// Returns true if there are pending bytes to transmit.
    pub fn has_tx_pending(&self) -> bool {
        !self.tx.is_empty()
    }

    /// Runs the GDB stub until a monitor-exit is requested.
    pub fn run_until_monitor_exit<S: PollByteStream<Error = Infallible>, T: Target>(
        &mut self,
        stream: &mut S,
        target: &mut T,
    ) -> Result<(), GdbServerError<T>> {
        // This server drives the stream synchronously instead of through an executor.
        // Receive and transmit are polled on every loop iteration.
        // `Poll::Pending` retries after `spin_loop`, so no task needs rescheduling.
        // The no-op waker therefore never needs to schedule this loop.
        // It remains borrowed only for the lifetime of this polling context.
        // Transports used here must make progress when polled repeatedly.
        // `Waker::noop` expresses that contract without a custom raw vtable.
        let mut cx = Context::from_waker(Waker::noop());
        let mut rx_buf = [0u8; 64];
        let mut tx_buf = [0u8; 128];
        let mut tx_pos = 0usize;
        let mut tx_len = 0usize;
        let mut exit_requested = false;

        loop {
            let mut progress = false;

            while !exit_requested {
                match stream.poll_read(&mut cx, &mut rx_buf) {
                    Poll::Ready(Ok(len)) => {
                        if len == 0 {
                            break;
                        }
                        progress = true;
                        for &byte in &rx_buf[..len] {
                            match self.on_rx_byte_irq(target, byte) {
                                Ok(ProcessResult::MonitorExit) => {
                                    exit_requested = true;
                                    break;
                                }
                                Ok(ProcessResult::None)
                                | Ok(ProcessResult::Resume(_))
                                | Ok(ProcessResult::FileIoReply { .. }) => {}
                                Err(GdbError::MalformedPacket | GdbError::PacketTooLong) => {
                                    self.resync();
                                }
                                Err(err) => return Err(err),
                            }
                        }
                        if exit_requested {
                            break;
                        }
                    }
                    Poll::Pending => break,
                    Poll::Ready(Err(err)) => match err {},
                }
            }

            if tx_pos < tx_len {
                match stream.poll_write(&mut cx, &tx_buf[tx_pos..tx_len]) {
                    Poll::Ready(Ok(written)) => {
                        if written != 0 {
                            tx_pos = tx_pos.saturating_add(written);
                            progress = true;
                            if tx_pos >= tx_len {
                                tx_pos = 0;
                                tx_len = 0;
                            }
                        }
                    }
                    Poll::Pending => {}
                    Poll::Ready(Err(err)) => match err {},
                }
            }

            if tx_pos == tx_len {
                tx_pos = 0;
                tx_len = 0;
                while tx_len < tx_buf.len() {
                    let Some(byte) = self.pop_tx_byte_irq() else {
                        break;
                    };
                    tx_buf[tx_len] = byte;
                    tx_len += 1;
                }
                if tx_len != 0 {
                    match stream.poll_write(&mut cx, &tx_buf[..tx_len]) {
                        Poll::Ready(Ok(written)) => {
                            if written != 0 {
                                progress = true;
                                tx_pos = written;
                                if tx_pos >= tx_len {
                                    tx_pos = 0;
                                    tx_len = 0;
                                }
                            }
                        }
                        Poll::Pending => {}
                        Poll::Ready(Err(err)) => match err {},
                    }
                }
            }

            if exit_requested && tx_pos == tx_len && !self.has_tx_pending() {
                match stream.poll_flush(&mut cx) {
                    Poll::Ready(Ok(())) => return Ok(()),
                    Poll::Pending => {}
                    Poll::Ready(Err(err)) => match err {},
                }
            }

            if progress {
                match stream.poll_flush(&mut cx) {
                    Poll::Ready(Ok(())) | Poll::Pending => {}
                    Poll::Ready(Err(err)) => match err {},
                }
            } else {
                spin_loop();
            }
        }
    }

    fn reply_recoverable<T: Target>(
        &mut self,
        target: &T,
        e: &T::RecoverableError,
    ) -> Result<(), GdbServerError<T>> {
        let code = target.recoverable_error_code(e);
        let reply = [b'E', HEX[(code >> 4) as usize], HEX[(code & 0xF) as usize]];
        self.send::<T>(&reply)?;
        Ok(())
    }

    fn handle_target_err_core<T: Target, R>(
        &mut self,
        target: &T,
        result: Result<R, TargetErr<T>>,
    ) -> Result<Option<R>, GdbServerError<T>> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(TargetError::Recoverable(e)) => {
                self.reply_recoverable(target, &e)?;
                Ok(None)
            }
            Err(TargetError::NotSupported) => {
                self.send::<T>(b"E01")?;
                Ok(None)
            }
            Err(TargetError::Unrecoverable(e)) => {
                Err(GdbError::Target(TargetError::Unrecoverable(e)))
            }
        }
    }

    fn handle_target_err_optional<T: Target>(
        &mut self,
        target: &T,
        result: Result<(), TargetErr<T>>,
    ) -> Result<bool, GdbServerError<T>> {
        match result {
            Ok(()) => Ok(false),
            Err(TargetError::NotSupported) => {
                self.send_empty::<T>()?;
                Ok(true)
            }
            Err(TargetError::Recoverable(e)) => {
                self.reply_recoverable(target, &e)?;
                Ok(true)
            }
            Err(TargetError::Unrecoverable(e)) => {
                Err(GdbError::Target(TargetError::Unrecoverable(e)))
            }
        }
    }

    fn handle_query<T: Target>(
        &mut self,
        target: &mut T,
        payload: &[u8],
    ) -> Result<ProcessResult, GdbServerError<T>> {
        gdb_debug!(
            self,
            "handle_query: payload=\"{}\"",
            debug_printable_prefix(payload)
        );
        if payload.starts_with(b"qSupported") {
            gdb_debug!(self, "handle_query: qSupported");
            // A new handshake implies a new session. Clear any stale monitor-exit state.
            self.monitor_exit_armed = false;

            let caps = target.capabilities();
            let wants_aarch64_xml = caps.contains(TargetCapabilities::XFER_FEATURES)
                && qsupported_has_xml_registers(payload, b"aarch64");
            let mut reply = [0u8; 128];
            let mut idx = 0usize;
            append_bytes(&mut reply, &mut idx, b"PacketSize=");
            // PacketSize is advertised as lowercase hex without a 0x prefix.
            append_hex_u64(&mut reply, &mut idx, self.out_payload_cap() as u64);
            append_bytes(&mut reply, &mut idx, b";vMustReplyEmpty+;vFlash+");
            if caps.contains(TargetCapabilities::SW_BREAK) {
                append_bytes(&mut reply, &mut idx, b";swbreak+");
            } else {
                append_bytes(&mut reply, &mut idx, b";swbreak-");
            }
            if caps.contains(TargetCapabilities::HW_BREAK) {
                append_bytes(&mut reply, &mut idx, b";hwbreak+");
            } else {
                append_bytes(&mut reply, &mut idx, b";hwbreak-");
            }
            if caps.contains(TargetCapabilities::VCONT) {
                append_bytes(&mut reply, &mut idx, b";vContSupported+");
            } else {
                append_bytes(&mut reply, &mut idx, b";vContSupported-");
            }
            if caps.contains(TargetCapabilities::XFER_FEATURES) {
                append_bytes(&mut reply, &mut idx, b";qXfer:features:read+");
            }
            if caps.contains(TargetCapabilities::XFER_MEMORY_MAP) {
                append_bytes(&mut reply, &mut idx, b";qXfer:memory-map:read+");
            }
            if wants_aarch64_xml {
                append_bytes(&mut reply, &mut idx, b";xmlRegisters=aarch64");
            }
            self.send::<T>(&reply[..idx])?;
            return Ok(ProcessResult::None);
        }

        if let Some(rest) = payload.strip_prefix(b"qXfer:features:read:") {
            if !target
                .capabilities()
                .contains(TargetCapabilities::XFER_FEATURES)
            {
                self.send_empty::<T>()?;
                return Ok(ProcessResult::None);
            }
            let Some((annex, offset, length)) = parse_qxfer_read(rest, false) else {
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            };
            let annex = match core::str::from_utf8(annex) {
                Ok(value) => value,
                Err(_) => {
                    self.send::<T>(b"E01")?;
                    return Ok(ProcessResult::None);
                }
            };

            let data = match target.xfer_features(annex) {
                Ok(Some(data)) => data,
                Ok(None) | Err(TargetError::NotSupported) => {
                    self.send::<T>(b"E01")?;
                    return Ok(ProcessResult::None);
                }
                Err(TargetError::Recoverable(e)) => {
                    self.reply_recoverable(target, &e)?;
                    return Ok(ProcessResult::None);
                }
                Err(TargetError::Unrecoverable(e)) => {
                    return Err(GdbError::Target(TargetError::Unrecoverable(e)));
                }
            };

            let offset = match usize::try_from(offset) {
                Ok(value) => value,
                Err(_) => {
                    self.send::<T>(b"E01")?;
                    return Ok(ProcessResult::None);
                }
            };
            let length = match usize::try_from(length) {
                Ok(value) => value,
                Err(_) => {
                    self.send::<T>(b"E01")?;
                    return Ok(ProcessResult::None);
                }
            };

            if offset >= data.len() {
                self.send::<T>(b"l")?;
                return Ok(ProcessResult::None);
            }

            let max_len = core::cmp::min(length, data.len() - offset);
            if max_len == 0 {
                let prefix = if offset < data.len() { b'm' } else { b'l' };
                let reply = [prefix];
                self.send::<T>(&reply)?;
                return Ok(ProcessResult::None);
            }

            let cap = self.out_payload_cap();
            if cap < 1 {
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            }

            let (consumed, encoded_len) = {
                let reply = &mut self.scratch_b;
                encode_rsp_binary(&data[offset..offset + max_len], &mut reply[1..cap])
            };
            if consumed == 0 {
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            }

            let more = offset + consumed < data.len();
            self.scratch_b[0] = if more { b'm' } else { b'l' };
            self.send_scratch_b::<T>(1 + encoded_len)?;
            return Ok(ProcessResult::None);
        }

        if let Some(rest) = payload.strip_prefix(b"qXfer:memory-map:read:") {
            if !target
                .capabilities()
                .contains(TargetCapabilities::XFER_MEMORY_MAP)
            {
                self.send_empty::<T>()?;
                return Ok(ProcessResult::None);
            }
            let Some((_annex, offset, length)) = parse_qxfer_read(rest, true) else {
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            };

            let data = match target.xfer_memory_map() {
                Ok(Some(data)) => data,
                Ok(None) | Err(TargetError::NotSupported) => {
                    self.send::<T>(b"E01")?;
                    return Ok(ProcessResult::None);
                }
                Err(TargetError::Recoverable(e)) => {
                    self.reply_recoverable(target, &e)?;
                    return Ok(ProcessResult::None);
                }
                Err(TargetError::Unrecoverable(e)) => {
                    return Err(GdbError::Target(TargetError::Unrecoverable(e)));
                }
            };

            let offset = match usize::try_from(offset) {
                Ok(value) => value,
                Err(_) => {
                    self.send::<T>(b"E01")?;
                    return Ok(ProcessResult::None);
                }
            };
            let length = match usize::try_from(length) {
                Ok(value) => value,
                Err(_) => {
                    self.send::<T>(b"E01")?;
                    return Ok(ProcessResult::None);
                }
            };

            if offset >= data.len() {
                self.send::<T>(b"l")?;
                return Ok(ProcessResult::None);
            }

            let max_len = core::cmp::min(length, data.len() - offset);
            if max_len == 0 {
                let prefix = if offset < data.len() { b'm' } else { b'l' };
                let reply = [prefix];
                self.send::<T>(&reply)?;
                return Ok(ProcessResult::None);
            }

            let cap = self.out_payload_cap();
            if cap < 1 {
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            }

            let (consumed, encoded_len) = {
                let reply = &mut self.scratch_b;
                encode_rsp_binary(&data[offset..offset + max_len], &mut reply[1..cap])
            };
            if consumed == 0 {
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            }

            let more = offset + consumed < data.len();
            self.scratch_b[0] = if more { b'm' } else { b'l' };
            self.send_scratch_b::<T>(1 + encoded_len)?;
            return Ok(ProcessResult::None);
        }

        if let Some(rest) = payload.strip_prefix(b"qRcmd,") {
            gdb_debug!(
                self,
                "handle_query: qRcmd raw=\"{}\"",
                debug_printable_prefix(rest)
            );
            if rest.len() / 2 > self.scratch_a.len() {
                gdb_debug!(
                    self,
                    "handle_query: qRcmd hex decode failed src_len={} dst_cap={}",
                    rest.len(),
                    self.scratch_a.len()
                );
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            }
            let decoded_len = match hex_decode(rest, &mut self.scratch_a) {
                Ok(len) => len,
                Err(_) => {
                    gdb_debug!(
                        self,
                        "handle_query: qRcmd hex decode failed src_len={} dst_cap={}",
                        rest.len(),
                        self.scratch_a.len()
                    );
                    self.send::<T>(b"E01")?;
                    return Ok(ProcessResult::None);
                }
            };
            let decoded_print = debug_printable_prefix(&self.scratch_a[..decoded_len]);
            gdb_debug!(self, "handle_query: qRcmd decoded=\"{}\"", decoded_print);
            let decoded_cmd = &self.scratch_a[..decoded_len];
            if let Ok(text) = core::str::from_utf8(decoded_cmd) {
                if text.trim() == "exit 0" {
                    self.send_ok::<T>()?;
                    // Do NOT terminate immediately: GDB typically sends `vKill` (or `D`)
                    // after this command as part of its shutdown path. Exiting here makes
                    // the transport disappear mid-teardown (Broken pipe).
                    self.monitor_exit_armed = true;
                    return Ok(ProcessResult::None);
                }
            }

            let out_cap = self.out_payload_cap();
            let reply_cap = out_cap;
            let result = {
                let out_buf = &mut self.scratch_b[..out_cap];
                target.monitor_command(decoded_cmd, out_buf)
            };
            match result {
                Err(TargetError::NotSupported) => {
                    self.send_empty::<T>()?;
                }
                Err(TargetError::Recoverable(e)) => {
                    self.reply_recoverable(target, &e)?;
                }
                Err(TargetError::Unrecoverable(e)) => {
                    return Err(GdbError::Target(TargetError::Unrecoverable(e)));
                }
                Ok(0) => {
                    self.send_ok::<T>()?;
                }
                Ok(n) => {
                    const TRUNCATED_SUFFIX: &[u8] = b"\n(truncated)\n";
                    if reply_cap < 2 {
                        self.send::<T>(b"E01")?;
                        return Ok(ProcessResult::None);
                    }

                    let out_buf = &mut self.scratch_b[..out_cap];
                    let mut out_len = n.min(out_buf.len());
                    ensure_trailing_newline(out_buf, &mut out_len);
                    let expected_hex_len = out_len.saturating_mul(2);
                    let mut reply_error = false;
                    let reply_len = {
                        let reply = &mut self.scratch_a[..reply_cap];
                        if expected_hex_len <= reply_cap {
                            let reply_len = hex_encode(&out_buf[..out_len], reply);
                            if reply_len < expected_hex_len {
                                reply_error = true;
                            }
                            reply_len
                        } else {
                            let suffix_hex_len = TRUNCATED_SUFFIX.len().saturating_mul(2);
                            let mut prefix_len = out_len;
                            let mut append_suffix = false;
                            if reply_cap > suffix_hex_len {
                                let max_prefix = (reply_cap - suffix_hex_len) / 2;
                                prefix_len = prefix_len.min(max_prefix);
                                append_suffix = true;
                            } else {
                                prefix_len = reply_cap / 2;
                            }

                            let mut reply_len = hex_encode(&out_buf[..prefix_len], reply);
                            if append_suffix {
                                reply_len +=
                                    hex_encode(TRUNCATED_SUFFIX, &mut reply[reply_len..reply_cap]);
                            }
                            reply_len
                        }
                    };
                    if reply_error {
                        self.send::<T>(b"E01")?;
                        return Ok(ProcessResult::None);
                    }
                    let payload = &self.scratch_a[..reply_len];
                    Self::queue_packet(&mut self.tx, payload).map_err(|_| GdbError::TxOverflow)?;
                }
            }
            return Ok(ProcessResult::None);
        }

        gdb_debug!(self, "handle_query: unknown query, replying empty");
        self.send_empty::<T>()?;
        Ok(ProcessResult::None)
    }

    fn handle_read_all_registers<T: Target>(
        &mut self,
        target: &mut T,
    ) -> Result<ProcessResult, GdbServerError<T>> {
        let result = {
            let regs = &mut self.scratch_a;
            target.read_registers(regs)
        };
        let len = match self.handle_target_err_core(target, result)? {
            Some(len) => len,
            None => return Ok(ProcessResult::None),
        };
        gdb_debug!(self, "handle_read_all_registers: total_len={} bytes", len);

        let out_cap = self.out_payload_cap();
        let expected_hex_len = len.saturating_mul(2);
        if expected_hex_len > out_cap {
            gdb_debug!(
                self,
                "handle_read_all_registers: response too large hex_len={} out_cap={}",
                expected_hex_len,
                out_cap
            );
            self.send::<T>(b"E01")?;
            return Ok(ProcessResult::None);
        }
        let hex_len = {
            let out = &mut self.scratch_b[..out_cap];
            let regs = &self.scratch_a[..len];
            hex_encode(regs, out)
        };
        if hex_len < expected_hex_len {
            gdb_debug!(
                self,
                "handle_read_all_registers: hex_encode truncated expected={} actual={}",
                expected_hex_len,
                hex_len
            );
            self.send::<T>(b"E01")?;
            return Ok(ProcessResult::None);
        }
        self.send_scratch_b::<T>(hex_len)?;
        Ok(ProcessResult::None)
    }

    fn handle_write_all_registers<T: Target>(
        &mut self,
        target: &mut T,
        payload: &[u8],
    ) -> Result<ProcessResult, GdbServerError<T>> {
        let data_hex = &payload[1..];
        gdb_debug!(
            self,
            "handle_write_all_registers: payload_len={} hex_len={}",
            payload.len(),
            data_hex.len()
        );
        let len = match {
            let regs = &mut self.scratch_a;
            hex_decode(data_hex, regs)
        } {
            Ok(len) => len,
            Err(_) => {
                gdb_debug!(
                    self,
                    "handle_write_all_registers: hex decode failed src_len={} dst_cap={}",
                    data_hex.len(),
                    self.scratch_a.len()
                );
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            }
        };
        gdb_debug!(self, "handle_write_all_registers: decoded_len={}", len);
        let result = target.write_registers(&self.scratch_a[..len]);
        if self.handle_target_err_core(target, result)?.is_none() {
            return Ok(ProcessResult::None);
        }
        self.send_ok::<T>()?;
        Ok(ProcessResult::None)
    }

    fn handle_set_thread<T: Target>(
        &mut self,
        payload: &[u8],
    ) -> Result<ProcessResult, GdbServerError<T>> {
        let Some(kind) = payload.get(1).copied() else {
            self.send_empty::<T>()?;
            return Ok(ProcessResult::None);
        };

        match kind {
            b'c' | b'g' => {
                self.send_ok::<T>()?;
                Ok(ProcessResult::None)
            }
            _ => {
                self.send_empty::<T>()?;
                Ok(ProcessResult::None)
            }
        }
    }

    fn handle_read_single_register<T: Target>(
        &mut self,
        target: &mut T,
        payload: &[u8],
    ) -> Result<ProcessResult, GdbServerError<T>> {
        let regno = match parse_hex_u32(&payload[1..]) {
            Some(r) => r,
            None => {
                gdb_debug!(self, "handle_read_single_register: bad regno payload");
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            }
        };

        gdb_debug!(self, "handle_read_single_register: regno={}", regno);

        let result = {
            let reg = &mut self.scratch_a;
            target.read_register(regno, reg)
        };
        let len = match self.handle_target_err_core(target, result)? {
            Some(len) => len,
            None => return Ok(ProcessResult::None),
        };
        let out_cap = self.out_payload_cap();
        let expected_hex_len = len.saturating_mul(2);
        if expected_hex_len > out_cap {
            gdb_debug!(
                self,
                "handle_read_single_register: response too large regno={} hex_len={} out_cap={}",
                regno,
                expected_hex_len,
                out_cap
            );
            self.send::<T>(b"E01")?;
            return Ok(ProcessResult::None);
        }
        let hex_len = {
            let out = &mut self.scratch_b[..out_cap];
            let reg = &self.scratch_a[..len];
            hex_encode(reg, out)
        };
        if hex_len < expected_hex_len {
            gdb_debug!(
                self,
                "handle_read_single_register: hex_encode truncated regno={} expected={} actual={}",
                regno,
                expected_hex_len,
                hex_len
            );
            self.send::<T>(b"E01")?;
            return Ok(ProcessResult::None);
        }
        self.send_scratch_b::<T>(hex_len)?;
        Ok(ProcessResult::None)
    }

    fn handle_write_single_register<T: Target>(
        &mut self,
        target: &mut T,
        payload: &[u8],
    ) -> Result<ProcessResult, GdbServerError<T>> {
        let body = &payload[1..];
        let Some(eq_pos) = body.iter().position(|&b| b == b'=') else {
            gdb_debug!(self, "handle_write_single_register: missing '=' separator");
            self.send::<T>(b"E01")?;
            return Ok(ProcessResult::None);
        };
        let regno_hex = &body[..eq_pos];
        let val_hex = &body[eq_pos + 1..];

        let regno = match parse_hex_u32(regno_hex) {
            Some(r) => r,
            None => {
                gdb_debug!(
                    self,
                    "handle_write_single_register: bad regno payload=\"{}\"",
                    debug_printable_prefix(regno_hex)
                );
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            }
        };

        gdb_debug!(
            self,
            "handle_write_single_register: regno={} val_hex_len={}",
            regno,
            val_hex.len()
        );

        let len = match {
            let reg = &mut self.scratch_a;
            hex_decode(val_hex, reg)
        } {
            Ok(len) => len,
            Err(_) => {
                gdb_debug!(
                    self,
                    "handle_write_single_register: hex decode failed src_len={} dst_cap={}",
                    val_hex.len(),
                    self.scratch_a.len()
                );
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            }
        };

        gdb_debug!(
            self,
            "handle_write_single_register: decoded_len={} regno={}",
            len,
            regno
        );
        let result = target.write_register(regno, &self.scratch_a[..len]);
        if self.handle_target_err_core(target, result)?.is_none() {
            return Ok(ProcessResult::None);
        }
        self.send_ok::<T>()?;
        Ok(ProcessResult::None)
    }

    fn handle_read_memory<T: Target>(
        &mut self,
        target: &mut T,
        payload: &[u8],
    ) -> Result<ProcessResult, GdbServerError<T>> {
        let (addr, len) =
            match parse_addr_len::<T::RecoverableError, T::UnrecoverableError>(payload) {
                Ok(v) => v,
                Err(_) => {
                    gdb_debug!(self, "handle_read_memory: parse_addr_len failed");
                    self.send::<T>(b"E02")?;
                    return Ok(ProcessResult::None);
                }
            };
        gdb_debug!(self, "handle_read_memory: addr=0x{:x} len={}", addr, len);
        if addr.checked_add(len).is_none() {
            // Workaround for a GDB bug that can send wrapped "m" packets like "$mfffffffffffffffc,4#...".
            // If the address+length overflows, treat it as EFAULT (14) and keep the session running.
            gdb_debug!(self, "handle_read_memory: overflow addr+len -> E14");
            self.send::<T>(b"E14")?;
            return Ok(ProcessResult::None);
        }
        let len_usize = match usize::try_from(len) {
            Ok(v) => v,
            Err(_) => {
                gdb_debug!(self, "handle_read_memory: length conversion failed");
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            }
        };
        let out_cap = self.out_payload_cap();
        if len_usize > out_cap {
            gdb_debug!(
                self,
                "handle_read_memory: len_usize {} exceeds out_cap {}, sending E01",
                len_usize,
                out_cap
            );
            self.send::<T>(b"E01")?;
            return Ok(ProcessResult::None);
        }

        let expected_hex_len = len_usize.saturating_mul(2);
        if expected_hex_len > out_cap {
            gdb_debug!(
                self,
                "handle_read_memory: hex response too large hex_len={} out_cap={}",
                expected_hex_len,
                out_cap
            );
            self.send::<T>(b"E01")?;
            return Ok(ProcessResult::None);
        }

        let result = {
            let data = &mut self.scratch_a;
            target.read_memory(addr, &mut data[..len_usize])
        };
        if self.handle_target_err_core(target, result)?.is_none() {
            return Ok(ProcessResult::None);
        }

        let hex_len = {
            let out = &mut self.scratch_b[..out_cap];
            let data = &self.scratch_a[..len_usize];
            hex_encode(data, out)
        };
        if hex_len < expected_hex_len {
            gdb_debug!(
                self,
                "handle_read_memory: hex_encode truncated expected={} actual={}",
                expected_hex_len,
                hex_len
            );
            self.send::<T>(b"E01")?;
            return Ok(ProcessResult::None);
        }
        self.send_scratch_b::<T>(hex_len)?;
        Ok(ProcessResult::None)
    }

    fn handle_write_memory_hex<T: Target>(
        &mut self,
        target: &mut T,
        payload: &[u8],
    ) -> Result<ProcessResult, GdbServerError<T>> {
        let Some(colon) = payload.iter().position(|&b| b == b':') else {
            gdb_debug!(self, "handle_write_memory_hex: missing ':' separator");
            self.send::<T>(b"E02")?;
            return Ok(ProcessResult::None);
        };
        let header = &payload[..colon];
        let data_hex = &payload[colon + 1..];

        let (addr, len) = match parse_addr_len::<T::RecoverableError, T::UnrecoverableError>(header)
        {
            Ok(v) => v,
            Err(_) => {
                gdb_debug!(self, "handle_write_memory_hex: parse_addr_len failed");
                self.send::<T>(b"E02")?;
                return Ok(ProcessResult::None);
            }
        };
        gdb_debug!(
            self,
            "handle_write_memory_hex: addr=0x{:x} len={}",
            addr,
            len
        );

        let decoded = match {
            let data = &mut self.scratch_a;
            hex_decode(data_hex, data)
        } {
            Ok(len) => len,
            Err(_) => {
                gdb_debug!(
                    self,
                    "handle_write_memory_hex: hex decode failed src_len={} dst_cap={}",
                    data_hex.len(),
                    self.scratch_a.len()
                );
                self.send::<T>(b"E03")?;
                return Ok(ProcessResult::None);
            }
        };
        if decoded as u64 != len {
            gdb_debug!(
                self,
                "handle_write_memory_hex: decoded_len {} != header_len {}",
                decoded,
                len
            );
            self.send::<T>(b"E03")?;
            return Ok(ProcessResult::None);
        }

        let result = target.write_memory(addr, &self.scratch_a[..decoded]);
        if self.handle_target_err_core(target, result)?.is_none() {
            return Ok(ProcessResult::None);
        }
        self.send_ok::<T>()?;
        Ok(ProcessResult::None)
    }

    fn handle_write_memory_binary<T: Target>(
        &mut self,
        target: &mut T,
        payload: &[u8],
    ) -> Result<ProcessResult, GdbServerError<T>> {
        let Some(colon) = payload.iter().position(|&b| b == b':') else {
            gdb_debug!(self, "handle_write_memory_binary: missing ':' separator");
            self.send::<T>(b"E02")?;
            return Ok(ProcessResult::None);
        };
        let header = &payload[..colon];
        let binary = &payload[colon + 1..];

        // RSP "X" packet: X<addr_hex>,<len_hex>:<binary-data>.
        // Addresses and lengths are hex per the GDB RSP overview.
        let (addr, len) = match parse_addr_len::<T::RecoverableError, T::UnrecoverableError>(header)
        {
            Ok(v) => v,
            Err(_) => {
                gdb_debug!(self, "handle_write_memory_binary: parse_addr_len failed");
                self.send::<T>(b"E02")?;
                return Ok(ProcessResult::None);
            }
        };
        gdb_debug!(
            self,
            "handle_write_memory_binary: addr=0x{:x} len={}",
            addr,
            len
        );

        let len_usize = match usize::try_from(len) {
            Ok(value) => value,
            Err(_) => {
                gdb_debug!(self, "handle_write_memory_binary: len overflow");
                self.send::<T>(b"E03")?;
                return Ok(ProcessResult::None);
            }
        };

        // Decode RSP binary data after checksum validation. The on-wire payload uses 0x7d
        // escaping (no RLE expansion for incoming data).
        if len_usize > self.scratch_a.len() {
            gdb_debug!(
                self,
                "handle_write_memory_binary: len {} exceeds MAX_PKT {}",
                len_usize,
                self.scratch_a.len()
            );
            self.send::<T>(b"E03")?;
            return Ok(ProcessResult::None);
        }
        let decoded_len = match {
            let decoded = &mut self.scratch_a;
            decode_rsp_binary(binary, &mut decoded[..len_usize])
        } {
            Ok(len) => len,
            Err(_) => {
                // Malformed escape or output buffer overflow.
                gdb_debug!(
                    self,
                    "handle_write_memory_binary: binary decode failed src_len={} dst_cap={}",
                    binary.len(),
                    len_usize
                );
                self.send::<T>(b"E03")?;
                return Ok(ProcessResult::None);
            }
        };

        if decoded_len != len_usize {
            gdb_debug!(
                self,
                "handle_write_memory_binary: decoded_len {} != header_len {}",
                decoded_len,
                len_usize
            );
            self.send::<T>(b"E03")?;
            return Ok(ProcessResult::None);
        }

        let result = target.write_memory(addr, &self.scratch_a[..decoded_len]);
        if self.handle_target_err_core(target, result)?.is_none() {
            return Ok(ProcessResult::None);
        }
        self.send_ok::<T>()?;
        Ok(ProcessResult::None)
    }

    fn handle_breakpoint<T: Target>(
        &mut self,
        target: &mut T,
        payload: &[u8],
        insert: bool,
    ) -> Result<ProcessResult, GdbServerError<T>> {
        let caps = target.capabilities();
        let body = payload.get(1..).unwrap_or(&[]);
        let mut parts = body.splitn(3, |&b| b == b',');
        let type_bytes = parts.next().unwrap_or(&[]);
        let addr_hex = parts.next().unwrap_or(&[]);
        let kind_hex = parts.next().unwrap_or(&[]);

        if type_bytes.is_empty() || addr_hex.is_empty() || kind_hex.is_empty() {
            self.send::<T>(b"E02")?;
            return Ok(ProcessResult::None);
        }

        let Some(bp_type) = parse_dec_u8(type_bytes) else {
            self.send::<T>(b"E02")?;
            return Ok(ProcessResult::None);
        };
        let Some(addr) = parse_hex_u64(addr_hex) else {
            self.send::<T>(b"E02")?;
            return Ok(ProcessResult::None);
        };
        let Some(len_or_kind) = parse_hex_u64(kind_hex) else {
            self.send::<T>(b"E02")?;
            return Ok(ProcessResult::None);
        };

        let result = match bp_type {
            0 => {
                if !caps.contains(TargetCapabilities::SW_BREAK) {
                    self.send_empty::<T>()?;
                    return Ok(ProcessResult::None);
                }
                if insert {
                    target.insert_sw_breakpoint(addr)
                } else {
                    target.remove_sw_breakpoint(addr)
                }
            }
            1 => {
                if !caps.contains(TargetCapabilities::HW_BREAK) {
                    self.send_empty::<T>()?;
                    return Ok(ProcessResult::None);
                }
                if insert {
                    target.insert_hw_breakpoint(addr, len_or_kind)
                } else {
                    target.remove_hw_breakpoint(addr, len_or_kind)
                }
            }
            2 => {
                if !caps.contains(TargetCapabilities::WATCH_W) {
                    self.send_empty::<T>()?;
                    return Ok(ProcessResult::None);
                }
                if insert {
                    target.insert_watchpoint(WatchpointKind::Write, addr, len_or_kind)
                } else {
                    target.remove_watchpoint(WatchpointKind::Write, addr, len_or_kind)
                }
            }
            3 => {
                if !caps.contains(TargetCapabilities::WATCH_R) {
                    self.send_empty::<T>()?;
                    return Ok(ProcessResult::None);
                }
                if insert {
                    target.insert_watchpoint(WatchpointKind::Read, addr, len_or_kind)
                } else {
                    target.remove_watchpoint(WatchpointKind::Read, addr, len_or_kind)
                }
            }
            4 => {
                if !caps.contains(TargetCapabilities::WATCH_A) {
                    self.send_empty::<T>()?;
                    return Ok(ProcessResult::None);
                }
                if insert {
                    target.insert_watchpoint(WatchpointKind::Access, addr, len_or_kind)
                } else {
                    target.remove_watchpoint(WatchpointKind::Access, addr, len_or_kind)
                }
            }
            _ => {
                self.send_empty::<T>()?;
                return Ok(ProcessResult::None);
            }
        };

        if self.handle_target_err_optional(target, result)? {
            return Ok(ProcessResult::None);
        }

        self.send_ok::<T>()?;
        Ok(ProcessResult::None)
    }

    fn handle_continue<T: Target>(
        &mut self,
        payload: &[u8],
    ) -> Result<ProcessResult, GdbServerError<T>> {
        let new_pc = if payload.len() > 1 {
            match parse_hex_u64(&payload[1..]) {
                Some(addr) => Some(addr),
                None => {
                    gdb_debug!(self, "handle_continue: bad pc payload");
                    self.send::<T>(b"E01")?;
                    return Ok(ProcessResult::None);
                }
            }
        } else {
            None
        };
        gdb_debug!(self, "handle_continue: Resume Continue pc={:#x?}", new_pc);
        Ok(ProcessResult::Resume(ResumeAction::Continue(new_pc)))
    }

    fn handle_step<T: Target>(
        &mut self,
        payload: &[u8],
    ) -> Result<ProcessResult, GdbServerError<T>> {
        let new_pc = if payload.len() > 1 {
            match parse_hex_u64(&payload[1..]) {
                Some(addr) => Some(addr),
                None => {
                    gdb_debug!(self, "handle_step: bad pc payload");
                    self.send::<T>(b"E01")?;
                    return Ok(ProcessResult::None);
                }
            }
        } else {
            None
        };
        gdb_debug!(self, "handle_step: Resume Step pc={:#x?}", new_pc);
        Ok(ProcessResult::Resume(ResumeAction::Step(new_pc)))
    }

    fn handle_v_packet<T: Target>(
        &mut self,
        target: &mut T,
        payload: &[u8],
    ) -> Result<ProcessResult, GdbServerError<T>> {
        gdb_debug!(
            self,
            "handle_v_packet: payload=\"{}\"",
            debug_printable_prefix(payload)
        );
        if payload == b"vMustReplyEmpty" {
            self.send_empty::<T>()?;
            return Ok(ProcessResult::None);
        }
        // GDB sends `vKill;...` during shutdown (`quit`). Reply OK so GDB can complete
        // teardown. If `monitor exit 0` was armed, finish the harness now.
        if payload.starts_with(b"vKill") {
            gdb_debug!(self, "handle_v_packet: vKill");
            self.send_ok::<T>()?;
            if self.monitor_exit_armed {
                self.monitor_exit_armed = false;
                return Ok(ProcessResult::MonitorExit);
            }
            return Ok(ProcessResult::None);
        }
        if payload == b"vCont?" {
            if target.capabilities().contains(TargetCapabilities::VCONT) {
                self.send::<T>(b"vCont;c;s")?;
            } else {
                self.send_empty::<T>()?;
            }
            return Ok(ProcessResult::None);
        }

        if let Some(rest) = payload.strip_prefix(b"vCont;") {
            if !target.capabilities().contains(TargetCapabilities::VCONT) {
                self.send_empty::<T>()?;
                return Ok(ProcessResult::None);
            }
            let action = rest.split(|&b| b == b';').next().unwrap_or(&[]);
            let Some((&action_byte, action_tail)) = action.split_first() else {
                self.send_empty::<T>()?;
                return Ok(ProcessResult::None);
            };
            if !action_tail.is_empty() {
                if action_tail[0] != b':' {
                    self.send::<T>(b"E01")?;
                    return Ok(ProcessResult::None);
                }
            }
            return match action_byte {
                b'c' => Ok(ProcessResult::Resume(ResumeAction::Continue(None))),
                b's' => Ok(ProcessResult::Resume(ResumeAction::Step(None))),
                _ => {
                    self.send_empty::<T>()?;
                    Ok(ProcessResult::None)
                }
            };
        }

        if payload == b"vFlashDone" {
            gdb_debug!(self, "handle_v_packet: vFlashDone");
            self.send_ok::<T>()?;
            return Ok(ProcessResult::None);
        }

        if let Some(rest) = payload.strip_prefix(b"vFlashErase:") {
            if let Some((addr, len)) = parse_flash_header(rest) {
                gdb_debug!(
                    self,
                    "handle_v_packet: vFlashErase addr=0x{:x} len={}",
                    addr,
                    len
                );
                self.send_ok::<T>()?;
            } else {
                gdb_debug!(self, "handle_v_packet: vFlashErase parse failed");
                self.send::<T>(b"E02")?;
            }
            return Ok(ProcessResult::None);
        }

        if let Some(rest) = payload.strip_prefix(b"vFlashWrite:") {
            let Some(colon) = rest.iter().position(|&b| b == b':') else {
                gdb_debug!(self, "handle_v_packet: vFlashWrite missing ':'");
                self.send::<T>(b"E02")?;
                return Ok(ProcessResult::None);
            };
            let header = &rest[..colon];
            let data = &rest[colon + 1..];

            let Some((addr, len)) = parse_flash_header(header) else {
                gdb_debug!(self, "handle_v_packet: vFlashWrite parse failed");
                self.send::<T>(b"E02")?;
                return Ok(ProcessResult::None);
            };
            gdb_debug!(
                self,
                "handle_v_packet: vFlashWrite addr=0x{:x} len={}",
                addr,
                len
            );

            if len as usize > MAX_PKT {
                gdb_debug!(
                    self,
                    "handle_v_packet: vFlashWrite len {} exceeds MAX_PKT {}",
                    len,
                    MAX_PKT
                );
                self.send::<T>(b"E01")?;
                return Ok(ProcessResult::None);
            }

            let decoded_len = match {
                let decoded = &mut self.scratch_a;
                decode_rsp_binary(data, decoded)
            } {
                Ok(len) => len,
                Err(_) => {
                    gdb_debug!(
                        self,
                        "handle_v_packet: vFlashWrite decode failed src_len={} dst_cap={}",
                        data.len(),
                        self.scratch_a.len()
                    );
                    self.send::<T>(b"E03")?;
                    return Ok(ProcessResult::None);
                }
            };

            if decoded_len as u64 != len {
                gdb_debug!(
                    self,
                    "handle_v_packet: vFlashWrite decoded_len {} != header_len {}",
                    decoded_len,
                    len
                );
                self.send::<T>(b"E03")?;
                return Ok(ProcessResult::None);
            }

            let result = target.write_memory(addr, &self.scratch_a[..decoded_len]);
            if self.handle_target_err_core(target, result)?.is_none() {
                return Ok(ProcessResult::None);
            }
            self.send_ok::<T>()?;
            return Ok(ProcessResult::None);
        }

        self.send_empty::<T>()?;
        Ok(ProcessResult::None)
    }
}

impl<T: Target, const MAX_PKT: usize, const TX_CAP: usize> RspIrqEndpoint<T>
    for GdbServer<MAX_PKT, TX_CAP>
{
    fn on_rx_byte_irq(
        &mut self,
        target: &mut T,
        byte: u8,
    ) -> Result<ProcessResult, GdbServerError<T>> {
        GdbServer::on_rx_byte_irq(self, target, byte)
    }

    fn pop_tx_byte_irq(&mut self) -> Option<u8> {
        GdbServer::pop_tx_byte_irq(self)
    }

    fn has_tx_pending(&self) -> bool {
        GdbServer::has_tx_pending(self)
    }
}

#[cfg(any(feature = "gdb_monitor_debug", test))]
struct DebugBufWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
    cap: usize,
}

#[cfg(any(feature = "gdb_monitor_debug", test))]
impl<'a> DebugBufWriter<'a> {
    fn new(buf: &'a mut [u8], cap: usize) -> Self {
        Self { buf, pos: 0, cap }
    }

    fn len(&self) -> usize {
        self.pos
    }
}

#[cfg(any(feature = "gdb_monitor_debug", test))]
impl<'a> fmt::Write for DebugBufWriter<'a> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.cap == 0 {
            return Ok(());
        }

        let mut bytes = s.as_bytes();
        while !bytes.is_empty() {
            let remaining = self.cap.saturating_sub(self.pos);
            if remaining == 0 {
                break;
            }
            let to_copy = bytes.len().min(remaining);
            self.buf[self.pos..self.pos + to_copy].copy_from_slice(&bytes[..to_copy]);
            self.pos += to_copy;
            bytes = &bytes[to_copy..];
            if self.pos == self.cap {
                break;
            }
        }
        Ok(())
    }
}

const DEBUG_PRINTABLE_BUF: usize = 64;
const DEBUG_PRINTABLE_TRUNC: usize = 48;

struct DebugPrintable {
    buf: [u8; DEBUG_PRINTABLE_BUF],
    len: usize,
}

impl DebugPrintable {
    fn new(data: &[u8]) -> Self {
        let mut buf = [0u8; DEBUG_PRINTABLE_BUF];
        let mut len = 0usize;
        let limit = DEBUG_PRINTABLE_TRUNC.min(DEBUG_PRINTABLE_BUF);

        for &b in data.iter().take(limit) {
            if len >= buf.len() {
                break;
            }
            buf[len] = match b {
                0x20..=0x7e => b,
                _ => b'.',
            };
            len += 1;
        }

        if data.len() > limit {
            for &ch in b"..." {
                if len >= buf.len() {
                    break;
                }
                buf[len] = ch;
                len += 1;
            }
        }

        Self { buf, len }
    }
}

fn debug_printable_prefix(data: &[u8]) -> DebugPrintable {
    DebugPrintable::new(data)
}

impl fmt::Display for DebugPrintable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Buffer only contains printable ASCII.
        // SAFETY: DebugPrintable::new only writes ASCII bytes into the buffer.
        let s = unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) };
        f.write_str(s)
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn from_hex_digit(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(10 + b - b'a'),
        b'A'..=b'F' => Ok(10 + b - b'A'),
        _ => Err(()),
    }
}

fn parse_hex_u64(buf: &[u8]) -> Option<u64> {
    if buf.is_empty() {
        return None;
    }
    let mut val: u64 = 0;
    for &b in buf {
        let digit = from_hex_digit(b).ok()? as u64;
        val = val.checked_mul(16)?;
        val = val.checked_add(digit)?;
    }
    Some(val)
}

fn parse_hex_u32(buf: &[u8]) -> Option<u32> {
    parse_hex_u64(buf).and_then(|v| u32::try_from(v).ok())
}

fn parse_dec_u8(buf: &[u8]) -> Option<u8> {
    parse_dec_u64(buf).and_then(|v| u8::try_from(v).ok())
}

fn parse_dec_u64(buf: &[u8]) -> Option<u64> {
    if buf.is_empty() {
        return None;
    }
    let mut val: u64 = 0;
    for &b in buf {
        if !(b'0'..=b'9').contains(&b) {
            return None;
        }
        val = val.checked_mul(10)?;
        val = val.checked_add((b - b'0') as u64)?;
    }
    Some(val)
}

fn parse_hex_or_dec_u64(buf: &[u8]) -> Option<u64> {
    if buf.is_empty() {
        return None;
    }
    let has_alpha = buf.iter().any(|&b| matches!(b, b'a'..=b'f' | b'A'..=b'F'));
    if has_alpha {
        return parse_hex_u64(buf);
    }
    parse_hex_u64(buf).or_else(|| parse_dec_u64(buf))
}

fn parse_signed_i64(buf: &[u8]) -> Option<i64> {
    if buf.is_empty() {
        return None;
    }
    let (neg, rest) = if buf[0] == b'-' {
        (true, &buf[1..])
    } else {
        (false, buf)
    };
    if rest.is_empty() {
        return None;
    }
    let unsigned = parse_hex_or_dec_u64(rest)? as i128;
    let value = if neg { -unsigned } else { unsigned };
    if value < i64::MIN as i128 || value > i64::MAX as i128 {
        return None;
    }
    Some(value as i64)
}

fn parse_fileio_reply(payload: &[u8]) -> Option<(i64, i32, bool)> {
    let body = payload.strip_prefix(b"F")?;
    let mut parts = body.split(|&b| b == b',');
    let retcode = parse_signed_i64(parts.next()?)?;
    let errno = match parts.next() {
        Some(part) if !part.is_empty() => {
            let val = parse_signed_i64(part)?;
            if val < i32::MIN as i64 || val > i32::MAX as i64 {
                return None;
            }
            val as i32
        }
        Some(_) => return None,
        None => 0,
    };
    let ctrl_c = match parts.next() {
        Some(part) if !part.is_empty() => parse_signed_i64(part)? != 0,
        Some(_) => return None,
        None => false,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((retcode, errno, ctrl_c))
}

fn parse_flash_header(buf: &[u8]) -> Option<(u64, u64)> {
    let mut parts = buf.splitn(2, |&b| b == b',');
    let addr = parse_hex_u64(parts.next()?)?;
    let len = parse_hex_u64(parts.next()?)?;
    Some((addr, len))
}

fn parse_qxfer_read(buf: &[u8], allow_empty_annex: bool) -> Option<(&[u8], u64, u64)> {
    let mut parts = buf.splitn(2, |&b| b == b':');
    let annex = parts.next()?;
    let rest = parts.next()?;

    if annex.is_empty() && !allow_empty_annex {
        return None;
    }

    let mut range = rest.splitn(2, |&b| b == b',');
    let offset_hex = range.next()?;
    let len_hex = range.next()?;
    let offset = parse_hex_u64(offset_hex)?;
    let len = parse_hex_u64(len_hex)?;
    Some((annex, offset, len))
}

fn qsupported_has_xml_registers(payload: &[u8], arch: &[u8]) -> bool {
    let Some(rest) = payload.strip_prefix(b"qSupported") else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(b":") else {
        return false;
    };

    for item in rest.split(|&b| b == b';') {
        let mut parts = item.splitn(2, |&b| b == b'=');
        let key = parts.next().unwrap_or(&[]);
        if key != b"xmlRegisters" {
            continue;
        }
        let list = parts.next().unwrap_or(&[]);
        for value in list.split(|&b| b == b',') {
            if value == arch {
                return true;
            }
        }
    }
    false
}

fn parse_addr_len<R, U>(payload: &[u8]) -> Result<(u64, u64), GdbError<R, U>> {
    let body = payload.get(1..).ok_or(GdbError::MalformedPacket)?;
    let Some(comma) = body.iter().position(|&b| b == b',') else {
        return Err(GdbError::MalformedPacket);
    };
    let addr_hex = &body[..comma];
    let len_hex = &body[comma + 1..];
    let addr = parse_hex_u64(addr_hex).ok_or(GdbError::MalformedPacket)?;
    let len = parse_hex_u64(len_hex).ok_or(GdbError::MalformedPacket)?;
    Ok((addr, len))
}

fn append_bytes(buf: &mut [u8], idx: &mut usize, src: &[u8]) {
    let end = idx.saturating_add(src.len());
    if end > buf.len() {
        return;
    }
    buf[*idx..end].copy_from_slice(src);
    *idx = end;
}

fn append_hex_u64(buf: &mut [u8], idx: &mut usize, mut val: u64) {
    let mut tmp = [0u8; 16];
    let mut len = 0usize;

    if val == 0 {
        tmp[0] = b'0';
        len = 1;
    } else {
        while val != 0 {
            let digit = (val & 0xF) as usize;
            tmp[len] = HEX[digit];
            len += 1;
            val >>= 4;
        }
    }

    let end = idx.saturating_add(len);
    if end > buf.len() {
        return;
    }

    for i in 0..len {
        buf[*idx + i] = tmp[len - 1 - i];
    }
    *idx = end;
}

fn ensure_trailing_newline(buf: &mut [u8], len: &mut usize) {
    if *len == 0 {
        return;
    }
    let last = len.saturating_sub(1);
    if buf[last] == b'\n' {
        return;
    }
    if *len < buf.len() {
        buf[*len] = b'\n';
        *len += 1;
    } else {
        buf[last] = b'\n';
    }
}

fn hex_encode(src: &[u8], dst: &mut [u8]) -> usize {
    let mut idx = 0usize;
    for &b in src {
        if idx + 2 > dst.len() {
            break;
        }
        dst[idx] = HEX[(b >> 4) as usize];
        dst[idx + 1] = HEX[(b & 0xF) as usize];
        idx += 2;
    }
    idx
}

/// Decode ASCII hex in `src` into raw bytes in `dst`.
/// Returns decoded length on success.
pub fn hex_decode(src: &[u8], dst: &mut [u8]) -> Result<usize, ()> {
    if src.len() % 2 != 0 {
        return Err(());
    }
    let mut out = 0usize;
    for chunk in src.chunks_exact(2) {
        if out >= dst.len() {
            return Err(());
        }
        let hi = from_hex_digit(chunk[0])?;
        let lo = from_hex_digit(chunk[1])?;
        dst[out] = (hi << 4) | lo;
        out += 1;
    }
    Ok(out)
}

/// Decode RSP binary data (with 0x7d escaping) into `dst`.
/// Per the GDB RSP overview, '#' '$' '}' are escaped by prefixing '}' and XOR 0x20.
/// Returns decoded length on success.
pub fn decode_rsp_binary(src: &[u8], dst: &mut [u8]) -> Result<usize, ()> {
    let mut in_idx = 0usize;
    let mut out_idx = 0usize;

    while in_idx < src.len() {
        let b = src[in_idx];
        if b == b'}' {
            in_idx += 1;
            if in_idx >= src.len() {
                return Err(());
            }
            let val = src[in_idx] ^ 0x20;
            if out_idx >= dst.len() {
                return Err(());
            }
            dst[out_idx] = val;
            in_idx += 1;
            out_idx += 1;
            continue;
        } else {
            if out_idx >= dst.len() {
                return Err(());
            }
            dst[out_idx] = b;
        }

        in_idx += 1;
        out_idx += 1;
    }

    Ok(out_idx)
}

/// Encode RSP binary data (with 0x7d escaping) into `dst`.
/// Returns (bytes_consumed, bytes_written).
fn encode_rsp_binary(src: &[u8], dst: &mut [u8]) -> (usize, usize) {
    let mut in_idx = 0usize;
    let mut out_idx = 0usize;

    while in_idx < src.len() {
        let b = src[in_idx];
        // Escape '$', '#', '}' per RSP; escape '*' to avoid triggering GDB RLE parsing.
        let needs_escape = b == b'$' || b == b'#' || b == b'}' || b == b'*';
        let needed = if needs_escape { 2 } else { 1 };

        if out_idx + needed > dst.len() {
            break;
        }

        if needs_escape {
            dst[out_idx] = b'}';
            dst[out_idx + 1] = b ^ 0x20;
            out_idx += 2;
        } else {
            dst[out_idx] = b;
            out_idx += 1;
        }

        in_idx += 1;
    }

    (in_idx, out_idx)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::GdbServer;
    use super::HEX;
    use super::ProcessResult;
    use super::Target;
    use super::TargetError;
    use super::WatchpointKind;
    use super::parse_dec_u8;
    use core::convert::Infallible;
    use core::task::Context;
    use core::task::Poll;
    use io_api::stream::PollByteStream;
    use std::vec::Vec;

    const REG_BYTES: usize = 356;

    struct DummyTarget;

    type DummyError = TargetError<Infallible, Infallible>;

    impl Target for DummyTarget {
        type RecoverableError = Infallible;
        type UnrecoverableError = Infallible;

        fn read_registers(&mut self, dst: &mut [u8]) -> Result<usize, DummyError> {
            if dst.len() < REG_BYTES {
                return Err(TargetError::NotSupported);
            }
            for (idx, byte) in dst[..REG_BYTES].iter_mut().enumerate() {
                *byte = (idx & 0xff) as u8;
            }
            Ok(REG_BYTES)
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

    struct BufferedStream {
        rx: Vec<u8>,
        rx_pos: usize,
        tx: Vec<u8>,
    }

    impl PollByteStream for BufferedStream {
        type Error = Infallible;

        fn poll_read(
            &mut self,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<Result<usize, Self::Error>> {
            if self.rx_pos == self.rx.len() {
                return Poll::Pending;
            }
            let len = core::cmp::min(buf.len(), self.rx.len() - self.rx_pos);
            buf[..len].copy_from_slice(&self.rx[self.rx_pos..self.rx_pos + len]);
            self.rx_pos += len;
            Poll::Ready(Ok(len))
        }

        fn poll_write(
            &mut self,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, Self::Error>> {
            self.tx.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn build_frame(payload: &[u8]) -> Vec<u8> {
        let checksum = payload.iter().fold(0u8, |sum, &b| sum.wrapping_add(b));
        let mut frame = Vec::new();
        frame.push(b'$');
        frame.extend_from_slice(payload);
        frame.push(b'#');
        frame.push(HEX[(checksum >> 4) as usize]);
        frame.push(HEX[(checksum & 0xF) as usize]);
        frame
    }

    fn is_hex(b: u8) -> bool {
        matches!(b, b'0'..=b'9' | b'a'..=b'f')
    }

    struct Packet {
        payload: Vec<u8>,
        checksum: [u8; 2],
    }

    fn parse_packets(tx: &[u8]) -> Vec<Packet> {
        let mut packets = Vec::new();
        let mut idx = 0usize;
        while idx < tx.len() {
            if tx[idx] != b'$' {
                idx += 1;
                continue;
            }
            idx = idx.saturating_add(1);
            let start = idx;
            while idx < tx.len() && tx[idx] != b'#' {
                idx += 1;
            }
            if idx >= tx.len() {
                panic!("unterminated packet in tx stream");
            }
            let payload = tx[start..idx].to_vec();
            if idx + 2 >= tx.len() {
                panic!("missing checksum in tx stream");
            }
            let checksum = [tx[idx + 1], tx[idx + 2]];
            idx += 3;
            packets.push(Packet { payload, checksum });
        }
        packets
    }

    fn is_console_packet(packet: &Packet) -> bool {
        let Some((&b'O', rest)) = packet.payload.split_first() else {
            return false;
        };
        if rest.is_empty() || rest.len() % 2 != 0 {
            return false;
        }
        rest.iter().all(|&b| is_hex(b))
    }

    fn drain_tx<const MAX: usize, const TX: usize>(server: &mut GdbServer<MAX, TX>) -> Vec<u8> {
        let mut tx = Vec::new();
        while let Some(byte) = server.pop_tx_byte_irq() {
            tx.push(byte);
        }
        tx
    }

    fn build_qrcmd_frame(cmd: &[u8]) -> Vec<u8> {
        let mut hex = Vec::new();
        hex.resize(cmd.len().saturating_mul(2), 0u8);
        let hex_len = super::hex_encode(cmd, &mut hex);
        hex.truncate(hex_len);

        let mut payload = Vec::new();
        payload.extend_from_slice(b"qRcmd,");
        payload.extend_from_slice(&hex);
        build_frame(&payload)
    }

    #[test]
    fn g_packet_queues_register_reply() {
        let mut server: GdbServer<8192, 2048> = GdbServer::new();
        let mut target = DummyTarget;
        let frame = build_frame(b"g");

        let mut last = ProcessResult::None;
        for &byte in &frame {
            last = server
                .on_rx_byte_irq(&mut target, byte)
                .expect("rsp handling failed");
        }
        assert!(matches!(last, ProcessResult::None));

        let mut tx = Vec::new();
        while let Some(byte) = server.pop_tx_byte_irq() {
            tx.push(byte);
        }

        let packets = parse_packets(&tx);
        let packet = packets
            .iter()
            .find(|packet| {
                packet.payload.len() == REG_BYTES * 2 && packet.payload.iter().all(|&b| is_hex(b))
            })
            .expect("missing register response packet");

        let expected = packet
            .payload
            .iter()
            .fold(0u8, |sum, &b| sum.wrapping_add(b));
        let expected_hex = [
            HEX[(expected >> 4) as usize],
            HEX[(expected & 0xF) as usize],
        ];
        assert!(packet.checksum.iter().all(|&b| is_hex(b)));
        assert_eq!(packet.checksum, expected_hex);
    }

    enum MonitorReply {
        NotSupported,
        Output(&'static [u8]),
    }

    struct MonitorTarget {
        reply: MonitorReply,
    }

    impl MonitorTarget {
        fn new(reply: MonitorReply) -> Self {
            Self { reply }
        }
    }

    impl Target for MonitorTarget {
        type RecoverableError = Infallible;
        type UnrecoverableError = Infallible;

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

        fn monitor_command(&mut self, _cmd: &[u8], out: &mut [u8]) -> Result<usize, DummyError> {
            match self.reply {
                MonitorReply::NotSupported => Err(TargetError::NotSupported),
                MonitorReply::Output(data) => {
                    let len = data.len().min(out.len());
                    out[..len].copy_from_slice(&data[..len]);
                    Ok(len)
                }
            }
        }
    }

    #[test]
    fn qrcmd_not_supported_replies_empty() {
        let mut server: GdbServer<256, 256> = GdbServer::new();
        let mut target = MonitorTarget::new(MonitorReply::NotSupported);
        let frame = build_qrcmd_frame(b"hp memfault?");

        let mut last = ProcessResult::None;
        for &byte in &frame {
            last = server
                .on_rx_byte_irq(&mut target, byte)
                .expect("rsp handling failed");
        }
        assert!(matches!(last, ProcessResult::None));

        let tx = drain_tx(&mut server);
        let packets = parse_packets(&tx);
        assert!(packets.iter().any(|packet| packet.payload.is_empty()));
    }

    #[test]
    fn qrcmd_output_is_hex_encoded() {
        let mut server: GdbServer<256, 256> = GdbServer::new();
        let mut target = MonitorTarget::new(MonitorReply::Output(b"hi\n"));
        let frame = build_qrcmd_frame(b"hp memfault?");

        for &byte in &frame {
            server
                .on_rx_byte_irq(&mut target, byte)
                .expect("rsp handling failed");
        }

        let tx = drain_tx(&mut server);
        let packets = parse_packets(&tx);
        assert!(
            packets.iter().any(|packet| packet.payload == b"68690a"),
            "missing qRcmd output reply"
        );
    }

    #[test]
    fn decimal_u8_parser_checks_bounds() {
        assert_eq!(parse_dec_u8(b"0"), Some(0));
        assert_eq!(parse_dec_u8(b"255"), Some(u8::MAX));
        assert_eq!(parse_dec_u8(b"256"), None);
        assert_eq!(parse_dec_u8(b"18446744073709551616"), None);
    }

    #[test]
    fn fileio_reply_parses_and_queues_no_packet() {
        let mut server: GdbServer<256, 256> = GdbServer::new();
        let mut target = DummyTarget;
        let frame = build_frame(b"F0,0");

        let mut last = ProcessResult::None;
        for &byte in &frame {
            last = server
                .on_rx_byte_irq(&mut target, byte)
                .expect("rsp handling failed");
        }

        match last {
            ProcessResult::FileIoReply {
                retcode,
                errno,
                ctrl_c,
            } => {
                assert_eq!(retcode, 0);
                assert_eq!(errno, 0);
                assert!(!ctrl_c);
            }
            _ => panic!("expected file-io reply"),
        }

        let tx = drain_tx(&mut server);
        let packets = parse_packets(&tx)
            .into_iter()
            .filter(|packet| !is_console_packet(packet))
            .collect::<Vec<_>>();
        assert!(packets.is_empty());
    }

    #[test]
    fn fileio_reply_parses_negative_retcode() {
        let mut server: GdbServer<256, 256> = GdbServer::new();
        let mut target = DummyTarget;
        let frame = build_frame(b"F-1,2");

        let mut last = ProcessResult::None;
        for &byte in &frame {
            last = server
                .on_rx_byte_irq(&mut target, byte)
                .expect("rsp handling failed");
        }

        match last {
            ProcessResult::FileIoReply {
                retcode,
                errno,
                ctrl_c,
            } => {
                assert_eq!(retcode, -1);
                assert_eq!(errno, 2);
                assert!(!ctrl_c);
            }
            _ => panic!("expected file-io reply"),
        }
    }

    #[test]
    fn fileio_reply_parses_retcode_only() {
        let mut server: GdbServer<256, 256> = GdbServer::new();
        let mut target = DummyTarget;
        let frame = build_frame(b"F1");

        let mut last = ProcessResult::None;
        for &byte in &frame {
            last = server
                .on_rx_byte_irq(&mut target, byte)
                .expect("rsp handling failed");
        }

        match last {
            ProcessResult::FileIoReply {
                retcode,
                errno,
                ctrl_c,
            } => {
                assert_eq!(retcode, 1);
                assert_eq!(errno, 0);
                assert!(!ctrl_c);
            }
            _ => panic!("expected file-io reply"),
        }
    }

    #[test]
    fn fileio_reply_malformed_is_ignored() {
        let mut server: GdbServer<256, 256> = GdbServer::new();
        let mut target = DummyTarget;
        let frame = build_frame(b"F,");

        let mut last = ProcessResult::None;
        for &byte in &frame {
            last = server
                .on_rx_byte_irq(&mut target, byte)
                .expect("rsp handling failed");
        }
        assert!(matches!(last, ProcessResult::None));
        assert!(server.take_fileio_parse_error());

        let tx = drain_tx(&mut server);
        let packets = parse_packets(&tx)
            .into_iter()
            .filter(|packet| !is_console_packet(packet))
            .collect::<Vec<_>>();
        assert!(packets.is_empty());
    }

    #[test]
    fn ensure_trailing_newline_no_change_when_present() {
        let mut buf = *b"hi\n";
        let mut len = buf.len();
        super::ensure_trailing_newline(&mut buf, &mut len);
        assert_eq!(len, 3);
        assert_eq!(&buf[..len], b"hi\n");
    }

    #[test]
    fn ensure_trailing_newline_appends_when_missing() {
        let mut buf = [0u8; 8];
        buf[..5].copy_from_slice(b"hello");
        let mut len = 5;
        super::ensure_trailing_newline(&mut buf, &mut len);
        assert_eq!(len, 6);
        assert_eq!(&buf[..len], b"hello\n");
    }

    #[test]
    fn ensure_trailing_newline_overwrites_when_full() {
        let mut buf = *b"hello";
        let mut len = buf.len();
        super::ensure_trailing_newline(&mut buf, &mut len);
        assert_eq!(len, 5);
        assert_eq!(&buf, b"hell\n");
    }

    #[test]
    fn ensure_trailing_newline_ignores_empty_output() {
        let mut buf = [0u8; 4];
        let mut len = 0;
        super::ensure_trailing_newline(&mut buf, &mut len);
        assert_eq!(len, 0);
        assert_eq!(&buf, &[0u8; 4]);
    }

    #[test]
    fn stop_reply_watchpoint_format() {
        let mut server: GdbServer<256, 256> = GdbServer::new();
        server
            .notify_stop_watch(WatchpointKind::Write, 0x1a2b)
            .expect("stop reply failed");

        let tx = drain_tx(&mut server);
        let packets = parse_packets(&tx);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].payload, b"T05watch:1a2b;");
    }

    #[test]
    fn stop_reply_signal_format() {
        let mut server: GdbServer<256, 256> = GdbServer::new();
        server.notify_stop_signal(2).expect("stop reply failed");

        let tx = drain_tx(&mut server);
        let packets = parse_packets(&tx);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].payload, b"S02");
    }

    #[test]
    fn stop_reply_query_uses_last_signal() {
        let mut server: GdbServer<256, 256> = GdbServer::new();
        let mut target = DummyTarget;
        server.set_last_stop_signal(2);

        let frame = build_frame(b"?");
        for &byte in &frame {
            server
                .on_rx_byte_irq(&mut target, byte)
                .expect("rsp handling failed");
        }

        let tx = drain_tx(&mut server);
        let packets = parse_packets(&tx)
            .into_iter()
            .filter(|packet| !is_console_packet(packet))
            .collect::<Vec<_>>();
        assert!(
            packets.iter().any(|packet| packet.payload == b"S02"),
            "missing S02 stop reply in '?' path"
        );
    }

    #[test]
    fn in_place_init_preserves_initial_sigtrap() {
        let mut slot = core::mem::MaybeUninit::<GdbServer<256, 256>>::uninit();
        GdbServer::init_in_place(&mut slot);
        // SAFETY: init_in_place initialized every field immediately above, and the
        // slot remains alive and immutably borrowed for this assertion.
        let server = unsafe { slot.assume_init_ref() };

        assert_eq!(server.last_stop.signal, 5);
    }

    #[test]
    fn monitor_exit_flushes_queued_replies() {
        let mut rx = build_qrcmd_frame(b"exit 0");
        rx.push(b'+');
        rx.extend_from_slice(&build_frame(b"vKill;1"));
        let mut stream = BufferedStream {
            rx,
            rx_pos: 0,
            tx: Vec::new(),
        };
        let mut server: GdbServer<256, 2048> = GdbServer::new();
        let mut target = DummyTarget;

        server
            .run_until_monitor_exit(&mut stream, &mut target)
            .expect("monitor exit failed");

        let packets = parse_packets(&stream.tx);
        assert_eq!(
            packets
                .iter()
                .filter(|packet| packet.payload == b"OK")
                .count(),
            2
        );
        assert_eq!(stream.tx.iter().filter(|&&byte| byte == b'+').count(), 2);
    }

    #[test]
    fn no_max_pkt_stack_buffers() {
        let src = include_str!("lib.rs");
        let needle = concat!("[0u8; ", "MAX_PKT]");
        assert!(
            !src.contains(needle),
            "gdb_remote/src/lib.rs should avoid MAX_PKT stack buffers"
        );
    }
}
