#![no_std]

//! Probe-less development for RP2040/RP2350 with [Embassy](https://embassy.dev).
//!
//! No debug probe? With `unprobe`, a debug build gets:
//! - **Logging over USB serial** with the usual `info!`, `warn!`, `error!`, `debug!`, `trace!`.
//! - **Panic capture:** the message survives a reset and is printed on the next boot.
//!   The firmware then halts there instead of running again, like stopping at a breakpoint.
//! - **HardFault and stack overflow capture:** reported the same way, with the source line
//!   (the CLI looks it up). On the RP2350 the stack limit register turns a stack overflow
//!   into a fault instead of silent memory corruption, like flip-link.
//! - **Stack usage:** [`stack_usage`] tells how deep the stack has been so far.
//! - **Flashing without the BOOTSEL button:** the `unprobe` CLI opens the port at 1200 baud
//!   and the board reboots into BOOTSEL by itself.
//!
//! In a **release** build all of this disappears: no USB stack, no logger, no format strings,
//! no fault handler. The binary is the same as one built without `unprobe` (the panic handler
//! behaves exactly like `panic-halt`).
//!
//! # Usage
//!
//! ```ignore
//! use unprobe::info;
//!
//! #[embassy_executor::main]
//! async fn main(spawner: Spawner) {
//!     let p = embassy_rp::init(Default::default());
//!     unprobe::init!(spawner, p.USB);
//!     info!("hello from {}", "unprobe");
//! }
//! ```
//!
//! Use the `unprobe` CLI as the cargo runner (`cargo install unprobe-cli`), in
//! `.cargo/config.toml`:
//!
//! ```toml
//! [target.'cfg(all(target_arch = "arm", target_os = "none"))']
//! runner = "unprobe run"
//! ```
//!
//! `cargo run` then flashes the board and streams the log into the terminal.
//!
//! [`init!`] needs an async context: if the previous boot panicked or faulted, it prints the
//! report and never returns, so the application does not run again until you reflash or replug.
//!
//! ## Device identity, log level and format
//!
//! ```ignore
//! unprobe::init!(spawner, p.USB, unprobe::Config {
//!     product: "My Board",
//!     level: log::LevelFilter::Info,
//!     format: unprobe::Format::Detailed, // or Format::Custom(your_fn)
//!     ..unprobe::Config::new()
//! });
//! ```
//!
//! The log buffer is 2048 bytes; set `UNPROBE_BUFFER_SIZE` in the `[env]` section of your
//! `.cargo/config.toml` to change it.
//!
//! If you change the VID/PID, pass them to the CLI too: `unprobe run --vid 1234 --pid 5678`.
//!
//! ## Sharing USB with your own device
//!
//! If the firmware already uses USB (HID, MIDI, ...), add the log port to your own builder
//! instead. The device then appears as a composite device. Driver, interrupt, VID/PID and
//! `UsbDevice::run` stay yours; only [`Config::level`] is used. In release, nothing is added.
//!
//! ```ignore
//! let mut builder = embassy_usb::Builder::new(driver, config, ...);
//! unprobe::attach!(spawner, &mut builder); // first, before your own classes
//! // ... add your own classes ...
//! spawner.spawn(usb_task(builder.build())).unwrap();
//! unprobe::halt_if_panicked!();
//! ```
//!
//! # Cargo features
//!
//! All are on by default. With `default-features = false`, only the logger is left.
//!
//! - `panic-handler`: provides the `#[panic_handler]`. Turn it off to use your own
//!   (for example `panic-halt` or `panic-probe`).
//! - `hardfault-handler`: provides the `HardFault` handler and, on the RP2350, the stack
//!   limit. Turn it off to use your own `#[exception] fn HardFault`.
//! - `bootsel-touch`: opening the port at 1200 baud reboots the board into BOOTSEL.
//!   Without it, you have to press the button for every flash.
//!
//! # Notes
//!
//! - Use the macros from this crate (`unprobe::info!`). Calling `log::info!` directly
//!   also works in debug builds, but in release it leaves the call in the binary unless
//!   your app enables the `log` feature `release_max_level_off`.
//! - `unprobe` installs itself as the `log` logger. If another logger is already installed,
//!   it doesn't panic: it prints a notice over USB, and log output goes to that logger.
//! - Debug and release are told apart by `debug_assertions` (`cargo build` vs
//!   `cargo build --release`).
//! - The chip feature of `embassy-rp` (`rp235xa`, `rp2040`, ...) is chosen by your application.

#[cfg(all(debug_assertions, feature = "hardfault-handler", target_arch = "arm"))]
mod fault;
#[cfg(debug_assertions)]
mod logger;
#[cfg(feature = "panic-handler")]
mod panic;
#[cfg(all(debug_assertions, target_arch = "arm"))]
mod stack;
mod uptime;
#[cfg(debug_assertions)]
mod usb;

pub use uptime::{Uptime, uptime};

#[cfg(debug_assertions)]
pub use log::{debug, error, info, trace, warn};

/// USB device identity and log level. Only used in debug builds.
///
/// Start from [`Config::new`] and override what you need:
/// `unprobe::Config { product: "My Board", ..unprobe::Config::new() }`.
#[derive(Clone, Copy)]
pub struct Config {
    /// USB vendor ID. Default `0xc0de` (a test ID, also used by the Embassy examples).
    pub vid: u16,
    /// USB product ID. Default `0xcafe`.
    pub pid: u16,
    /// Manufacturer string shown by the OS. Default `"unprobe"`.
    pub manufacturer: &'static str,
    /// Product string shown by the OS. Default `"unprobe console"`.
    pub product: &'static str,
    /// Most verbose level that is sent. Default [`log::LevelFilter::Trace`].
    pub level: log::LevelFilter,
    /// How a log line looks. Default [`Format::Plain`].
    pub format: Format,
    /// Fill the unused stack with a pattern at start, so [`stack_usage`] can measure it.
    /// Default `true`. Turn it off if something else lives in that RAM, e.g. a heap that
    /// starts at `cortex_m_rt::heap_start()`.
    pub measure_stack: bool,
}

/// Formats one log line (without the line ending). See [`Format::Custom`].
pub type FormatFn = fn(&mut dyn core::fmt::Write, &log::Record) -> core::fmt::Result;

/// How a log line looks. Only used in debug builds.
#[derive(Clone, Copy)]
pub enum Format {
    /// `[INFO] message`
    Plain,
    /// The time since boot ([`uptime`]), level, message, and where it was logged:
    /// ```text
    /// 1.234567 INFO  message
    /// └─ blinky @ src/main.rs:23
    /// ```
    Detailed,
    /// Your own function. It may write several lines (separate them with `"\r\n"`).
    ///
    /// ```ignore
    /// fn my_format(w: &mut dyn core::fmt::Write, r: &log::Record) -> core::fmt::Result {
    ///     write!(w, "{} {} {}", unprobe::uptime().as_millis(), r.level(), r.args())
    /// }
    /// ```
    Custom(FormatFn),
}

/// How much of the stack was used so far (the deepest point), see [`stack_usage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackUsage {
    /// Bytes used at the deepest point so far.
    pub used: usize,
    /// Bytes available in total.
    pub size: usize,
}

impl core::fmt::Display for StackUsage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} of {} bytes", self.used, self.size)
    }
}

/// The deepest the stack has been since boot: `info!("stack: {}", unprobe::stack_usage().unwrap())`.
///
/// `None` in release builds, before `init!`, with [`Config::measure_stack`] off, or with an
/// unusual stack layout. It scans the unused stack, which takes about a millisecond.
pub fn stack_usage() -> Option<StackUsage> {
    #[cfg(all(debug_assertions, target_arch = "arm"))]
    return stack::usage();
    #[cfg(not(all(debug_assertions, target_arch = "arm")))]
    None
}

impl Config {
    /// The default configuration (usable in `const` context).
    pub const fn new() -> Self {
        Self {
            vid: 0xc0de,
            pid: 0xcafe,
            manufacturer: "unprobe",
            product: "unprobe console",
            level: log::LevelFilter::Trace,
            format: Format::Plain,
            measure_stack: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

/// Reboots the chip into BOOTSEL (USB mass storage) mode, as if the button was held.
///
/// Available in release builds too; it only takes up space if you call it.
pub fn reboot_to_bootsel() {
    embassy_rp::rom_data::reset_to_usb_boot(0, 0);
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod __private {
    pub use embassy_rp;

    use crate::Config;
    use embassy_executor::Spawner;
    use embassy_rp::interrupt::typelevel::{Binding, USBCTRL_IRQ};
    use embassy_rp::peripherals::USB;
    use embassy_rp::usb::{Driver, InterruptHandler};
    use embassy_usb::class::cdc_acm::CdcAcmClass;

    use core::sync::atomic::{AtomicBool, Ordering};

    /// The previous boot ended in a panic or HardFault: the app must not run again.
    pub(crate) static PANICKED: AtomicBool = AtomicBool::new(false);
    /// USB and the logger are fully set up (a panic can be reported over USB).
    /// Starts out false on every boot (.bss).
    pub(crate) static USB_UP: AtomicBool = AtomicBool::new(false);

    fn start(spawner: Spawner, class: CdcAcmClass<'static, Driver<'static, USB>>, config: Config) {
        #[cfg(target_arch = "arm")]
        crate::stack::setup(config.measure_stack);
        let report = panic_report().or_else(fault_report);
        PANICKED.store(report.is_some(), Ordering::Relaxed);
        let (sender, receiver, control) = class.split_with_control();
        spawner
            .spawn(crate::usb::control_task(control, receiver))
            .unwrap();
        spawner
            .spawn(crate::logger::log_task(sender, report))
            .unwrap();
        if crate::logger::init_logger(config.level, config.format).is_err() {
            // Not a panic: that would happen again on every boot.
            crate::logger::notice(
                "[unprobe] another logger is already installed (log::set_logger); log output goes there, not to USB",
            );
        }
        // Only now: a panic during setup goes to BOOTSEL instead of a reset loop.
        USB_UP.store(true, Ordering::Relaxed);
    }

    #[cfg(feature = "panic-handler")]
    fn panic_report() -> Option<crate::logger::Report> {
        crate::panic::take_report()
    }

    #[cfg(not(feature = "panic-handler"))]
    fn panic_report() -> Option<crate::logger::Report> {
        None
    }

    #[cfg(all(feature = "hardfault-handler", target_arch = "arm"))]
    fn fault_report() -> Option<crate::logger::Report> {
        crate::fault::take_report()
    }

    #[cfg(not(all(feature = "hardfault-handler", target_arch = "arm")))]
    fn fault_report() -> Option<crate::logger::Report> {
        None
    }

    /// Never returns if the previous boot panicked or faulted; USB (log + BOOTSEL) keeps running.
    pub async fn halt_if_panicked() {
        if PANICKED.load(Ordering::Relaxed) {
            core::future::pending::<()>().await;
        }
    }

    pub fn init(
        spawner: Spawner,
        usb_periph: embassy_rp::Peri<'static, USB>,
        irqs: impl Binding<USBCTRL_IRQ, InterruptHandler<USB>>,
        config: Config,
    ) {
        let class = crate::usb::init_usb(spawner, usb_periph, irqs, &config);
        start(spawner, class, config);
    }

    pub fn attach(
        spawner: Spawner,
        builder: &mut embassy_usb::Builder<'static, Driver<'static, USB>>,
        config: Config,
    ) {
        start(spawner, crate::usb::add_class(builder), config);
    }
}

/// Starts unprobe on the USB peripheral (the firmware uses USB for nothing else).
///
/// ```ignore
/// unprobe::init!(spawner, p.USB);
/// unprobe::init!(spawner, p.USB, unprobe::Config { product: "My Board", ..unprobe::Config::new() });
/// ```
///
/// - Call it once, early in an async function (usually `main`), before code that can panic.
///   A panic before it reboots the board into BOOTSEL, since the message can't be shown yet.
/// - If the previous boot panicked, it prints the report and **never returns**.
/// - It binds `USBCTRL_IRQ` itself, so don't bind that interrupt elsewhere. To share USB
///   with your own device, use [`attach!`] instead.
/// - Release: does nothing; `p.USB` is just consumed.
#[cfg(debug_assertions)]
#[macro_export]
macro_rules! init {
    ($spawner:expr, $usb:expr) => {
        $crate::init!($spawner, $usb, $crate::Config::new())
    };
    ($spawner:expr, $usb:expr, $config:expr) => {{
        $crate::__private::embassy_rp::bind_interrupts!(struct __UnprobeIrqs {
            USBCTRL_IRQ => $crate::__private::embassy_rp::usb::InterruptHandler<
                $crate::__private::embassy_rp::peripherals::USB,
            >;
        });
        $crate::__private::init($spawner, $usb, __UnprobeIrqs, $config);
        $crate::halt_if_panicked!();
    }};
}

/// For [`attach!`] users: call right after spawning your USB device task, in an async function.
///
/// If the previous boot panicked, it never returns: the report is shown over USB and the
/// application does not run again. [`init!`] does this by itself. Release: does nothing.
#[cfg(debug_assertions)]
#[macro_export]
macro_rules! halt_if_panicked {
    () => {
        $crate::__private::halt_if_panicked().await
    };
}

#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! halt_if_panicked {
    () => {{}};
}

/// Release: nothing happens; the USB peripheral is just consumed.
#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! init {
    ($spawner:expr, $usb:expr) => {{
        let _ = $spawner;
        let _ = $usb;
    }};
    ($spawner:expr, $usb:expr, $config:expr) => {{
        let _ = $spawner;
        let _ = $usb;
        let _: $crate::Config = $config;
    }};
}

/// Adds the log port to your own USB device (composite device).
///
/// ```ignore
/// unprobe::attach!(spawner, &mut builder);
/// // ... add your own classes, build and spawn the device ...
/// unprobe::halt_if_panicked!();
/// ```
///
/// - Call it **before** adding your own classes. The log port then gets the lowest
///   interface number, which is how the `unprobe` CLI tells it apart from your own
///   serial ports (they share the VID/PID).
/// - `builder` is an `embassy_usb::Builder<'static, embassy_rp::usb::Driver<'static, USB>>`.
///   Your device must keep `composite_with_iads` on (the `embassy_usb::Config` default).
/// - Driver, interrupt binding, VID/PID and `UsbDevice::run` stay yours; from the optional
///   [`Config`] only `level` is used.
/// - Call [`halt_if_panicked!`] after spawning the device, so a panic report can be shown.
/// - Release: the builder is left untouched.
#[cfg(debug_assertions)]
#[macro_export]
macro_rules! attach {
    ($spawner:expr, $builder:expr) => {
        $crate::attach!($spawner, $builder, $crate::Config::new())
    };
    ($spawner:expr, $builder:expr, $config:expr) => {
        $crate::__private::attach($spawner, $builder, $config)
    };
}

/// Release: the builder is left untouched.
#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! attach {
    ($spawner:expr, $builder:expr) => {{
        let _ = $spawner;
        let _ = $builder;
    }};
    ($spawner:expr, $builder:expr, $config:expr) => {{
        let _ = $spawner;
        let _ = $builder;
        let _: $crate::Config = $config;
    }};
}

// Release: the arguments are still type-checked (so no unused warnings),
// but `if false` lets the compiler remove them completely.
#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! info {
    ($($arg:tt)+) => {{ if false { let _ = ::core::format_args!($($arg)+); } }};
}
#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! warn {
    ($($arg:tt)+) => {{ if false { let _ = ::core::format_args!($($arg)+); } }};
}
#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! error {
    ($($arg:tt)+) => {{ if false { let _ = ::core::format_args!($($arg)+); } }};
}
#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! debug {
    ($($arg:tt)+) => {{ if false { let _ = ::core::format_args!($($arg)+); } }};
}
#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! trace {
    ($($arg:tt)+) => {{ if false { let _ = ::core::format_args!($($arg)+); } }};
}
