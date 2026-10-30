//! Writes the log to stdout line by line: optional host timestamps, and source locations
//! for the addresses in a `[HARDFAULT]` report (looked up in the ELF's debug info).

use anyhow::{Result, bail};
use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, Local};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DEFAULT_TIME_FORMAT: &str = "%H:%M:%S%.3f";

/// Checks a strftime format up front, so a typo is an error instead of a panic later.
pub fn parse_time_format(s: &str) -> Result<String, String> {
    if StrftimeItems::new(s).any(|i| matches!(i, Item::Error)) {
        return Err(format!(
            "invalid time format `{s}` (strftime syntax, e.g. \"{DEFAULT_TIME_FORMAT}\")"
        ));
    }
    Ok(s.to_string())
}

pub struct Output {
    timestamp: Option<String>,
    elf: Option<PathBuf>,
    symbols: Option<addr2line::Loader>,
    line: Vec<u8>,
    /// When the first byte of the current line arrived.
    started: Option<DateTime<Local>>,
}

impl Output {
    pub fn new(timestamp: Option<String>, elf: Option<PathBuf>) -> Self {
        Self {
            timestamp,
            elf,
            symbols: None,
            line: Vec::new(),
            started: None,
        }
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        for &b in data {
            if self.started.is_none() {
                self.started = Some(Local::now());
            }
            self.line.push(b);
            if b == b'\n' {
                self.end_line()?;
            }
        }
        Ok(())
    }

    /// Writes out a partial line (e.g. when the device disconnects).
    pub fn flush(&mut self) -> Result<()> {
        if !self.line.is_empty() {
            self.line.extend_from_slice(b"\r\n");
            self.end_line()?;
        }
        Ok(())
    }

    fn end_line(&mut self) -> Result<()> {
        let mut out = std::io::stdout().lock();
        if let (Some(fmt), Some(t)) = (&self.timestamp, self.started) {
            write!(out, "{} ", t.format(fmt))?;
        }
        out.write_all(&self.line)?;
        if self.line.starts_with(b"[HARDFAULT]") {
            let text = String::from_utf8_lossy(&self.line).into_owned();
            self.resolve(&mut out, &text)?;
        }
        out.flush()?;
        self.line.clear();
        self.started = None;
        Ok(())
    }

    fn resolve(&mut self, out: &mut impl Write, report: &str) -> Result<()> {
        let Some(elf) = &self.elf else {
            return Ok(());
        };
        if self.symbols.is_none() {
            match addr2line::Loader::new(elf) {
                Ok(l) => self.symbols = Some(l),
                Err(e) => {
                    writeln!(out, "  (can't read debug info from {}: {e})", elf.display())?;
                    self.elf = None;
                    return Ok(());
                }
            }
        }
        let symbols = self.symbols.as_ref().unwrap();
        for (name, addr) in addresses(report) {
            // LR is a return address with the Thumb bit set: look up the call itself.
            let probe = if name == "LR" {
                (addr & !1).wrapping_sub(2)
            } else {
                addr
            };
            let Ok(frames) = locate(symbols, probe) else {
                continue;
            };
            let note = if name == "LR" {
                " (return address, can be stale)"
            } else {
                ""
            };
            writeln!(out, "  └─ {name} {}{note}", frames[0].text)?;
            // Inside a library function (often inlined): also show where your code called it.
            if frames[0].library {
                if let Some(own) = frames.iter().find(|f| !f.library) {
                    writeln!(out, "        in {}", own.text)?;
                }
            }
        }
        Ok(())
    }
}

/// `PC=0x...` and `LR=0x...` in a report line. EXC_RETURN values (0xFxxxxxxx) are skipped.
fn addresses(report: &str) -> Vec<(&'static str, u64)> {
    let mut found = Vec::new();
    for name in ["PC", "LR"] {
        let key = format!("{name}=0x");
        if let Some(pos) = report.find(&key) {
            let hex: String = report[pos + key.len()..]
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if let Ok(a) = u64::from_str_radix(&hex, 16) {
                if a < 0xf000_0000 {
                    found.push((name, a));
                }
            }
        }
    }
    found
}

struct Frame {
    /// `function @ file:line`
    text: String,
    /// In the standard library or a crates.io dependency.
    library: bool,
}

/// The frames at the address, innermost (inlined) first.
fn locate(symbols: &addr2line::Loader, addr: u64) -> Result<Vec<Frame>> {
    let err = |e: addr2line::gimli::Error| anyhow::anyhow!("{e}");
    let mut iter = symbols
        .find_frames(addr)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut frames = Vec::new();
    while let Some(frame) = iter.next().map_err(err)? {
        let func = frame
            .function
            .as_ref()
            .and_then(|f| f.demangle().ok().map(|s| s.into_owned()))
            .or_else(|| symbols.find_symbol(addr).map(|s| s.to_string()))
            .unwrap_or_else(|| "?".to_string());
        let func = task_name(strip_hash(&func));
        let (text, library) = match frame.location.and_then(|l| Some((l.file?, l.line?))) {
            Some((file, line)) => {
                let (path, library) = short_path(file);
                (format!("{func} @ {path}:{line}"), library)
            }
            None => (func, true),
        };
        frames.push(Frame { text, library });
    }
    if frames.is_empty() {
        bail!("no debug info");
    }
    Ok(frames)
}

/// The body of an embassy task: `app::__blink_task::__blink_task_inner_function::{closure#0}`
/// -> `app::blink` (`main` for `#[embassy_executor::main]`).
fn task_name(name: &str) -> String {
    const INNER: &str = "_task_inner_function::{closure#0}";
    let Some(end) = name.find(INNER) else {
        return name.to_string();
    };
    let head = &name[..end];
    // head = "app::__blink_task::__blink"
    let Some((prefix, inner)) = head.rsplit_once("::") else {
        return name.to_string();
    };
    let prefix = prefix.rsplit_once("::").map_or("", |(p, _)| p);
    let task = inner.trim_start_matches('_');
    let task = if task == "embassy_main" { "main" } else { task };
    let rest = &name[end + INNER.len()..];
    match prefix {
        "" => format!("{task}{rest}"),
        _ => format!("{prefix}::{task}{rest}"),
    }
}

/// `blinky::main::h1234abcd...` -> `blinky::main`
fn strip_hash(name: &str) -> &str {
    match name.rsplit_once("::h") {
        Some((head, hash)) if hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit()) => {
            head
        }
        _ => name,
    }
}

/// Shortens paths: relative to the current directory, `core/src/...` for the standard
/// library, `crate-1.2.3/src/...` for crates.io dependencies, `repo/src/...` for git
/// dependencies. True for those.
fn short_path(file: &str) -> (String, bool) {
    let rel = std::env::current_dir().ok().and_then(|cwd| {
        Path::new(file)
            .strip_prefix(cwd)
            .ok()
            .map(|p| p.display().to_string())
    });
    let path = rel.unwrap_or_else(|| file.to_string()).replace('\\', "/");
    for marker in [
        "/rustlib/src/rust/library/",
        "/registry/src/",
        "/git/checkouts/",
    ] {
        if let Some(pos) = path.find(marker) {
            let rest = &path[pos + marker.len()..];
            let short = match marker {
                // One more level: the index directory.
                "/registry/src/" => rest.split_once('/').map_or(rest, |(_, r)| r).to_string(),
                // `repo-<hash>/<rev>/src/...` -> `repo/src/...`
                "/git/checkouts/" => match rest.splitn(3, '/').collect::<Vec<_>>()[..] {
                    [repo, _rev, file] => {
                        let name = repo.rsplit_once('-').map_or(repo, |(n, _)| n);
                        format!("{name}/{file}")
                    }
                    _ => rest.to_string(),
                },
                _ => rest.to_string(),
            };
            return (short, true);
        }
    }
    (path, false)
}
