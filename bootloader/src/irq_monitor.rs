use crate::vbar_watch;
use arch_hal::aarch64_mutex::RawSpinLockIrqSave;
use arch_hal::print;
use arch_hal::println;
use arch_hal::timer;
use core::time::Duration;

const STORM_WINDOW_MS: u64 = 100;
const STORM_WARN_INTERVAL_MS: u64 = 1000;
const STORM_THRESHOLD: u32 = 1000;
const MAX_OFFENDERS: usize = 8;

const MAX_INFLIGHT: usize = 64;
const EOI_TIMEOUT_MS: u64 = 2000;
const EOI_WARN_INTERVAL_MS: u64 = 1000;
const TIMEOUT_POLL_INTERVAL_MS: u64 = 100;
const MAX_TIMEOUT_WARNINGS_PER_POLL: usize = 4;

#[derive(Copy, Clone)]
struct OffenderEntry {
    intid: u32,
    count: u32,
    valid: bool,
}

impl OffenderEntry {
    const fn empty() -> Self {
        Self {
            intid: 0,
            count: 0,
            valid: false,
        }
    }
}

#[derive(Copy, Clone)]
struct OffenderCount {
    intid: u32,
    count: u32,
}

impl OffenderCount {
    const fn empty() -> Self {
        Self { intid: 0, count: 0 }
    }
}

#[derive(Copy, Clone)]
struct InflightEntry {
    pintid: u32,
    injected_ticks: u64,
    last_warn_ticks: u64,
    valid: bool,
}

impl InflightEntry {
    const fn empty() -> Self {
        Self {
            pintid: 0,
            injected_ticks: 0,
            last_warn_ticks: 0,
            valid: false,
        }
    }
}

struct IrqMonitorState {
    window_start_ticks: u64,
    total_in_window: u32,
    offenders: [OffenderEntry; MAX_OFFENDERS],
    offender_drop_count: u32,
    last_storm_warn_ticks: u64,
    inflight: [InflightEntry; MAX_INFLIGHT],
    inflight_drop_count: u32,
    last_timeout_poll_ticks: u64,
    counter_freq_hz: u64,
}

impl IrqMonitorState {
    const fn new() -> Self {
        Self {
            window_start_ticks: 0,
            total_in_window: 0,
            offenders: [OffenderEntry::empty(); MAX_OFFENDERS],
            offender_drop_count: 0,
            last_storm_warn_ticks: 0,
            inflight: [InflightEntry::empty(); MAX_INFLIGHT],
            inflight_drop_count: 0,
            last_timeout_poll_ticks: 0,
            counter_freq_hz: 0,
        }
    }

    fn reset_window(&mut self, now: u64) {
        self.window_start_ticks = now;
        self.total_in_window = 0;
        self.offenders = [OffenderEntry::empty(); MAX_OFFENDERS];
        self.offender_drop_count = 0;
    }

    fn bump_offender(&mut self, intid: u32) {
        if let Some(entry) = self
            .offenders
            .iter_mut()
            .find(|entry| entry.valid && entry.intid == intid)
        {
            entry.count = entry.count.saturating_add(1);
            return;
        }
        if let Some(slot) = self.offenders.iter_mut().find(|entry| !entry.valid) {
            *slot = OffenderEntry {
                intid,
                count: 1,
                valid: true,
            };
            return;
        }
        self.offender_drop_count = self.offender_drop_count.saturating_add(1);
    }
}

struct StormWarnSnapshot {
    total: u32,
    offenders: [OffenderCount; MAX_OFFENDERS],
    offender_len: usize,
    dropped: u32,
}

impl StormWarnSnapshot {
    const fn empty() -> Self {
        Self {
            total: 0,
            offenders: [OffenderCount::empty(); MAX_OFFENDERS],
            offender_len: 0,
            dropped: 0,
        }
    }

    fn sort_by_count_desc(&mut self) {
        let len = self.offender_len;
        for i in 0..len {
            let mut max_idx = i;
            for j in (i + 1)..len {
                if self.offenders[j].count > self.offenders[max_idx].count {
                    max_idx = j;
                }
            }
            if max_idx != i {
                self.offenders.swap(i, max_idx);
            }
        }
    }
}

#[derive(Copy, Clone)]
struct TimeoutWarn {
    pintid: u32,
    elapsed_ticks: u64,
}

impl TimeoutWarn {
    const fn empty() -> Self {
        Self {
            pintid: 0,
            elapsed_ticks: 0,
        }
    }
}

static IRQ_MONITOR: RawSpinLockIrqSave<IrqMonitorState> =
    RawSpinLockIrqSave::new(IrqMonitorState::new());
static EL2_TIMER: RawSpinLockIrqSave<Option<timer::El2PhysicalTimer>> =
    RawSpinLockIrqSave::new(None);

pub fn init_physical_timer_poll() {
    let timer = timer::El2PhysicalTimer::new();
    let counter_freq_hz = timer.counter_frequency_hz().get();
    let now = timer.now();
    timer.set_timeout(Duration::from_millis(TIMEOUT_POLL_INTERVAL_MS));
    {
        let mut guard = IRQ_MONITOR.lock_irqsave();
        guard.counter_freq_hz = counter_freq_hz;
        guard.window_start_ticks = now;
        guard.last_storm_warn_ticks = now;
        guard.last_timeout_poll_ticks = now;
    }
    let mut guard = EL2_TIMER.lock_irqsave();
    *guard = Some(timer);
}

pub fn handle_physical_timer_irq() {
    let Some(now) = with_timer(|timer| {
        timer.handle_irq_and_rearm(Duration::from_millis(TIMEOUT_POLL_INTERVAL_MS));
        timer.now()
    }) else {
        return;
    };
    poll_timeouts_at(now);
    vbar_watch::poll_vbar_el1_change();
}

pub fn record_ack(intid: u32, count_for_storm: bool) {
    if !count_for_storm {
        return;
    }
    let Some(now) = timer_now_ticks() else {
        return;
    };
    let mut snapshot = None;
    {
        let mut guard = IRQ_MONITOR.lock_irqsave();
        let freq = guard.counter_freq_hz;
        let window_ticks = ticks_from_ms(STORM_WINDOW_MS, freq);
        if window_ticks == 0 {
            return;
        }
        if guard.window_start_ticks == 0
            || now.wrapping_sub(guard.window_start_ticks) >= window_ticks
        {
            guard.reset_window(now);
        }
        guard.total_in_window = guard.total_in_window.saturating_add(1);
        guard.bump_offender(intid);
        let warn_interval_ticks = ticks_from_ms(STORM_WARN_INTERVAL_MS, freq);
        if guard.total_in_window >= STORM_THRESHOLD
            && now.wrapping_sub(guard.last_storm_warn_ticks) >= warn_interval_ticks
        {
            guard.last_storm_warn_ticks = now;
            let mut warn = StormWarnSnapshot::empty();
            warn.total = guard.total_in_window;
            warn.dropped = guard.offender_drop_count;
            for entry in guard.offenders.iter() {
                if !entry.valid || warn.offender_len >= warn.offenders.len() {
                    continue;
                }
                warn.offenders[warn.offender_len] = OffenderCount {
                    intid: entry.intid,
                    count: entry.count,
                };
                warn.offender_len += 1;
            }
            snapshot = Some(warn);
        }
    }
    if let Some(mut warn) = snapshot {
        warn.sort_by_count_desc();
        print_storm_warning(&warn);
    }
}

pub fn record_injected_pirq(pintid: u32, injection_hint: bool) {
    if !injection_hint {
        return;
    }
    let Some(now) = timer_now_ticks() else {
        return;
    };
    let mut guard = IRQ_MONITOR.lock_irqsave();
    if guard
        .inflight
        .iter()
        .any(|entry| entry.valid && entry.pintid == pintid)
    {
        return;
    }
    if let Some(slot) = guard.inflight.iter_mut().find(|entry| !entry.valid) {
        *slot = InflightEntry {
            pintid,
            injected_ticks: now,
            last_warn_ticks: now,
            valid: true,
        };
        return;
    }
    guard.inflight_drop_count = guard.inflight_drop_count.saturating_add(1);
}

pub fn record_pirq_eoi(pintid: u32) {
    clear_inflight(pintid);
}

pub fn record_pirq_deactivate(pintid: u32) {
    clear_inflight(pintid);
}

fn poll_timeouts_at(now: u64) {
    let mut warns = [TimeoutWarn::empty(); MAX_TIMEOUT_WARNINGS_PER_POLL];
    let mut warn_len = 0usize;
    let freq_hz;
    {
        let mut guard = IRQ_MONITOR.lock_irqsave();
        freq_hz = guard.counter_freq_hz;
        if freq_hz == 0 {
            return;
        }
        let poll_interval_ticks = ticks_from_ms(TIMEOUT_POLL_INTERVAL_MS, freq_hz);
        if poll_interval_ticks != 0
            && now.wrapping_sub(guard.last_timeout_poll_ticks) < poll_interval_ticks
        {
            return;
        }
        guard.last_timeout_poll_ticks = now;
        let timeout_ticks = ticks_from_ms(EOI_TIMEOUT_MS, freq_hz);
        let warn_interval_ticks = ticks_from_ms(EOI_WARN_INTERVAL_MS, freq_hz);
        for entry in guard.inflight.iter_mut() {
            if !entry.valid {
                continue;
            }
            let elapsed = now.wrapping_sub(entry.injected_ticks);
            if elapsed < timeout_ticks {
                continue;
            }
            if now.wrapping_sub(entry.last_warn_ticks) < warn_interval_ticks {
                continue;
            }
            if warn_len < warns.len() {
                entry.last_warn_ticks = now;
                warns[warn_len] = TimeoutWarn {
                    pintid: entry.pintid,
                    elapsed_ticks: elapsed,
                };
                warn_len += 1;
            }
        }
    }
    if warn_len == 0 {
        return;
    }
    for warn in warns.iter().take(warn_len) {
        let elapsed_ms = ms_from_ticks(warn.elapsed_ticks, freq_hz);
        println!("warning: pirq {} pending {}ms", warn.pintid, elapsed_ms);
    }
}

fn clear_inflight(pintid: u32) {
    let mut guard = IRQ_MONITOR.lock_irqsave();
    for entry in guard.inflight.iter_mut() {
        if entry.valid && entry.pintid == pintid {
            *entry = InflightEntry::empty();
            break;
        }
    }
}

fn with_timer<R>(f: impl FnOnce(&timer::El2PhysicalTimer) -> R) -> Option<R> {
    let guard = EL2_TIMER.lock_irqsave();
    let Some(timer) = guard.as_ref() else {
        return None;
    };
    Some(f(timer))
}

fn timer_now_ticks() -> Option<u64> {
    with_timer(|timer| timer.now())
}

fn print_storm_warning(warn: &StormWarnSnapshot) {
    print!(
        "warning: irq storm total={} window={}ms offenders:",
        warn.total, STORM_WINDOW_MS
    );
    for offender in warn.offenders.iter().take(warn.offender_len) {
        print!(" {}:{}", offender.intid, offender.count);
    }
    if warn.dropped != 0 {
        print!(" dropped={}", warn.dropped);
    }
    println!();
}

fn ticks_from_ms(ms: u64, freq_hz: u64) -> u64 {
    if freq_hz == 0 || ms == 0 {
        return 0;
    }
    let ticks = (u128::from(freq_hz) * u128::from(ms)) / 1000;
    ticks.min(u128::from(u64::MAX)) as u64
}

fn ms_from_ticks(ticks: u64, freq_hz: u64) -> u64 {
    if freq_hz == 0 {
        return 0;
    }
    let ms = (u128::from(ticks) * 1000) / u128::from(freq_hz);
    ms.min(u128::from(u64::MAX)) as u64
}
