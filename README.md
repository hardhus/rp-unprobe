# rp-unprobe

**Probe-less development for RP2040 / RP2350 with [Embassy](https://embassy.dev).**

No debug probe? `rp-unprobe` turns the board's own USB port into one, in debug builds only:

- **Logs over USB serial**: `info!`, `warn!`, `error!`, `debug!`, `trace!`, plain or with time since boot and source location, or in your own format.
- **Panic capture**: the message survives a reset and is shown on the next boot; the firmware then halts, like stopping at a breakpoint.
- **HardFault and stack overflow capture**: bad pointers, invalid instructions and (on the RP2350) stack overflows are reported the same way, with the source line, instead of a silent hang or memory corruption. What [flip-link](https://github.com/knurling-rs/flip-link) and a probe would give you.
- **Stack usage**: how deep the stack has been so far, before it overflows.
- **No BOOTSEL button**: `cargo run` reboots the running firmware into BOOTSEL, flashes it and streams the log into your terminal.

In **release** builds all of it is gone: no USB stack, no logger, no format strings, no fault handler. The release binary is byte-for-byte the same as one built with `panic-halt` and no logging.

```text
$ cargo run
Running firmware found (COM5), rebooting into BOOTSEL...
Flashing 298 KB (Raspberry Pi RP2350, family 0xe48bff59) -> E:\
Flash complete.
Waiting for USB serial log...
Connected: COM5 (Ctrl+C to quit)
[INFO] System started, initializing CYW43...
[INFO] LED ON
[INFO] LED OFF
--- device disconnected, waiting... ---
Connected: COM5 (Ctrl+C to quit)
[HARDFAULT] stack overflow on core 0 at PC=0x10004f34 LR=0x10004f57 [CFSR=0x00100000 HFSR=0x40000000]
  └─ PC app::parse @ src/main.rs:28
  └─ LR app::parse @ src/main.rs:31 (return address, can be stale)
[unprobe] firmware halted after HardFault; `cargo run` to reflash, or replug to restart
```

The repository has two crates, always released with the same version:

| Crate | What it is |
|---|---|
| [`rp-unprobe`](https://crates.io/crates/rp-unprobe) (lib name `unprobe`) | `no_std` library for the firmware |
| [`unprobe-cli`](https://crates.io/crates/unprobe-cli) (command `unprobe`) | host tool: cargo runner, flasher, log monitor |

## Quick start

**1. Install the CLI**

```sh
cargo install unprobe-cli
```

**2. Add the library** (`Cargo.toml` of your firmware)

```toml
[dependencies]
rp-unprobe = "0.2"
```

`rp-unprobe` provides the panic handler and the HardFault handler, so remove `panic-halt` / `panic-probe` (or see [Cargo features](#cargo-features)).

**3. Start it in `main`**

```rust
use unprobe::info;

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    unprobe::init!(spawner, p.USB);

    info!("hello from {}", "unprobe");
}
```

**4. Use the CLI as the cargo runner** (`.cargo/config.toml`)

```toml
[target.'cfg(all(target_arch = "arm", target_os = "none"))']
runner = "unprobe run"
```

**5. Run.** The first time, hold BOOTSEL while plugging in the board. After that, the button isn't needed:

```sh
cargo run            # debug: flash + live log
cargo run --release  # release: flash only, nothing of unprobe in the binary
```

## Debug vs. release

| | Debug (`cargo run`) | Release (`cargo run --release`) |
|---|---|---|
| Log macros | sent over USB serial | removed at compile time |
| USB | CDC-ACM serial port | not touched (`p.USB` is just consumed) |
| Panic | message saved, reset, shown on the next boot, then the firmware **halts** | **halts** (same code as `panic-halt`) |
| HardFault, stack overflow | saved, reset, shown on the next boot with the source line, then the firmware **halts** | cortex-m-rt's default handler (halts) |
| Next `cargo run` | no button needed | needs the BOOTSEL button (there is no USB to talk to) |
| Size cost | about 33–37 KB flash, 5 KB RAM ([details](#size-cost)) | **0** |

Debug and release are told apart by `debug_assertions`, the same switch `cargo build` / `cargo build --release` flips.

### Size cost

Measured with the same small app (a loop that logs a counter) built with and without `rp-unprobe`: `opt-level = "z"`, LTO, embassy-rp 0.10. Everything `rp-unprobe` adds is counted, including the USB stack and `core::fmt`; an app that already uses those pays less.

| Board | Build | Flash | RAM |
|---|---|---|---|
| Pico W (RP2040: 2 MB flash, 264 KB RAM) | debug | +36.4 KB (1.8%) | +5.0 KB (1.9%) |
| | release | **+0** (byte-identical) | **+0** |
| Pico 2 W (RP2350: 4 MB flash, 520 KB RAM) | debug | +32.9 KB (0.8%) | +5.0 KB (1.0%) |
| | release | **+0** (byte-identical) | **+0** |

What can be turned off or made smaller (debug only; measured against the logger alone, Pico W / Pico 2 W):

| Part | Flash | RAM | How |
|---|---|---|---|
| Log buffer | – | 2 KB | `UNPROBE_BUFFER_SIZE` (e.g. `512` saves 1.5 KB) |
| `panic-handler` | 15.3 KB / 3.1 KB | 268 B | feature; then use `panic-halt` |
| `hardfault-handler` | 2.1 KB / 3.0 KB | 36 B | feature |
| `bootsel-touch` | 64 B / 88 B | – | feature |
| Stack measurement | – | – | uses only the free stack; `measure_stack: false` |

The logger and the USB serial port themselves (18.7 KB / 27.2 KB flash, 2.7 KB RAM plus the buffer) are the core of the crate and are always there in debug. The parts share code (mostly `core::fmt`), so the numbers don't add up exactly; on the RP2040 the panic handler brings in most of the formatting code.

## The CLI

```text
unprobe run <ELF>   flash and show the log (the cargo runner)
                    --no-monitor  just flash
                    --family HEX  override the UF2 family ID
unprobe monitor     only show the log; reconnects after resets
                    --elf PATH    to show source lines in a HardFault report
unprobe reboot      reboot a running debug firmware into BOOTSEL
unprobe uf2 <ELF>   convert to UF2 only
```

`run`, `monitor` and `reboot` also take `--vid`, `--pid` (default `c0de:cafe`) and `--timeout` (seconds, default 10).

`run` and `monitor` take `--timestamp` to put the computer's local time in front of every line, optionally with a [strftime](https://docs.rs/chrono/latest/chrono/format/strftime/index.html) format:

```text
unprobe run --timestamp                         14:32:05.123 [INFO] LED ON
unprobe run --timestamp="%Y-%m-%d %H:%M:%S"     2026-10-30 14:32:05 [INFO] LED ON
```

To always have it, put it in the runner: `runner = "unprobe run --timestamp"`. This is the time a line *arrived*: lines logged before the terminal connected all arrive at once. For the time on the board, use [`Format::Detailed`](#log-format) or `unprobe::uptime()`.

- **Ctrl+C** stops the monitor with exit code 0, so cargo doesn't print an error.
- **HardFault reports** get the source location of the `PC`/`LR` addresses from the ELF's debug info. When the fault is inside library code (often inlined), the line in your own code is shown too.
- Flashing is a plain file copy to the BOOTSEL drive, so it needs no admin rights, drivers or libusb.
- The UF2 family ID comes from the board's `INFO_UF2.TXT` and the ELF: RP2040, RP2350 Arm or RP2350 RISC-V.
- BOOTSEL drives are looked up in drive letters on Windows, `/media/$USER`, `/run/media/$USER` and `/media` on Linux, and `/Volumes` on macOS.

## Configuration

```rust
unprobe::init!(spawner, p.USB, unprobe::Config {
    product: "My Board",              // shown by the OS
    level: log::LevelFilter::Info,    // default: Trace
    format: unprobe::Format::Detailed, // default: Plain
    ..unprobe::Config::new()
});
```

`Config` also has `vid`, `pid`, `manufacturer` and `measure_stack`. If you change the VID/PID, pass them to the CLI too: `runner = "unprobe run --vid 1234 --pid 5678"`.

### Log format

| `Format` | Looks like |
|---|---|
| `Plain` (default) | `[INFO] LED ON` |
| `Detailed` | `1.234567 INFO  LED ON`<br>`└─ blinky @ src/main.rs:23` |
| `Custom(fn)` | whatever your function writes |

```rust
fn my_format(w: &mut dyn core::fmt::Write, r: &log::Record) -> core::fmt::Result {
    write!(w, "{} ms {} {}", unprobe::uptime().as_millis(), r.level(), r.args())
}

unprobe::init!(spawner, p.USB, unprobe::Config {
    format: unprobe::Format::Custom(my_format),
    ..unprobe::Config::new()
});
```

The `log::Record` gives you the level, the message (`args()`), `module_path()`, `file()` and `line()`. A format can write several lines; separate them with `\r\n`. Like everything else, the format function is not in a release build.

`unprobe::uptime()` is the time since boot from the chip's 1 MHz timer (it doesn't need embassy-time's driver). It prints as seconds (`12.345678`) and has `as_micros()` / `as_millis()`.

### Log buffer size

Lines wait in one byte buffer until USB sends them. A line takes only its own length, so short lines leave room for many. The default is 2048 bytes; set it in your `.cargo/config.toml`:

```toml
[env]
UNPROBE_BUFFER_SIZE = "512"   # less RAM; or bigger if you log a lot at once
```

Cargo rebuilds the library when the value changes. In release there is no buffer at all.

### Sharing USB with your own device

If the firmware already uses USB (HID, MIDI, ...), add the log port to your own builder instead of `init!`. The board then shows up as a composite device:

```rust
let mut builder = embassy_usb::Builder::new(driver, config, ...);
unprobe::attach!(spawner, &mut builder);   // first, before your own classes
// ... add your own classes ...
spawner.spawn(usb_task(builder.build())).unwrap();
unprobe::halt_if_panicked!();
```

The driver, the `USBCTRL_IRQ` binding, the VID/PID and `UsbDevice::run` stay yours. In release, nothing is added to your device.

Call `attach!` before adding your own classes. If your device has serial ports of its own, they share the VID/PID with the log port; the CLI picks the log port by its lowest USB interface number.

## Crashes: panic, HardFault, stack overflow

All three end the same way in debug: the details are saved in RAM that survives a reset, the chip resets, `init!` shows the report over USB and the firmware halts there.

- **HardFault** covers bus faults (a bad pointer), undefined instructions, a bad function pointer, unaligned access and the like. The report names the cause, the core, the faulting address if there is one, and `PC`/`LR`. The CLI turns those into source lines.
- **Stack overflow (RP2350).** `init!` sets the Cortex-M33's stack limit register (MSPLIM) just above the end of your statics, keeping 1 KB below it for the fault handler. Instead of silently overwriting your statics (the usual way an overflow shows up without flip-link), the CPU faults right away and you get `[HARDFAULT] stack overflow`. Use it instead of flip-link: no extra linker step, and you see the message without a probe.
- **Location unknown.** If the stack is so full that the CPU can't even save where it was, the report says `location unknown: no room on the stack to save it`. It's still reported as a stack overflow; `stack_usage()` helps you find which part grows.
- **`LR`** is the return address: usually the caller, but in a function that hasn't called anything yet it can be left over from an earlier call. The CLI marks it as such.
- **Faults on core 1** are reported too (`on core 1`), and reset the whole chip.

### Stack usage

```rust
info!("stack: {}", unprobe::stack_usage().unwrap()); // stack: 3120 of 513640 bytes
```

At start, `init!` fills the unused stack with a pattern. `stack_usage()` returns how deep the stack has been at any point since then (the deepest point, not the current depth). It takes about a millisecond, since it scans the unused part. It's `None` in release, with `measure_stack: false`, or with a stack layout it doesn't recognize.

## Cargo features

All features are on by default.

| Feature | What it does |
|---|---|
| `panic-handler` | Provides `#[panic_handler]`. Turn it off to use your own (`panic-halt`, `panic-probe`, ...). |
| `hardfault-handler` | Provides the `HardFault` handler and, on the RP2350, the stack limit. Turn it off to use your own `#[exception] fn HardFault`. |
| `bootsel-touch` | Opening the port at 1200 baud reboots into BOOTSEL. This is what makes `cargo run` work without the button. |

Logger only:

```toml
rp-unprobe = { version = "0.2", default-features = false }
panic-halt = "1"
```

## Good to know

- **Crash before `init!`.** The message can't be shown without USB, and a reset could crash again in a loop. So the board goes straight to BOOTSEL, and the CLI says so. The same happens for a crash while a previous report is being shown. The board never gets stuck in a reset loop.
- **Halting after a crash.** In debug, the application does not run again after a panic or HardFault, so motors, relays and the like are not restarted behind your back. `cargo run` reflashes it; unplugging the board starts it normally again.
- **Nothing is written to flash.** Reports are kept in RAM only, so crashes can't wear out or damage the flash.
- **GPIOs after a crash.** Debug resets the chip, so pins go back to their reset state. Release halts in place, so pins keep their last state, as with `panic-halt`.
- **Lost log lines.** Lines still waiting in the buffer when the chip crashes are lost; the report itself is kept. If logging is faster than USB and the buffer fills up, lines are dropped and a `[unprobe] N messages dropped` line marks where. A line longer than the whole buffer is cut and ends with `...`.
- **Logs wait for the terminal.** Nothing is sent until a terminal opens the port, so early messages are not lost (as many as fit in the buffer).
- **Logging doesn't block interrupts.** A line is formatted with interrupts enabled; only reserving buffer space and committing the line happen in short critical sections. If an interrupt (or the other core) logs while a line is being formatted, its line is dropped and counted instead of waiting.
- **A heap at `cortex_m_rt::heap_start()`** lives in the RAM the stack measurement paints at start. Set `measure_stack: false` if you use one. A heap in a `static` array (the usual way with `embedded-alloc`) is fine.
- **flip-link, or your own stack layout.** If the stack isn't where cortex-m-rt normally puts it, or something already set the stack limit, `init!` leaves the stack alone: no limit, no measurement.
- **Use `unprobe::info!`, not `log::info!`.** Calling the `log` macros directly also works in debug. In release, though, the call stays in the binary unless your app enables `log`'s `release_max_level_off` feature. This crate doesn't enable it for you, because that would affect every crate in your build.
- **Another logger.** `unprobe` installs itself as the `log` logger. If something else already did, `init!` doesn't panic; it prints a notice over USB, and log output goes to the other logger.
- **One program per port.** On Windows only one program can open the serial port at a time. Close `unprobe monitor` or any other serial monitor before `cargo run`; the CLI tells you when the port is busy.
- **Windows opens Explorer** every time the board enters BOOTSEL, because it shows up as a USB drive. To stop this, go to Settings → Bluetooth & devices → AutoPlay → Removable drive → "Take no action".
- **1200 baud.** Any program that opens the port at 1200 baud reboots the board into BOOTSEL. Turn off `bootsel-touch` if that's a problem.
- **VID/PID `c0de:cafe`** is a test ID that the Embassy examples also use. If another device with the same ID is plugged in, give your firmware its own ID with `Config`.
- **Interrupt.** `init!` binds `USBCTRL_IRQ` itself; use `attach!` if you need USB for something else.
- **Chips.** The stack limit needs the RP2350's Cortex-M33; the RP2040 (Cortex-M0+) has none, so there a stack overflow is not caught (other HardFaults are). It usually ends with the CPU locked up while the USB port is still listed. The board then never answers: the CLI says so after 3 seconds, and it (or any serial monitor) can't finish until the board is unplugged, not even with Ctrl+C. Unplug the board, and plug it in with BOOTSEL held to flash again. On RISC-V there is no HardFault handler.
- **Versions.** Built for `embassy-rp` 0.10, `embassy-executor` 0.9 and `embassy-usb` 0.6. The chip feature of `embassy-rp` (`rp235xa`, `rp2040`, ...) is chosen by your application. On the RP2040, enable `portable-atomic`'s `critical-section` feature in your app (as for most Embassy RP2040 apps).
- **Tested hardware:** Raspberry Pi Pico 2 W (RP2350, Arm) and Pico W (RP2040), on Windows. RISC-V, Linux and macOS are supported by design but not tested yet. Reports are welcome.

## How it works

- **Logging:** a `log` logger formats each line straight into a byte ring buffer. A task sends it over USB CDC-ACM in 64-byte packets once the terminal has opened the port (DTR).
- **Panic:** the handler writes the message, with a checksum, into a `.uninit` RAM section that cortex-m-rt doesn't clear, then does a soft reset. SRAM survives the reset. `init!` finds the record, sends it and halts.
- **HardFault:** a small assembly entry first lifts the stack limit, so the handler can run on the 1 KB reserve even when the stack is full. The handler copies the fault registers and the stacked `PC`/`LR` (only from addresses known to be RAM) into `.uninit` and resets; the message is formatted on the next boot. Core 1 resets the chip through the boot ROM, since its own reset request only resets that core.
- **BOOTSEL:** when the host sets the line coding to 1200 baud, the firmware calls the boot ROM's `reset_to_usb_boot`.
- **Release:** every macro expands to nothing, or to `if false { format_args!(..) }`, which still type-checks the arguments. The modules behind `cfg(debug_assertions)` aren't compiled at all.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
