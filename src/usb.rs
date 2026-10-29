use crate::Config;
use embassy_executor::Spawner;
use embassy_rp::interrupt::typelevel::{Binding, USBCTRL_IRQ};
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_usb::Builder;
use embassy_usb::class::cdc_acm::{CdcAcmClass, ControlChanged, Receiver, State};

/// Fires on every DTR/RTS/line-coding change (log_task waits for DTR with this).
pub static CONTROL_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[embassy_executor::task]
async fn usb_task(mut device: embassy_usb::UsbDevice<'static, Driver<'static, USB>>) {
    device.run().await;
}

/// The single listener for control changes (ControlChanged supports only one waiter).
/// "1200 baud touch": if the host opens the port at 1200 baud, reboot into BOOTSEL.
#[embassy_executor::task]
pub async fn control_task(
    control: ControlChanged<'static>,
    receiver: Receiver<'static, Driver<'static, USB>>,
) {
    loop {
        control.control_changed().await;
        #[cfg(feature = "bootsel-touch")]
        if receiver.line_coding().data_rate() == 1200 {
            crate::reboot_to_bootsel();
        }
        #[cfg(not(feature = "bootsel-touch"))]
        let _ = &receiver;
        CONTROL_CHANGED.signal(());
    }
}

static CONFIG_DESCRIPTOR: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
static BOS_DESCRIPTOR: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
static CONTROL_BUF: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
static STATE: static_cell::StaticCell<State> = static_cell::StaticCell::new();

/// Adds the CDC-ACM serial port to an existing USB builder (for composite devices).
pub fn add_class(
    builder: &mut Builder<'static, Driver<'static, USB>>,
) -> CdcAcmClass<'static, Driver<'static, USB>> {
    CdcAcmClass::new(builder, STATE.init(State::new()), 64)
}

/// Builds a USB device containing only the serial port and spawns its task.
pub fn init_usb(
    spawner: Spawner,
    usb_periph: embassy_rp::Peri<'static, USB>,
    irqs: impl Binding<USBCTRL_IRQ, InterruptHandler<USB>>,
    cfg: &Config,
) -> CdcAcmClass<'static, Driver<'static, USB>> {
    let driver = Driver::new(usb_periph, irqs);

    let mut config = embassy_usb::Config::new(cfg.vid, cfg.pid);
    config.manufacturer = Some(cfg.manufacturer);
    config.product = Some(cfg.product);
    config.max_power = 100;

    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESCRIPTOR.init([0; 256]),
        BOS_DESCRIPTOR.init([0; 256]),
        &mut [],
        CONTROL_BUF.init([0; 64]),
    );

    let class = add_class(&mut builder);

    let usb_device = builder.build();

    spawner.spawn(usb_task(usb_device)).unwrap();

    class
}
