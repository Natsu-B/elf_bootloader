use arch_hal::aarch64_mutex::RawSpinLockIrqSave;
use arch_hal::timer;
use core::hint::spin_loop;
use core::mem::MaybeUninit;
use core::sync::atomic::Ordering;
use gdb_remote::RspFrameAssembler;
use gdb_remote::RspFrameEvent;
use mutex::pod::RawAtomicPod;

const RX_RING_SIZE: usize = 4096;
const PREFETCH_CAP: usize = 1024 + 4;
const NAK_ATTEMPTS: usize = 32;
const FLUSH_LIMIT: usize = RX_RING_SIZE;

// Publish debug entry state without relying on higher-level locking.
static DEBUG_ACTIVE: RawAtomicPod<bool> = unsafe { RawAtomicPod::new_raw_unchecked(false) };
// Set once a GDB session handshake has completed and we should emit async stop replies.
static DEBUG_SESSION_ACTIVE: RawAtomicPod<bool> = unsafe { RawAtomicPod::new_raw_unchecked(false) };
// Tracks whether we've completed an initial RSP handshake at least once.
static DEBUG_SESSION_INITIALIZED: RawAtomicPod<bool> =
    unsafe { RawAtomicPod::new_raw_unchecked(false) };
static STOP_LOOP_ACTIVE: RawAtomicPod<bool> = unsafe { RawAtomicPod::new_raw_unchecked(false) };
static ATTACH_REASON: RawAtomicPod<u8> = unsafe { RawAtomicPod::new_raw_unchecked(0) };

struct RxRing<const N: usize> {
    buf: [u8; N],
    head: usize,
    tail: usize,
}

impl<const N: usize> RxRing<N> {
    const fn new() -> Self {
        Self {
            buf: [0; N],
            head: 0,
            tail: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    fn push_drop_oldest(&mut self, byte: u8) -> bool {
        let next = (self.head + 1) % N;
        if next == self.tail {
            // Drop oldest to preserve the newest bytes for resync/attach.
            self.tail = (self.tail + 1) % N;
        }
        self.buf[self.head] = byte;
        self.head = next;
        true
    }

    fn pop(&mut self) -> Option<u8> {
        if self.is_empty() {
            return None;
        }
        let byte = self.buf[self.tail];
        self.tail = (self.tail + 1) % N;
        Some(byte)
    }

    fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
    }
}

struct GdbUartState<const N: usize> {
    rx: RxRing<N>,
    prefetch_buf: [u8; PREFETCH_CAP],
    prefetch_len: usize,
    prefetch_pos: usize,
}

// SAFETY: `GDB_UART_READY` publishes initialization completion with Release ordering.
static GDB_UART_READY: RawAtomicPod<bool> = unsafe { RawAtomicPod::new_raw_unchecked(false) };
// SAFETY: mutated only with interrupts disabled (IRQ-save lock).
static GDB_UART_STATE: RawSpinLockIrqSave<MaybeUninit<GdbUartState<RX_RING_SIZE>>> =
    RawSpinLockIrqSave::new(MaybeUninit::uninit());

/// Initializes the UDP-backed GDB UART state.
///
/// This keeps the same call signature as the PL011 backend; arguments are ignored.
pub fn init(_base: usize, _clock_hz: u64, _baud: u32) {
    // Fast-path: already initialized.
    if GDB_UART_READY.load(Ordering::Acquire) {
        return;
    }
    let mut guard = GDB_UART_STATE.lock_irqsave();
    if GDB_UART_READY.load(Ordering::Acquire) {
        return;
    }
    (&mut *guard).write(GdbUartState {
        rx: RxRing::new(),
        prefetch_buf: [0; PREFETCH_CAP],
        prefetch_len: 0,
        prefetch_pos: 0,
    });
    GDB_UART_READY.store(true, Ordering::Release);
}

/// Guard that keeps stop-loop state active until drop.
pub struct StopLoopGuard {
    restore: bool,
    pause_start_ticks: u64,
}

/// Begins a debug stop-loop section for attach/prefetch logic.
pub fn begin_stop_loop() -> StopLoopGuard {
    // Enforce nesting avoidance with one CAS RMW so only one core can mark stop-loop active.
    // RawAtomicPod becomes truly atomic only after `mutex::enable_raw_atomics()` during
    // single-core bring-up; SMP correctness requires that sequencing before secondaries start.
    if STOP_LOOP_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return StopLoopGuard {
            restore: false,
            pause_start_ticks: 0,
        };
    }
    let pause_start_ticks = timer::El2PhysicalTimer::new().now();

    if !GDB_UART_READY.load(Ordering::Acquire) {
        return StopLoopGuard {
            restore: true,
            pause_start_ticks,
        };
    }
    let mut guard = GDB_UART_STATE.lock_irqsave();
    // SAFETY: READY is published after full initialization with Release ordering.
    let state = unsafe { (&mut *guard).assume_init_mut() };
    state.prefetch_len = 0;
    state.prefetch_pos = 0;
    StopLoopGuard {
        restore: true,
        pause_start_ticks,
    }
}

impl Drop for StopLoopGuard {
    fn drop(&mut self) {
        if !self.restore {
            return;
        }
        let el2_timer = timer::El2PhysicalTimer::new();
        let now = el2_timer.now();
        let delta = now.wrapping_sub(self.pause_start_ticks);
        let old = el2_timer.lower_el_virtual_offset();
        // Compensate CNTVOFF_EL2 by the stop-loop duration so guest CNTVCT does not advance.
        let new = old.wrapping_add(delta);
        el2_timer.set_lower_el_virtual_offset(new);
        if GDB_UART_READY.load(Ordering::Acquire) {
            let mut guard = GDB_UART_STATE.lock_irqsave();
            // SAFETY: READY is published after full initialization with Release ordering.
            let state = unsafe { (&mut *guard).assume_init_mut() };
            state.rx.clear();
            state.prefetch_len = 0;
            state.prefetch_pos = 0;
        }
        STOP_LOOP_ACTIVE.store(false, Ordering::Release);
    }
}

/// Handles backend IRQ work.
///
/// For UDP transport this is equivalent to polling RX.
pub fn handle_irq() {
    poll_rx();
}

/// Polls UDP RX and drains bytes into the internal ring buffer.
pub fn poll_rx() {
    if !GDB_UART_READY.load(Ordering::Acquire) {
        return;
    }
    let mut guard = GDB_UART_STATE.lock_irqsave();
    // SAFETY: READY is published after full initialization with Release ordering.
    let state = unsafe { (&mut *guard).assume_init_mut() };
    crate::net::udp_uart::poll(
        |byte| {
            let _ = state.rx.push_drop_oldest(byte);
            record_attach_byte(byte);
        },
        |_byte| {},
    );
}

fn record_attach_byte(byte: u8) {
    // Breakpoint: attach byte detection (should be inactive after session init).
    if !DEBUG_SESSION_ACTIVE.load(Ordering::Acquire) {
        let reason = ATTACH_REASON.load(Ordering::Acquire);
        if byte == 0x03 {
            // Release publishes the attach request after RX buffering.
            if reason != 2 {
                ATTACH_REASON.store(2, Ordering::Release);
            }
        } else if byte == b'$' && reason == 0 {
            // Release publishes the attach request after RX buffering.
            ATTACH_REASON.store(1, Ordering::Release);
        }
    }
}

/// Returns whether the debugger is currently active.
pub fn is_debug_active() -> bool {
    // Acquire pairs with Release stores so debug entry sees any buffered bytes.
    DEBUG_ACTIVE.load(Ordering::Acquire)
}

/// Updates debug-active state.
pub fn set_debug_active(active: bool) {
    // Release publishes debug-active transitions to IRQ readers.
    DEBUG_ACTIVE.store(active, Ordering::Release);
}

/// Returns whether the GDB session handshake has completed.
pub fn is_debug_session_active() -> bool {
    DEBUG_SESSION_ACTIVE.load(Ordering::Acquire)
}

/// Updates GDB session-active state.
pub fn set_debug_session_active(active: bool) {
    DEBUG_SESSION_ACTIVE.store(active, Ordering::Release);
}

/// Returns whether the first session initialization has happened.
pub fn is_debug_session_initialized() -> bool {
    DEBUG_SESSION_INITIALIZED.load(Ordering::Acquire)
}

/// Updates one-time session-initialized state.
pub fn set_debug_session_initialized(active: bool) {
    DEBUG_SESSION_INITIALIZED.store(active, Ordering::Release);
}

/// Consumes and returns the current attach reason.
pub fn take_attach_reason() -> u8 {
    // AcqRel ensures we observe any IRQ-side buffering before consuming the reason.
    ATTACH_REASON.swap(0, Ordering::AcqRel)
}

/// Reads attach reason without consuming it.
pub fn peek_attach_reason() -> u8 {
    ATTACH_REASON.load(Ordering::Acquire)
}

/// Clears any buffered prefetch bytes.
pub fn clear_prefetch() {
    if !GDB_UART_READY.load(Ordering::Acquire) {
        return;
    }
    let mut guard = GDB_UART_STATE.lock_irqsave();
    // SAFETY: READY is published after full initialization with Release ordering.
    let state = unsafe { (&mut *guard).assume_init_mut() };
    state.prefetch_len = 0;
    state.prefetch_pos = 0;
}

/// Result of first-frame prefetch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrefetchResult {
    Success,
    Timeout,
    Overflow,
    Unavailable,
}

/// Prefetches the first incoming RSP frame for attach handling.
///
/// Waits for the first byte within `attach_deadline_ticks`, then extends the timeout
/// by `idle_timeout_ticks` as long as bytes continue arriving.
pub fn prefetch_first_rsp_frame(
    attach_deadline_ticks: u64,
    idle_timeout_ticks: u64,
) -> PrefetchResult {
    clear_prefetch();
    if !GDB_UART_READY.load(Ordering::Acquire) {
        return PrefetchResult::Unavailable;
    }

    let mut assembler = RspFrameAssembler::new();
    let mut len = 0usize;

    let el2_timer = timer::El2PhysicalTimer::new();
    let mut attached = false;
    let mut deadline_ticks = attach_deadline_ticks;

    loop {
        let now = el2_timer.now();
        if !attached {
            if now >= deadline_ticks {
                return fail_prefetch(PrefetchResult::Timeout);
            }
        } else if idle_timeout_ticks != 0 && now >= deadline_ticks {
            return fail_prefetch(PrefetchResult::Timeout);
        }

        let Some(byte) = try_read_byte() else {
            spin_loop();
            continue;
        };

        let event = assembler.push(byte);
        if !matches!(event, RspFrameEvent::Ignore) {
            attached = true;
            if idle_timeout_ticks != 0 {
                deadline_ticks = el2_timer.now().saturating_add(idle_timeout_ticks);
            }
        }
        match event {
            RspFrameEvent::Ignore => {}
            RspFrameEvent::Resync => {
                len = 0;
                if !push_prefetch_byte_state(&mut len, byte) {
                    return fail_prefetch(PrefetchResult::Overflow);
                }
            }
            RspFrameEvent::NeedMore => {
                if !push_prefetch_byte_state(&mut len, byte) {
                    return fail_prefetch(PrefetchResult::Overflow);
                }
            }
            RspFrameEvent::CtrlC | RspFrameEvent::FrameComplete => {
                if !push_prefetch_byte_state(&mut len, byte) {
                    return fail_prefetch(PrefetchResult::Overflow);
                }
                if !store_prefetch(len) {
                    return PrefetchResult::Unavailable;
                }
                return PrefetchResult::Success;
            }
        }
    }
}

fn push_prefetch_byte_state(len: &mut usize, byte: u8) -> bool {
    if !GDB_UART_READY.load(Ordering::Acquire) {
        return false;
    }
    let mut guard = GDB_UART_STATE.lock_irqsave();
    // SAFETY: READY is published after full initialization with Release ordering.
    let state = unsafe { (&mut *guard).assume_init_mut() };
    if *len >= state.prefetch_buf.len() {
        return false;
    }
    state.prefetch_buf[*len] = byte;
    *len += 1;
    true
}

fn store_prefetch(len: usize) -> bool {
    if !GDB_UART_READY.load(Ordering::Acquire) {
        return false;
    }
    let mut guard = GDB_UART_STATE.lock_irqsave();
    // SAFETY: READY is published after full initialization with Release ordering.
    let state = unsafe { (&mut *guard).assume_init_mut() };
    if len > state.prefetch_buf.len() {
        return false;
    }
    state.prefetch_len = len;
    state.prefetch_pos = 0;
    true
}

fn fail_prefetch(result: PrefetchResult) -> PrefetchResult {
    if matches!(result, PrefetchResult::Timeout | PrefetchResult::Overflow) {
        try_send_nak();
        flush_rx_input();
        clear_prefetch();
    }
    result
}

fn try_send_nak() {
    for _ in 0..NAK_ATTEMPTS {
        if try_write_byte(b'-') {
            break;
        }
    }
}

fn flush_rx_input() {
    if !GDB_UART_READY.load(Ordering::Acquire) {
        return;
    }
    let mut guard = GDB_UART_STATE.lock_irqsave();
    // SAFETY: READY is published after full initialization with Release ordering.
    let state = unsafe { (&mut *guard).assume_init_mut() };
    for _ in 0..FLUSH_LIMIT {
        if state.rx.pop().is_none() {
            break;
        }
    }
}

/// Tries to read one byte from prefetched/ring data.
pub fn try_read_byte() -> Option<u8> {
    if !GDB_UART_READY.load(Ordering::Acquire) {
        return None;
    }
    {
        let mut guard = GDB_UART_STATE.lock_irqsave();
        // SAFETY: READY is published after full initialization with Release ordering.
        let state = unsafe { (&mut *guard).assume_init_mut() };

        if state.prefetch_pos < state.prefetch_len {
            let byte = state.prefetch_buf[state.prefetch_pos];
            state.prefetch_pos += 1;
            if state.prefetch_pos >= state.prefetch_len {
                state.prefetch_pos = 0;
                state.prefetch_len = 0;
            }
            return Some(byte);
        }
        if let Some(byte) = state.rx.pop() {
            return Some(byte);
        }
    }

    // Poll backend directly when the debug loop is active or in a stop-loop section.
    if STOP_LOOP_ACTIVE.load(Ordering::Acquire) || is_debug_active() {
        poll_rx();
        let mut guard = GDB_UART_STATE.lock_irqsave();
        // SAFETY: READY is published after full initialization with Release ordering.
        let state = unsafe { (&mut *guard).assume_init_mut() };
        return state.rx.pop();
    }
    None
}

/// Tries to write one byte over UDP-backed GDB transport.
pub fn try_write_byte(byte: u8) -> bool {
    if !GDB_UART_READY.load(Ordering::Acquire) {
        return false;
    }
    crate::net::udp_uart::gdb_try_write_byte(byte)
}

/// Flushes pending GDB transport bytes.
pub fn flush() {
    if !GDB_UART_READY.load(Ordering::Acquire) {
        return;
    }
    crate::net::udp_uart::gdb_flush();
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;

    fn reset_attach_state() {
        DEBUG_ACTIVE.store(false, Ordering::Release);
        DEBUG_SESSION_ACTIVE.store(false, Ordering::Release);
        DEBUG_SESSION_INITIALIZED.store(false, Ordering::Release);
        ATTACH_REASON.store(0, Ordering::Release);
    }

    #[test_case]
    fn attach_reason_sets_with_debug_active_and_session_inactive() {
        reset_attach_state();
        set_debug_active(true);
        set_debug_session_active(false);
        record_attach_byte(b'$');
        assert_eq!(ATTACH_REASON.load(Ordering::Acquire), 1);
        reset_attach_state();
    }

    #[test_case]
    fn attach_reason_ignored_when_session_active() {
        reset_attach_state();
        set_debug_session_active(true);
        record_attach_byte(b'$');
        assert_eq!(ATTACH_REASON.load(Ordering::Acquire), 0);
        reset_attach_state();
    }
}
