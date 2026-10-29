//! Finds the rp-unprobe serial port by VID/PID, 1200 baud touch and log monitoring.

use anyhow::Result;
use serialport::{SerialPort, SerialPortType};
use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub struct Device {
    pub vid: u16,
    pub pid: u16,
}

/// With `attach!` the device can have several serial ports with the same VID/PID.
/// The log port is the one with the lowest USB interface number, which is why
/// `attach!` has to be called before adding your own classes.
pub fn find_port(dev: Device) -> Option<String> {
    let mut ports: Vec<(u8, String)> = serialport::available_ports()
        .ok()?
        .into_iter()
        .filter_map(|p| match p.port_type {
            SerialPortType::UsbPort(u) if u.vid == dev.vid && u.pid == dev.pid => {
                Some((u.interface.unwrap_or(0), p.port_name))
            }
            _ => None,
        })
        .collect();
    ports.sort();

    static NOTED: AtomicBool = AtomicBool::new(false);
    if ports.len() > 1 && !NOTED.swap(true, Ordering::Relaxed) {
        let names: Vec<_> = ports.iter().map(|(_, n)| n.as_str()).collect();
        eprintln!(
            "Note: {} serial ports match {:04x}:{:04x}; using {} (lowest USB interface). \
             With attach!, call it before adding your own classes.",
            names.join(", "),
            dev.vid,
            dev.pid,
            names[0]
        );
    }
    ports.into_iter().next().map(|(_, name)| name)
}

/// Opens and closes the port at 1200 baud; rp-unprobe reboots the board into BOOTSEL.
pub fn touch(port: &str) -> Result<()> {
    match serialport::new(port, 1200)
        .timeout(Duration::from_millis(100))
        .open()
    {
        Ok(p) => {
            drop(p);
            Ok(())
        }
        Err(e) => anyhow::bail!(
            "can't open {port} ({e}). Is another program using it, e.g. `unprobe monitor` \
             or a serial monitor? Close it and try again."
        ),
    }
}

/// The OS lists the port a little before it can actually be opened.
fn open(port: &str) -> Result<Box<dyn SerialPort>> {
    let mut last = None;
    for _ in 0..20 {
        match serialport::new(port, 115_200)
            .timeout(Duration::from_millis(200))
            .open()
        {
            Ok(mut p) => {
                p.write_data_terminal_ready(true)?;
                return Ok(p);
            }
            Err(e) => last = Some(e),
        }
        sleep(Duration::from_millis(250));
    }
    Err(last.unwrap().into())
}

pub enum Seen {
    Port(String),
    Bootsel,
    Nothing,
}

/// Waits for either the log port or the BOOTSEL drive to show up.
pub fn wait_port_or_bootsel(dev: Device, timeout: Duration) -> Seen {
    let start = Instant::now();
    loop {
        if let Some(p) = find_port(dev) {
            return Seen::Port(p);
        }
        if crate::drive::find().is_some() {
            return Seen::Bootsel;
        }
        if start.elapsed() > timeout {
            return Seen::Nothing;
        }
        sleep(Duration::from_millis(200));
    }
}

const BOOTSEL_NOTE: &str = "--- board is in BOOTSEL mode: it panicked before unprobe::init! \
(the message can't be shown without USB), or a 1200 baud open requested it. \
Fix it and `cargo run` again (no button needed). ---";

/// Streams the log. When the board resets (e.g. after a panic) it waits and reconnects.
/// Returns `false` if neither the port nor a BOOTSEL drive appeared within `first_timeout`.
pub fn monitor(dev: Device, first_timeout: Duration) -> Result<bool> {
    let mut port_name = match wait_port_or_bootsel(dev, first_timeout) {
        Seen::Port(p) => p,
        Seen::Bootsel => {
            eprintln!("{BOOTSEL_NOTE}");
            return Ok(true);
        }
        Seen::Nothing => return Ok(false),
    };
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 512];
    loop {
        let mut port = open(&port_name)?;
        eprintln!("Connected: {port_name} (Ctrl+C to quit)");
        loop {
            match port.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => {
                    stdout.write_all(&buf[..n])?;
                    stdout.flush()?;
                }
                Err(e) if e.kind() == ErrorKind::TimedOut => {}
                Err(_) => break,
            }
            // On some systems a disconnect only shows up as endless timeouts.
            if find_port(dev).is_none() {
                break;
            }
        }
        drop(port);
        eprintln!("--- device disconnected, waiting... ---");
        port_name = match wait_port_or_bootsel(dev, Duration::MAX) {
            Seen::Port(p) => p,
            Seen::Bootsel => {
                eprintln!("{BOOTSEL_NOTE}");
                return Ok(true);
            }
            Seen::Nothing => unreachable!(),
        };
    }
}
