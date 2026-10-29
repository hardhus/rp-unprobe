# rp-unprobe

**Probe-less development for RP2040 / RP2350 with [Embassy](https://embassy.dev).**

No debug probe? `rp-unprobe` turns the board's own USB port into one, in debug builds only:

- **Logs over USB serial**: `info!`, `warn!`, `error!`, `debug!`, `trace!`.
- **Panic capture**: the message survives a reset and is shown on the next boot; the firmware then halts, like stopping at a breakpoint.
- **No BOOTSEL button**: `cargo run` reboots the running firmware into BOOTSEL, flashes it and streams the log into your terminal.

In **release** builds all of it is gone: no USB stack, no logger, no format strings. The release binary has exactly the same functions as one built with `panic-halt` and no logging.

```text
$ cargo run
Running firmware found (COM5), rebooting into BOOTSEL...
Flashing 295 KB (Raspberry Pi RP2350, family 0xe48bff59) -> E:\
Flash complete.
Waiting for USB serial log...
Connected: COM5 (Ctrl+C to quit)
[INFO] System started, initializing CYW43...
[INFO] LED ON
[INFO] LED OFF
--- device disconnected, waiting... ---
Connected: COM5 (Ctrl+C to quit)
[PANIC] test panic 42 at src/main.rs:87
[unprobe] firmware halted after panic; `cargo run` to reflash, or replug to restart
```

The repository has two crates:

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
rp-unprobe = "0.1"
```

`rp-unprobe` provides the panic handler, so remove `panic-halt` / `panic-probe` (or see [Cargo features](#cargo-features)).

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
| Next `cargo run` | no button needed | needs the BOOTSEL button (there is no USB to talk to) |
| Size cost (measured on a Pico 2 W blinky) | about 26 KB flash, about 5.5 KB RAM | **0** |

Debug and release are told apart by `debug_assertions`, the same switch `cargo build` / `cargo build --release` flips.

## The CLI

```text
unprobe run <ELF>   flash and show the log (the cargo runner)
                    --no-monitor  just flash
                    --family HEX  override the UF2 family ID
unprobe monitor     only show the log; reconnects after resets
unprobe reboot      reboot a running debug firmware into BOOTSEL
unprobe uf2 <ELF>   convert to UF2 only
```

`run`, `monitor` and `reboot` also take `--vid`, `--pid` (default `c0de:cafe`) and `--timeout` (seconds, default 10).

- Flashing is a plain file copy to the BOOTSEL drive, so it needs no admin rights, drivers or libusb.
- The UF2 family ID comes from the board's `INFO_UF2.TXT` and the ELF: RP2040, RP2350 Arm or RP2350 RISC-V.
- BOOTSEL drives are looked up in drive letters on Windows, `/media/$USER`, `/run/media/$USER` and `/media` on Linux, and `/Volumes` on macOS.

## Configuration

```rust
unprobe::init!(spawner, p.USB, unprobe::Config {
    product: "My Board",              // shown by the OS
    level: log::LevelFilter::Info,    // default: Trace
    ..unprobe::Config::new()
});
```

`Config` also has `vid`, `pid` and `manufacturer`. If you change the VID/PID, pass them to the CLI too: `runner = "unprobe run --vid 1234 --pid 5678"`.

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

## Cargo features

Both features are on by default.

| Feature | What it does |
|---|---|
| `panic-handler` | Provides `#[panic_handler]`. Turn it off to use your own (`panic-halt`, `panic-probe`, ...). |
| `bootsel-touch` | Opening the port at 1200 baud reboots into BOOTSEL. This is what makes `cargo run` work without the button. |

Logger only:

```toml
rp-unprobe = { version = "0.1", default-features = false }
panic-halt = "1"
```

## Good to know

- **Panic before `init!`.** The message can't be shown without USB, and a reset could panic again in a loop. So the board goes straight to BOOTSEL, and the CLI says so. The same happens for a panic while a previous panic report is being shown. The board never gets stuck in a reset loop.
- **Halting after a panic.** In debug, the application does not run again after a panic, so motors, relays and the like are not restarted behind your back. `cargo run` reflashes it; unplugging the board starts it normally again.
- **GPIOs after a panic.** Debug resets the chip, so pins go back to their reset state. Release halts in place, so pins keep their last state, as with `panic-halt`.
- **Lost log lines.** Lines still waiting in the queue when a panic happens are lost; the panic message itself is kept. If logging is faster than USB, lines are dropped and a `[unprobe] N messages dropped` line marks where. Lines longer than 128 bytes end with `...`.
- **Logs wait for the terminal.** Nothing is sent until a terminal opens the port, so early messages are not lost (up to 16 queued lines).
- **Use `unprobe::info!`, not `log::info!`.** Calling the `log` macros directly also works in debug. In release, though, the call stays in the binary unless your app enables `log`'s `release_max_level_off` feature. This crate doesn't enable it for you, because that would affect every crate in your build.
- **Another logger.** `unprobe` installs itself as the `log` logger. If something else already did, `init!` doesn't panic; it prints a notice over USB, and log output goes to the other logger.
- **One program per port.** On Windows only one program can open the serial port at a time. Close `unprobe monitor` or any other serial monitor before `cargo run`; the CLI tells you when the port is busy.
- **Windows opens Explorer** every time the board enters BOOTSEL, because it shows up as a USB drive. To stop this, go to Settings → Bluetooth & devices → AutoPlay → Removable drive → "Take no action".
- **1200 baud.** Any program that opens the port at 1200 baud reboots the board into BOOTSEL. Turn off `bootsel-touch` if that's a problem.
- **VID/PID `c0de:cafe`** is a test ID that the Embassy examples also use. If another device with the same ID is plugged in, give your firmware its own ID with `Config`.
- **Interrupt.** `init!` binds `USBCTRL_IRQ` itself; use `attach!` if you need USB for something else.
- **Versions.** Built for `embassy-rp` 0.10, `embassy-executor` 0.9 and `embassy-usb` 0.6. The chip feature of `embassy-rp` (`rp235xa`, `rp2040`, ...) is chosen by your application.
- **Tested hardware:** Raspberry Pi Pico 2 W (RP2350, Arm) on Windows. RP2040, RISC-V, Linux and macOS are supported by design but not tested yet. Reports are welcome.

## How it works

- **Logging:** a `log` logger formats each line into a 16-slot queue. A task sends the lines over USB CDC-ACM, split into 64-byte packets, once the terminal has opened the port (DTR).
- **Panic:** the handler writes the message, with a checksum, into a `.uninit` RAM section that cortex-m-rt doesn't clear, then does a soft reset. SRAM survives the reset. `init!` finds the record, sends it and halts.
- **BOOTSEL:** when the host sets the line coding to 1200 baud, the firmware calls the boot ROM's `reset_to_usb_boot`.
- **Release:** every macro expands to nothing, or to `if false { format_args!(..) }`, which still type-checks the arguments. The modules behind `cfg(debug_assertions)` aren't compiled at all.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
