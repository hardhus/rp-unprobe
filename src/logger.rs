use core::cell::Cell;
use core::fmt::Write;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_usb::class::cdc_acm::Sender;
use embassy_usb::driver::EndpointError;
use heapless::String;
use log::{Metadata, Record};

const LINE_LEN: usize = 128;
const TRUNCATED: &str = "...";

pub type Line = String<LINE_LEN>;
/// The panic report from the previous boot (sent before the first log line).
pub type Report = String<384>;

pub static LOG_CHANNEL: Channel<CriticalSectionRawMutex, Line, 16> = Channel::new();

// A Mutex instead of an atomic: RP2040 (thumbv6m) has no atomic read-modify-write.
static DROPPED: Mutex<CriticalSectionRawMutex, Cell<u32>> = Mutex::new(Cell::new(0));

fn take_dropped() -> u32 {
    DROPPED.lock(|d| d.replace(0))
}

fn dropped_line(n: u32) -> Line {
    let mut s = Line::new();
    let _ = write!(s, "[unprobe] {} messages dropped (log queue full)", n);
    s
}

/// Splits into max-packet-size chunks; the trailing "\r\n" (a short packet) ends the transfer.
async fn write_line(
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
    // A message that couldn't be sent because the connection dropped is kept here.
    let mut pending: Option<Line> = None;
    loop {
        // Don't send anything until a terminal opens the port (DTR), so nothing is lost.
        while !sender.dtr() {
            crate::usb::CONTROL_CHANGED.wait().await;
        }

        if let Some(r) = &report {
            if write_line(&mut sender, r.as_bytes()).await.is_err() {
                continue;
            }
            report = None;
        }

        loop {
            let msg = match pending.take() {
                Some(m) => m,
                None => LOG_CHANNEL.receive().await,
            };
            if !sender.dtr() {
                pending = Some(msg);
                break;
            }

            if write_line(&mut sender, msg.as_bytes()).await.is_err() {
                pending = Some(msg);
                break;
            }

            // Drops at the very end, with no later message to carry the marker.
            if LOG_CHANNEL.is_empty() {
                let dropped = take_dropped();
                if dropped > 0 {
                    let _ = write_line(&mut sender, dropped_line(dropped).as_bytes()).await;
                }
            }
        }
    }
}

/// Fills a fixed-size line; if the message doesn't fit, it ends with "...".
struct LineWriter {
    line: Line,
    truncated: bool,
}

impl Write for LineWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for c in s.chars() {
            if self.line.push(c).is_err() {
                self.truncated = true;
                return Err(core::fmt::Error);
            }
        }
        Ok(())
    }
}

impl LineWriter {
    fn finish(mut self) -> Line {
        if self.truncated {
            while self.line.len() > LINE_LEN - TRUNCATED.len() {
                self.line.pop();
            }
            let _ = self.line.push_str(TRUNCATED);
        }
        self.line
    }
}

struct UsbLogger;

impl log::Log for UsbLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let level_str = match record.level() {
            log::Level::Error => "ERROR",
            log::Level::Warn => "WARN",
            log::Level::Info => "INFO",
            log::Level::Debug => "DEBUG",
            log::Level::Trace => "TRACE",
        };

        let mut w = LineWriter {
            line: Line::new(),
            truncated: false,
        };
        let _ = write!(w, "[{}] {}", level_str, record.args());
        let line = w.finish();

        DROPPED.lock(|d| {
            // Put the drop marker into the queue exactly where the gap is.
            let n = d.get();
            if n > 0 {
                if LOG_CHANNEL.free_capacity() < 2 {
                    d.set(n + 1);
                    return;
                }
                let _ = LOG_CHANNEL.try_send(dropped_line(n));
                d.set(0);
            }
            if LOG_CHANNEL.try_send(line).is_err() {
                d.set(d.get() + 1);
            }
        });
    }

    fn flush(&self) {}
}

/// Queues a line from unprobe itself (not through `log`).
pub fn notice(s: &str) {
    let mut line = Line::new();
    let _ = line.push_str(s);
    let _ = LOG_CHANNEL.try_send(line);
}

pub fn init_logger(level: log::LevelFilter) -> Result<(), log::SetLoggerError> {
    static LOGGER: UsbLogger = UsbLogger;
    log::set_logger(&LOGGER)?;
    log::set_max_level(level);
    Ok(())
}
