//! Debug: the panic message is written to RAM that isn't cleared on reset (`.uninit`) and
//! the chip resets. On the next boot `init!` sees the record, prints it over USB and halts
//! there without running the application again, like stopping at a breakpoint.
//! Whenever a reset could just panic again (before `init!` finished, while a previous
//! panic report is being shown, or a panic inside the panic handler), the chip reboots
//! into BOOTSEL instead, so it never ends up in a reset loop.
//! Release: halts, the same as panic-halt.

#[cfg(debug_assertions)]
pub use debug::take_report;

#[cfg(debug_assertions)]
mod debug {
    use crate::__private::{PANICKED, USB_UP};
    use crate::logger::Report;
    use core::fmt::Write;
    use core::panic::PanicInfo;
    use core::sync::atomic::Ordering;

    const MAGIC: u32 = 0x554e_5052; // "UNPR"
    const MSG_LEN: usize = 256;

    #[repr(C)]
    struct PanicRecord {
        magic: u32,
        len: u32,
        checksum: u32,
        msg: [u8; MSG_LEN],
    }

    // cortex-m-rt neither initializes nor zeroes `.uninit`; it survives a soft reset.
    // Any bit pattern is a valid PanicRecord, so reading it uninitialized is fine.
    #[unsafe(link_section = ".uninit.unprobe")]
    static mut RECORD: core::mem::MaybeUninit<PanicRecord> = core::mem::MaybeUninit::uninit();

    // Guards against a second panic while formatting the first one.
    static mut IN_PANIC: bool = false;

    fn checksum(len: u32, msg: &[u8]) -> u32 {
        msg.iter().fold(len ^ 0x9e37_79b9, |h, &b| {
            (h ^ b as u32).wrapping_mul(0x0100_0193)
        })
    }

    fn record() -> &'static mut PanicRecord {
        unsafe { &mut *(&raw mut RECORD).cast::<PanicRecord>() }
    }

    struct MsgWriter<'a> {
        buf: &'a mut [u8; MSG_LEN],
        len: usize,
    }

    impl Write for MsgWriter<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let n = s.len().min(MSG_LEN - self.len);
            self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
            self.len += n;
            Ok(())
        }
    }

    #[panic_handler]
    fn panic(info: &PanicInfo) -> ! {
        #[cfg(target_arch = "arm")]
        cortex_m::interrupt::disable();

        // In these cases a reset would likely panic again, in a loop. Go to BOOTSEL
        // instead: `unprobe run` can reflash right away.
        // - panic inside the panic handler
        // - USB not up yet: the message couldn't be shown anyway
        // - a panic while the previous panic's report is shown
        unsafe {
            if IN_PANIC {
                bootsel();
            }
            IN_PANIC = true;
        }
        if !USB_UP.load(Ordering::Relaxed) || PANICKED.load(Ordering::Relaxed) {
            bootsel();
        }

        let rec = record();
        let mut w = MsgWriter {
            buf: &mut rec.msg,
            len: 0,
        };
        let _ = write!(w, "{}", info.message());
        if let Some(loc) = info.location() {
            let _ = write!(w, " at {}:{}", loc.file(), loc.line());
        }
        let len = w.len as u32;
        rec.len = len;
        rec.checksum = checksum(len, &rec.msg[..len as usize]);
        rec.magic = MAGIC;

        reset()
    }

    fn reset() -> ! {
        #[cfg(target_arch = "arm")]
        cortex_m::peripheral::SCB::sys_reset();
        #[cfg(not(target_arch = "arm"))]
        bootsel();
    }

    fn bootsel() -> ! {
        crate::reboot_to_bootsel();
        // Only reached if the ROM call fails.
        loop {
            core::hint::spin_loop();
        }
    }

    /// Returns the panic report from the previous boot, if there is one, and clears it.
    pub fn take_report() -> Option<Report> {
        let rec = record();
        if rec.magic != MAGIC || rec.len as usize > MSG_LEN {
            return None;
        }
        let msg = &rec.msg[..rec.len as usize];
        let valid = rec.checksum == checksum(rec.len, msg);
        rec.magic = 0;
        if !valid {
            return None;
        }

        // A truncated message can end in the middle of a UTF-8 character.
        let text = match core::str::from_utf8(msg) {
            Ok(s) => s,
            Err(e) => unsafe { core::str::from_utf8_unchecked(&msg[..e.valid_up_to()]) },
        };
        let mut report = Report::new();
        let _ = report.push_str("[PANIC] ");
        let _ = report.push_str(text);
        let _ = report.push_str("\r\n[unprobe] firmware halted after panic; `cargo run` to reflash, or replug to restart");
        Some(report)
    }
}

// Identical to panic-halt 1.0, so release binaries are byte-for-byte the same.
#[cfg(not(debug_assertions))]
#[allow(clippy::empty_loop)]
#[inline(never)]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
