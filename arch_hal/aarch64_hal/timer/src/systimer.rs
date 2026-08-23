use core::arch::asm;
use core::hint::spin_loop;
use core::num::NonZero;
use core::num::NonZeroU64;
use core::time::Duration;

use cpu::isb;

/// Simple polling timer that spins on CNTPCT_EL0.
#[derive(Default)]
pub struct SystemTimer {
    counter_frequency: Option<NonZeroU64>,
}

impl SystemTimer {
    /// Create an uninitialized timer (call `init` before use).
    pub const fn new() -> Self {
        Self {
            counter_frequency: None,
        }
    }

    /// Read CNTFRQ_EL0 once and cache the counter frequency.
    pub fn init(&mut self) {
        self.counter_frequency = Some(NonZero::new(read_counter_frequency()).unwrap());
    }

    /// Return the cached counter frequency in Hz.
    pub fn counter_frequency_hz(&self) -> NonZeroU64 {
        self.counter_frequency
            .expect("before calling wait function call init")
    }

    /// Busy-wait until the requested duration elapses.
    pub fn wait(&self, duration: Duration) {
        let start = read_counter();
        let ticks =
            (u128::from(self.counter_frequency_hz().get()) * duration.as_nanos()) / 1_000_000_000;

        while u128::from(read_counter().wrapping_sub(start)) < ticks {
            spin_loop();
        }
    }
}

pub fn read_counter_frequency() -> u64 {
    let current_frequency;
    // SAFETY: Reading CNTFRQ_EL0 is permitted in EL2 in this project and has no side effects.
    unsafe {
        asm!("mrs {current_frequency}, CNTFRQ_EL0", current_frequency = out(reg) current_frequency);
    }
    current_frequency
}

pub fn read_counter() -> u64 {
    // Synchronize before sampling the counter.
    isb();
    let counter;
    // SAFETY: Reading CNTPCT_EL0 is permitted in EL2 in this project and has no side effects.
    unsafe {
        asm!("mrs {counter}, CNTPCT_EL0", counter = out(reg) counter);
    }
    counter
}
