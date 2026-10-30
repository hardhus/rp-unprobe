mod drive;
mod output;
mod serial;
mod uf2;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use output::Output;
use serial::Device;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Flash RP2040/RP2350 over BOOTSEL without a probe and stream the rp-unprobe log.
#[derive(Parser)]
#[command(name = "unprobe", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Flash an ELF and show the log (for use as a cargo runner)
    Run {
        elf: PathBuf,
        /// Just flash, don't connect to serial
        #[arg(long)]
        no_monitor: bool,
        /// UF2 family ID (default: picked from the board and the ELF)
        #[arg(long, value_parser = parse_hex32)]
        family: Option<u32>,
        #[command(flatten)]
        dev: DeviceArgs,
        #[command(flatten)]
        log: LogArgs,
        /// Extra arguments cargo passes after the ELF (ignored)
        #[arg(trailing_var_arg = true, hide = true)]
        _rest: Vec<String>,
    },
    /// Connect to the log only (reconnects automatically after a reset)
    Monitor {
        #[command(flatten)]
        dev: DeviceArgs,
        #[command(flatten)]
        log: LogArgs,
        /// ELF of the running firmware, to show source locations in a HardFault report
        #[arg(long)]
        elf: Option<PathBuf>,
    },
    /// Reboot a running rp-unprobe firmware into BOOTSEL
    Reboot {
        #[command(flatten)]
        dev: DeviceArgs,
    },
    /// Convert an ELF to UF2
    Uf2 {
        elf: PathBuf,
        out: Option<PathBuf>,
        /// UF2 family ID (default: RP2350 ARM-S, or RISC-V for RISC-V ELFs; RP2040: e48bff56)
        #[arg(long, value_parser = parse_hex32)]
        family: Option<u32>,
    },
}

#[derive(Args)]
struct DeviceArgs {
    /// USB VID of the rp-unprobe firmware
    #[arg(long, value_parser = parse_hex16, default_value = "c0de")]
    vid: u16,
    /// USB PID of the rp-unprobe firmware
    #[arg(long, value_parser = parse_hex16, default_value = "cafe")]
    pid: u16,
    /// How long to wait for the BOOTSEL drive / serial port (seconds)
    #[arg(long, default_value_t = 10)]
    timeout: u64,
}

#[derive(Args)]
struct LogArgs {
    /// Prefix each line with the host's local time. Optional strftime format,
    /// e.g. --timestamp="%Y-%m-%d %H:%M:%S" (default "%H:%M:%S%.3f")
    #[arg(long, value_name = "FORMAT", num_args = 0..=1, require_equals = true,
          default_missing_value = output::DEFAULT_TIME_FORMAT, value_parser = output::parse_time_format)]
    timestamp: Option<String>,
}

impl DeviceArgs {
    fn device(&self) -> Device {
        Device {
            vid: self.vid,
            pid: self.pid,
        }
    }
    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout)
    }
}

fn parse_hex16(s: &str) -> Result<u16, String> {
    u16::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
}

fn parse_hex32(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
}

fn wait_drive(timeout: Duration) -> Option<drive::BootDrive> {
    let start = Instant::now();
    loop {
        if let Some(d) = drive::find() {
            return Some(d);
        }
        if start.elapsed() > timeout {
            return None;
        }
        sleep(Duration::from_millis(250));
    }
}

fn reboot(dev: &DeviceArgs) -> Result<drive::BootDrive> {
    let port = serial::find_port(dev.device()).context("no running rp-unprobe firmware found")?;
    eprintln!("Running firmware found ({port}), rebooting into BOOTSEL...");
    // The board can reboot while the port is still being opened, so an open error
    // alone doesn't mean it failed: only report it if no BOOTSEL drive shows up.
    let touched = serial::touch(&port);
    let wait = if touched.is_ok() {
        dev.timeout()
    } else {
        Duration::from_secs(3)
    };
    match wait_drive(wait) {
        Some(d) => Ok(d),
        None => {
            touched?;
            bail!(
                "BOOTSEL drive did not appear (firmware built without the `bootsel-touch` feature?). \
                 Hold BOOTSEL and plug the board in again."
            )
        }
    }
}

fn run(
    elf: PathBuf,
    no_monitor: bool,
    family: Option<u32>,
    dev: DeviceArgs,
    log: LogArgs,
) -> Result<()> {
    let data = std::fs::read(&elf).with_context(|| format!("can't read {}", elf.display()))?;

    let drive = match drive::find() {
        Some(d) => d,
        None if serial::find_port(dev.device()).is_some() => reboot(&dev)?,
        None => bail!("RP2040/RP2350 BOOTSEL drive not found. Hold BOOTSEL and plug the board in."),
    };

    let family = match family {
        Some(f) => f,
        None => uf2::family_for(&data, drive.rp2040)?,
    };
    let image = uf2::convert(&data, family)?;
    eprintln!(
        "Flashing {} KB ({}, family 0x{family:08x}) -> {}",
        image.len() / 2 / 1024,
        drive.model,
        drive.root.display()
    );
    // The board reboots as soon as the last block is written.
    std::fs::write(drive.root.join("unprobe.uf2"), &image)
        .context("copy to the BOOTSEL drive failed")?;
    eprintln!("Flash complete.");

    // Wait until the drive goes away (the board is rebooting); otherwise the monitor
    // would mistake the old drive for a new BOOTSEL.
    let start = Instant::now();
    while drive::find().is_some() && start.elapsed() < Duration::from_secs(5) {
        sleep(Duration::from_millis(100));
    }

    if no_monitor {
        return Ok(());
    }
    eprintln!("Waiting for USB serial log...");
    let mut out = Output::new(log.timestamp, Some(elf));
    if !serial::monitor(dev.device(), dev.timeout(), &mut out)? {
        eprintln!("No serial port appeared (release build, or a firmware without rp-unprobe).");
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Run {
            elf,
            no_monitor,
            family,
            dev,
            log,
            ..
        } => run(elf, no_monitor, family, dev, log),
        Cmd::Monitor { dev, log, elf } => {
            eprintln!("Waiting for USB serial log...");
            let mut out = Output::new(log.timestamp, elf);
            serial::monitor(dev.device(), Duration::MAX, &mut out)?;
            Ok(())
        }
        Cmd::Reboot { dev } => {
            let d = reboot(&dev)?;
            eprintln!("BOOTSEL drive: {}", d.root.display());
            Ok(())
        }
        Cmd::Uf2 { elf, out, family } => {
            let data = std::fs::read(&elf)?;
            let family = match family {
                Some(f) => f,
                None => uf2::family_for(&data, false)?,
            };
            let out = out.unwrap_or_else(|| elf.with_extension("uf2"));
            std::fs::write(&out, uf2::convert(&data, family)?)?;
            eprintln!("{} (family 0x{family:08x})", out.display());
            Ok(())
        }
    }
}
