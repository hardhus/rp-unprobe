//! Debug builds: a HardFault handler that reports the fault on the next boot, like a panic.
//!
//! The handler stores the fault registers in RAM that survives a reset (`.uninit`) and
//! resets; `init!` then prints the report and halts. When a reset could just fault again
//! (before `init!` finished, or while a previous report is shown), it reboots into BOOTSEL.
//!
//! On the RP2350 a stack overflow ends up here too (see `stack.rs`). The first thing the
//! handler does is lift the stack limit, so it can run on the reserve below it.
//! It doesn't format anything (that happens on the next boot) and reads memory only where
//! it is known to exist, so it can't fault itself.

use crate::__private::{PANICKED, USB_UP};
use crate::logger::Report;
use core::fmt::Write;
use core::ptr::read_volatile;
use core::sync::atomic::Ordering;

const MAGIC: u32 = 0x554e_4846; // "UNHF"

// SRAM, where an exception frame can be.
#[cfg(unprobe_v8m)]
const RAM: (u32, u32) = (0x2000_0000, 0x2008_2000);
#[cfg(not(unprobe_v8m))]
const RAM: (u32, u32) = (0x2000_0000, 0x2004_2000);

const SIO_CPUID: u32 = 0xd000_0000;
#[cfg(unprobe_v8m)]
const SCB_CFSR: u32 = 0xe000_ed28;
#[cfg(unprobe_v8m)]
const SCB_HFSR: u32 = 0xe000_ed2c;
#[cfg(unprobe_v8m)]
const SCB_MMFAR: u32 = 0xe000_ed34;
#[cfg(unprobe_v8m)]
const SCB_BFAR: u32 = 0xe000_ed38;

#[repr(C)]
#[derive(Clone, Copy)]
struct FaultRecord {
    magic: u32,
    core: u32,
    /// 0 if the exception frame couldn't be read.
    pc: u32,
    lr: u32,
    cfsr: u32,
    hfsr: u32,
    mmfar: u32,
    bfar: u32,
    checksum: u32,
}

impl FaultRecord {
    fn checksum(&self) -> u32 {
        [
            self.core, self.pc, self.lr, self.cfsr, self.hfsr, self.mmfar, self.bfar,
        ]
        .iter()
        .fold(0x9e37_79b9, |h, &v| (h ^ v).wrapping_mul(0x0100_0193))
    }
}

// Not initialized or zeroed by cortex-m-rt; survives a soft reset.
#[unsafe(link_section = ".uninit.unprobe.fault")]
static mut RECORD: core::mem::MaybeUninit<FaultRecord> = core::mem::MaybeUninit::uninit();

fn record() -> &'static mut FaultRecord {
    unsafe { &mut *(&raw mut RECORD).cast::<FaultRecord>() }
}

// The vector table points here (it replaces cortex-m-rt's default `HardFault`).
// r0 = the stack the frame was pushed to, r1 = EXC_RETURN.
#[cfg(unprobe_v8m)]
core::arch::global_asm!(
    ".pushsection .HardFault.unprobe, \"ax\", %progbits",
    ".global HardFault",
    ".type HardFault, %function",
    ".thumb_func",
    "HardFault:",
    // The stack pointer may sit right at the limit: lift it before anything is pushed.
    "movs r0, #0",
    "msr MSPLIM, r0",
    "mov r1, lr",
    "tst r1, #4",
    "ite eq",
    "mrseq r0, MSP",
    "mrsne r0, PSP",
    "b {handler}",
    ".size HardFault, . - HardFault",
    ".popsection",
    handler = sym unprobe_hardfault,
);

#[cfg(unprobe_v6m)]
core::arch::global_asm!(
    ".pushsection .HardFault.unprobe, \"ax\", %progbits",
    ".global HardFault",
    ".type HardFault, %function",
    ".thumb_func",
    "HardFault:",
    "mov r1, lr",
    "movs r2, #4",
    "tst r1, r2",
    "bne 1f",
    "mrs r0, MSP",
    "b 2f",
    "1:",
    "mrs r0, PSP",
    "2:",
    "ldr r2, ={handler}",
    "bx r2",
    ".ltorg",
    ".size HardFault, . - HardFault",
    ".popsection",
    handler = sym unprobe_hardfault,
);

#[unsafe(no_mangle)]
unsafe extern "C" fn unprobe_hardfault(sp: u32, _exc_return: u32) -> ! {
    // Same rule as the panic handler: no reset loops. Nothing is recorded then: the report
    // can't be shown, and it would still be in RAM after the next flash (the boot ROM
    // keeps SRAM), halting the new firmware.
    if !USB_UP.load(Ordering::Relaxed) || PANICKED.load(Ordering::Relaxed) {
        crate::reboot_to_bootsel();
    }

    let reg = |addr: u32| unsafe { read_volatile(addr as *const u32) };
    let rec = record();

    rec.core = reg(SIO_CPUID);

    // Basic frame: r0 r1 r2 r3 r12 lr pc xpsr.
    // When there was no room to save the frame, the CPU leaves the stack pointer at the
    // limit and the frame is garbage. Only core 0's stack has a limit.
    let limit = if rec.core == 0 {
        crate::stack::limit() as u32
    } else {
        0
    };
    let frame_ok = sp % 4 == 0 && sp >= RAM.0 && sp <= RAM.1 - 32 && (limit == 0 || sp > limit);
    (rec.lr, rec.pc) = if frame_ok {
        (reg(sp + 20), reg(sp + 24))
    } else {
        (0, 0)
    };
    #[cfg(unprobe_v8m)]
    {
        rec.cfsr = reg(SCB_CFSR);
        rec.hfsr = reg(SCB_HFSR);
        rec.mmfar = reg(SCB_MMFAR);
        rec.bfar = reg(SCB_BFAR);
    }
    #[cfg(not(unprobe_v8m))]
    {
        rec.cfsr = 0;
        rec.hfsr = 0;
        rec.mmfar = 0;
        rec.bfar = 0;
    }
    rec.checksum = rec.checksum();
    rec.magic = MAGIC;

    if rec.core == 0 {
        cortex_m::peripheral::SCB::sys_reset()
    }
    // SYSRESETREQ only resets the core that asks for it: core 1 resets the whole chip.
    reset_chip()
}

#[cfg(unprobe_v8m)]
fn reset_chip() -> ! {
    const REBOOT_TYPE_NORMAL: u32 = 0x0;
    const NO_RETURN_ON_SUCCESS: u32 = 0x100;
    embassy_rp::rom_data::reboot(REBOOT_TYPE_NORMAL | NO_RETURN_ON_SUCCESS, 1, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

// Like embassy-rp's `Watchdog::trigger_reset`: reset everything except the oscillators.
#[cfg(not(unprobe_v8m))]
fn reset_chip() -> ! {
    const PSM_WDSEL: u32 = 0x4001_0008;
    const WATCHDOG_CTRL: u32 = 0x4005_8000;
    unsafe {
        core::ptr::write_volatile(PSM_WDSEL as *mut u32, 0x0001_ffff & !0b11);
        core::ptr::write_volatile(WATCHDOG_CTRL as *mut u32, 1 << 31);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Where code can be: boot ROM, flash (XIP), SRAM.
fn is_code(addr: u32) -> bool {
    addr % 2 == 0
        && ((0x100..0x8000).contains(&addr)
            || (0x1000_0000..0x1400_0000).contains(&addr)
            || (RAM.0..RAM.1).contains(&addr))
}

const STKOF: u32 = 1 << 20;

fn cause(rec: &FaultRecord) -> &'static str {
    let c = rec.cfsr;
    let causes: [(u32, &str); 13] = [
        (STKOF, "stack overflow"),
        (1 << 16, "undefined instruction"),
        (1 << 17, "invalid state (a bad function pointer?)"),
        (1 << 18, "invalid exception return"),
        (1 << 19, "FPU/coprocessor not enabled"),
        (1 << 24, "unaligned access"),
        (1 << 25, "division by zero"),
        (1 << 9, "bus fault"),
        (1 << 10, "bus fault (imprecise)"),
        (1 << 8, "bus fault on instruction fetch"),
        (1 << 12, "bus fault while stacking"),
        (1 << 1, "memory access violation"),
        (1 << 0, "instruction access violation"),
    ];
    causes
        .iter()
        .find(|(bit, _)| c & bit != 0)
        .map(|(_, s)| *s)
        .unwrap_or(if rec.hfsr & (1 << 1) != 0 {
            "vector table read fault"
        } else {
            "hard fault"
        })
}

/// Returns the HardFault report from the previous boot, if there is one, and clears it.
pub fn take_report() -> Option<Report> {
    let rec = *record();
    if rec.magic != MAGIC {
        return None;
    }
    record().magic = 0;
    if rec.checksum != rec.checksum() {
        return None;
    }

    let mut r = Report::new();
    let _ = write!(r, "[HARDFAULT] {} on core {}", cause(&rec), rec.core);
    // The fault address registers, when they hold one.
    if rec.cfsr & (1 << 15) != 0 && rec.cfsr & (1 << 9) != 0 {
        let _ = write!(r, ", address 0x{:08x}", rec.bfar);
    } else if rec.cfsr & (1 << 7) != 0 && rec.cfsr & (1 << 1) != 0 {
        let _ = write!(r, ", address 0x{:08x}", rec.mmfar);
    }
    if is_code(rec.pc) {
        let _ = write!(r, " at PC=0x{:08x}", rec.pc);
        if is_code(rec.lr & !1) {
            let _ = write!(r, " LR=0x{:08x}", rec.lr);
        }
    } else if rec.cfsr & STKOF != 0 {
        // The stack was too full to save where it happened.
        let _ = r.push_str(" (location unknown: no room on the stack to save it)");
    } else {
        let _ = r.push_str(" (location unknown)");
    }
    #[cfg(unprobe_v8m)]
    let _ = write!(r, " [CFSR=0x{:08x} HFSR=0x{:08x}]", rec.cfsr, rec.hfsr);
    let _ = r.push_str(
        "\r\n[unprobe] firmware halted after HardFault; `cargo run` to reflash, or replug to restart",
    );
    Some(r)
}
