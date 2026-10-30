//! Time since boot, read straight from the chip's 1 MHz timer (the one embassy-time uses),
//! so it works without a time driver.

use core::fmt;
use core::ptr::read_volatile;

#[cfg(unprobe_v6m)]
const RESETS: usize = 0x4000_c000;
#[cfg(unprobe_v6m)]
const TIMER: usize = 0x4005_4000;
#[cfg(unprobe_v6m)]
const TIMER_RESET_BIT: u32 = 21;

// RP2350 (Arm or RISC-V)
#[cfg(not(unprobe_v6m))]
const RESETS: usize = 0x4002_0000;
#[cfg(not(unprobe_v6m))]
const TIMER: usize = 0x400b_0000;
#[cfg(not(unprobe_v6m))]
const TIMER_RESET_BIT: u32 = 23;

const RESET_DONE: usize = 0x08;
const TIMERAWH: usize = 0x24;
const TIMERAWL: usize = 0x28;

/// Time since boot in microseconds. Displayed as seconds: `12.345678`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uptime(u64);

impl Uptime {
    pub const fn as_micros(self) -> u64 {
        self.0
    }

    pub const fn as_millis(self) -> u64 {
        self.0 / 1000
    }
}

impl fmt::Display for Uptime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:06}", self.0 / 1_000_000, self.0 % 1_000_000)
    }
}

/// Time since boot from the hardware timer. Zero before `embassy_rp::init` (the timer is
/// still held in reset then).
///
/// Available in release builds too; it only takes up space if you call it.
pub fn uptime() -> Uptime {
    let reg = |addr: usize| unsafe { read_volatile(addr as *const u32) };
    if reg(RESETS + RESET_DONE) & (1 << TIMER_RESET_BIT) == 0 {
        return Uptime(0);
    }
    // The raw registers don't latch: re-read if the high word changed in between.
    loop {
        let hi = reg(TIMER + TIMERAWH);
        let lo = reg(TIMER + TIMERAWL);
        if reg(TIMER + TIMERAWH) == hi {
            return Uptime(((hi as u64) << 32) | lo as u64);
        }
    }
}
