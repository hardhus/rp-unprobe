//! The log buffer: one byte ring that whole lines are written into back to back, so a
//! short line only takes its own length. Its size is `UNPROBE_BUFFER_SIZE` (default 2048).
//!
//! A line is formatted straight into the free part of the ring with interrupts enabled:
//! only taking the writer slot and committing the line happen in (tiny) critical sections.
//! A line logged while another one is being formatted (from an interrupt, the other core,
//! or a `Display` impl that logs) is dropped and counted instead of waiting for it.

use crate::Format;
use core::cell::{Cell, UnsafeCell};
use core::fmt::Write;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_sync::blocking_mutex::CriticalSectionMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_usb::class::cdc_acm::Sender;
use embassy_usb::driver::EndpointError;
use log::{Metadata, Record};

const SIZE: usize = buffer_size();
const TRUNCATED: &[u8] = b"...";

const fn buffer_size() -> usize {
    let Some(s) = option_env!("UNPROBE_BUFFER_SIZE") else {
        return 2048;
    };
    let s = s.as_bytes();
    assert!(
        !s.is_empty(),
        "UNPROBE_BUFFER_SIZE must be a number of bytes"
    );
    let mut n = 0;
    let mut i = 0;
    while i < s.len() {
        assert!(
            s[i].is_ascii_digit(),
            "UNPROBE_BUFFER_SIZE must be a number of bytes"
        );
        n = n * 10 + (s[i] - b'0') as usize;
        i += 1;
    }
    assert!(n >= 128, "UNPROBE_BUFFER_SIZE must be at least 128");
    n
}

/// The panic / HardFault report from the previous boot (sent before the first log line).
pub type Report = heapless::String<384>;

/// Bookkeeping, only touched inside the critical section.
#[derive(Clone, Copy)]
struct State {
    /// Index of the oldest unsent byte.
    head: usize,
    /// Committed (complete) bytes after `head`.
    len: usize,
    /// Someone is formatting a line into the free space.
    writing: bool,
    /// Lines dropped since the last drop marker.
    dropped: u32,
}

static STATE: CriticalSectionMutex<Cell<State>> = CriticalSectionMutex::new(Cell::new(State {
    head: 0,
    len: 0,
    writing: false,
    dropped: 0,
}));

struct Buf(UnsafeCell<[u8; SIZE]>);
// The writer only touches free space (after head + len) and only while it holds the
// `writing` slot; the reader only touches committed bytes. The two never overlap.
unsafe impl Sync for Buf {}
static BUF: Buf = Buf(UnsafeCell::new([0; SIZE]));

/// Wakes the log task when a line was committed.
static DATA: Signal<CriticalSectionRawMutex, ()> = Signal::new();

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.lock(|c| {
        let mut s = c.get();
        let r = f(&mut s);
        c.set(s);
        r
    })
}

/// Formats into the free space after the committed bytes. Nothing is visible to the
/// reader until the line is committed.
struct LineWriter {
    start: usize,
    /// Free bytes, minus 2 kept for the trailing "\r\n".
    room: usize,
    len: usize,
    full: bool,
}

impl LineWriter {
    fn put(&mut self, b: u8) {
        let buf = unsafe { &mut *BUF.0.get() };
        buf[(self.start + self.len) % SIZE] = b;
        self.len += 1;
    }
}

impl Write for LineWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if self.len == self.room {
                self.full = true;
                return Err(core::fmt::Error);
            }
            self.put(b);
        }
        Ok(())
    }
}

/// Writes one line (plus "\r\n") with `f`. Returns false if it was dropped.
fn push_line(f: impl FnOnce(&mut LineWriter) -> core::fmt::Result) -> bool {
    let slot = with_state(|s| {
        if s.writing {
            s.dropped += 1;
            return None;
        }
        s.writing = true;
        Some((s.head + s.len, SIZE - s.len, s.len == 0))
    });
    let Some((start, free, was_empty)) = slot else {
        return false;
    };

    let mut w = LineWriter {
        start: start % SIZE,
        room: free.saturating_sub(2),
        len: 0,
        full: false,
    };
    let _ = f(&mut w);
    let keep = if !w.full {
        true
    } else if was_empty {
        // Longer than the whole buffer: cut it, it would never fit.
        w.len = w.room - TRUNCATED.len();
        TRUNCATED.iter().for_each(|&b| w.put(b));
        true
    } else {
        false
    };
    if keep {
        w.put(b'\r');
        w.put(b'\n');
    }

    with_state(|s| {
        s.writing = false;
        if keep {
            s.len += w.len;
        } else {
            s.dropped += 1;
        }
    });
    if keep {
        DATA.signal(());
    }
    keep
}

/// Puts the drop marker into the buffer exactly where the gap is. False if it didn't fit.
fn push_dropped_marker() -> bool {
    let n = with_state(|s| core::mem::take(&mut s.dropped));
    if n == 0 {
        return true;
    }
    let ok = push_line(|w| write!(w, "[unprobe] {} messages dropped (log buffer full)", n));
    if !ok {
        // push_line counted the marker itself as dropped; it stands for the n lines.
        with_state(|s| s.dropped = s.dropped - 1 + n);
    }
    ok
}

fn has_dropped() -> bool {
    with_state(|s| s.dropped > 0)
}

/// Copies up to `out.len()` committed bytes without consuming them.
fn peek(out: &mut [u8]) -> usize {
    let (head, len) = with_state(|s| (s.head, s.len));
    let n = len.min(out.len());
    let buf = unsafe { &*BUF.0.get() };
    for (i, b) in out[..n].iter_mut().enumerate() {
        *b = buf[(head + i) % SIZE];
    }
    n
}

fn consume(n: usize) {
    with_state(|s| {
        s.head = (s.head + n) % SIZE;
        s.len -= n;
    });
}

/// Splits into max-packet-size chunks; the trailing "\r\n" (a short packet) ends the transfer.
async fn write_report(
    sender: &mut Sender<'static, Driver<'static, USB>>,
    data: &[u8],
) -> Result<(), EndpointError> {
    let max = sender.max_packet_size() as usize;
    for chunk in data.chunks(max) {
        sender.write_packet(chunk).await?;
    }
    sender.write_packet(b"\r\n").await
}

#[embassy_executor::task]
pub async fn log_task(
    mut sender: Sender<'static, Driver<'static, USB>>,
    mut report: Option<Report>,
) {
    let mut packet = [0u8; 64];
    let max = (sender.max_packet_size() as usize).min(packet.len());
    loop {
        // Don't send anything until a terminal opens the port (DTR), so nothing is lost.
        while !sender.dtr() {
            crate::usb::CONTROL_CHANGED.wait().await;
        }

        if let Some(r) = &report {
            if write_report(&mut sender, r.as_bytes()).await.is_err() {
                continue;
            }
            report = None;
        }

        // A full-size packet must be followed by a shorter one (or a zero-length packet)
        // before the host hands the data over.
        let mut last_full = false;
        loop {
            let n = peek(&mut packet[..max]);
            if n == 0 {
                // Drops at the very end, with no later line to carry the marker.
                if has_dropped() && push_dropped_marker() {
                    continue;
                }
                if last_full {
                    if sender.write_packet(&[]).await.is_err() {
                        break;
                    }
                    last_full = false;
                }
                DATA.wait().await;
                continue;
            }
            if !sender.dtr() {
                break;
            }
            // Only consumed once it was sent: after a disconnect it is sent again.
            if sender.write_packet(&packet[..n]).await.is_err() {
                break;
            }
            consume(n);
            last_full = n == max;
        }
    }
}

fn level_str(level: log::Level) -> &'static str {
    match level {
        log::Level::Error => "ERROR",
        log::Level::Warn => "WARN",
        log::Level::Info => "INFO",
        log::Level::Debug => "DEBUG",
        log::Level::Trace => "TRACE",
    }
}

/// `[INFO] message`
fn plain(w: &mut dyn Write, r: &Record) -> core::fmt::Result {
    write!(w, "[{}] {}", level_str(r.level()), r.args())
}

/// ```text
/// 1.234567 INFO  message
/// └─ blinky @ src/main.rs:23
/// ```
fn detailed(w: &mut dyn Write, r: &Record) -> core::fmt::Result {
    write!(
        w,
        "{} {:<5} {}\r\n└─ {} @ {}:{}",
        crate::uptime(),
        level_str(r.level()),
        r.args(),
        r.module_path().unwrap_or("?"),
        r.file().unwrap_or("?"),
        r.line().unwrap_or(0)
    )
}

struct UsbLogger {
    format: Cell<Format>,
}

// `format` is only written in `init_logger`, before `log::set_logger` publishes the logger.
unsafe impl Sync for UsbLogger {}

impl log::Log for UsbLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let format = match self.format.get() {
            Format::Plain => plain,
            Format::Detailed => detailed,
            Format::Custom(f) => f,
        };
        if has_dropped() && !push_dropped_marker() {
            with_state(|s| s.dropped += 1);
            return;
        }
        push_line(|w| format(w, record));
    }

    fn flush(&self) {}
}

/// Queues a line from unprobe itself (not through `log`).
pub fn notice(s: &str) {
    push_line(|w| w.write_str(s));
}

pub fn init_logger(level: log::LevelFilter, format: Format) -> Result<(), log::SetLoggerError> {
    static LOGGER: UsbLogger = UsbLogger {
        format: Cell::new(Format::Plain),
    };
    LOGGER.format.set(format);
    #[cfg(target_has_atomic = "ptr")]
    {
        log::set_logger(&LOGGER)?;
        log::set_max_level(level);
    }
    // RP2040 (no atomic compare-and-swap): `log` only has the racy versions. `init!` runs
    // once, at the start of the application, so nothing calls them at the same time.
    #[cfg(not(target_has_atomic = "ptr"))]
    unsafe {
        log::set_logger_racy(&LOGGER)?;
        log::set_max_level_racy(level);
    }
    Ok(())
}
