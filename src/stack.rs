//! Debug builds: the hardware stack limit (RP2350 only) and stack usage measurement.
//!
//! cortex-m-rt puts the stack at the top of RAM, growing down towards the end of the
//! statics (`__euninit`). An overflow silently overwrites them.
//! - On the Cortex-M33, MSPLIM is set to that bottom + `RESERVE`: an overflow is a
//!   HardFault instead, and the handler uses the reserve to report it.
//! - The unused part of the stack is filled with a pattern; [`usage`] finds how far down
//!   it was overwritten.
//!
//! Both only happen for the standard layout while running on that stack. Anything else
//! (flip-link, a custom `_stack_start`, a limit that is already set) is left alone.
//!
//! `_stack_end` alone can't be trusted: with the usual RP2350 `memory.x` (`.end_block`
//! inserted after `.uninit`) it ends up pointing into flash. The bottom is `__euninit`,
//! or `_stack_end` if that is a higher address in RAM.

use crate::StackUsage;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

unsafe extern "C" {
    static _ram_start: u32;
    static _stack_start: u32;
    static _stack_end: u32;
    static __euninit: u32;
}

/// Kept free below the stack limit for the HardFault handler.
#[cfg(all(unprobe_v8m, feature = "hardfault-handler"))]
const RESERVE: usize = 1024;
/// Left untouched below the current stack pointer when painting (this function's callees).
const PAINT_MARGIN: usize = 1024;
const PAINT: u32 = 0x5354_4b21;

/// Lowest painted address; 0 if the stack wasn't painted.
static PAINTED: AtomicUsize = AtomicUsize::new(0);
/// The stack limit, if it was set.
static LIMIT: AtomicUsize = AtomicUsize::new(0);

/// (bottom, top) of the stack, if the layout is the standard one.
fn bounds() -> Option<(usize, usize)> {
    let ram_start = &raw const _ram_start as usize;
    let top = &raw const _stack_start as usize;
    let euninit = &raw const __euninit as usize;
    let stack_end = &raw const _stack_end as usize;
    let mut bottom = euninit;
    if (euninit..top).contains(&stack_end) {
        bottom = stack_end;
    }
    (ram_start <= euninit && bottom < top).then_some((bottom, top))
}

#[inline(always)]
fn sp() -> usize {
    let sp: usize;
    unsafe {
        core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags))
    };
    sp
}

pub fn setup(paint: bool) {
    let Some((bottom, top)) = bounds() else {
        return;
    };
    let sp = sp();
    if !(bottom < sp && sp <= top && sp - bottom >= 8 * 1024) {
        return;
    }

    #[cfg(all(unprobe_v8m, feature = "hardfault-handler"))]
    unsafe {
        let current: usize;
        core::arch::asm!("mrs {}, MSPLIM", out(reg) current, options(nomem, nostack, preserves_flags));
        if current == 0 {
            let limit = bottom + RESERVE;
            core::arch::asm!("msr MSPLIM, {}", in(reg) limit, options(nomem, nostack, preserves_flags));
            LIMIT.store(limit, Ordering::Relaxed);
        }
    }

    if paint {
        // Interrupts may run below us meanwhile; what they leave there is dead once they
        // return, so painting over it is fine.
        let start = (bottom + 3) & !3;
        let end = (sp - PAINT_MARGIN) & !3;
        let mut p = start;
        while p < end {
            unsafe { write_volatile(p as *mut u32, PAINT) };
            p += 4;
        }
        PAINTED.store(start, Ordering::Relaxed);
    }
}

/// The stack limit (MSPLIM) that `setup` set, or 0.
#[cfg(feature = "hardfault-handler")]
pub fn limit() -> usize {
    LIMIT.load(Ordering::Relaxed)
}

pub fn usage() -> Option<StackUsage> {
    let start = PAINTED.load(Ordering::Relaxed);
    if start == 0 {
        return None;
    }
    let (bottom, top) = bounds()?;
    let mut p = start;
    while p < top && unsafe { read_volatile(p as *const u32) } == PAINT {
        p += 4;
    }
    let floor = match LIMIT.load(Ordering::Relaxed) {
        0 => bottom,
        limit => limit,
    };
    Some(StackUsage {
        used: top - p,
        size: top - floor,
    })
}
