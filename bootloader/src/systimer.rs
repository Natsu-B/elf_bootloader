// system timer

use crate::println;
use core::arch::asm;
use core::num::NonZero;
use core::num::NonZeroU64;

pub struct SystemTimer {
    counter_frequency: Option<NonZeroU64>,
}

impl SystemTimer {
    pub fn new() -> Self {
        Self {
            counter_frequency: None,
        }
    }
    pub fn init(&mut self) {
        self.counter_frequency = Some(NonZero::new(Self::get_timer_frequency()).unwrap());
    }
    pub fn wait(&self, duration: core::time::Duration) {
        let micros = duration.as_micros();
        let start = Self::get_timer_counter();
        let wait_time = u128::from(
            self.counter_frequency
                .expect("before calling wait function call init")
                .get()
                / 1000
                / 1000,
        ) * micros;
        while u128::from(Self::get_timer_counter() - start) < wait_time {
            core::hint::spin_loop();
        }
    }
    fn get_timer_frequency() -> u64 {
        let current_frequency;
        unsafe {
            asm!("mrs {current_frequency}, CNTFRQ_EL0", current_frequency = out(reg)current_frequency);
        }
        println!("system counter frequency: {}Hz", current_frequency);
        current_frequency
    }
    fn get_timer_counter() -> u64 {
        let counter;
        unsafe {
            asm!("
        isb
        mrs {counter}, CNTPCT_EL0
        ", counter = out(reg)counter)
        };
        counter
    }
}
