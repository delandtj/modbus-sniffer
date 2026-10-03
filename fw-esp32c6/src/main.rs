//! Modbus RTU sniffer for an ESP32-C6-DevKitC-1 (ESP32-C6-WROOM-1).
//!
//! Sniffs an RS-485 bus on UART1 RX and streams decoded frames out of the chip's
//! built-in USB-Serial-JTAG peripheral, which enumerates as /dev/ttyACM0 on the
//! port labelled "USB" (not the "UART" port, which is the CP2102N bridge).
//!
//! Framing, CRC and line formatting live in `modbus-sniffer-core` and are shared
//! verbatim with the STM32F411 firmware.

#![no_std]
#![no_main]

use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
#[cfg(not(feature = "selftest"))]
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::Write as _;
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
#[cfg(not(feature = "selftest"))]
use esp_hal::uart::{Config as UartConfig, DataBits, Parity, RxConfig, StopBits, UartRx};
use esp_hal::usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagTx};
use esp_hal::Async;

// The panic handler comes from esp-backtrace, configured with the `println`
// feature; esp-println is pinned to its `jtag-serial` backend (see Cargo.toml),
// so a panic message and backtrace land on the SAME /dev/ttyACM0 the analyzer
// output goes to - exactly where the user is already looking.
//
// This coexists with the async UsbSerialJtag driver below because esp-println's
// serial-JTAG backend touches only the USB_DEVICE FIFO/CONF registers directly:
// it never takes the peripheral singleton, it polls instead of using the
// interrupt, and it gives up after a bounded spin if no host is draining the
// FIFO (so a panic can never turn into a silent hang on an unattached board).
// By the time it runs, the driver's own writes are over anyway - esp-backtrace
// prints and then parks the core with interrupts off.
//
// The deliberate consequence: `jtag-serial` rather than esp-println's default
// `auto` backend. `auto` would also link the UART0 printer and pick at runtime,
// which is dead weight here and would send panics to the other USB port.
use esp_backtrace as _;

use modbus_sniffer_core as sniffer;
use sniffer::{Line, Marker, DROPPED, FRAME_CHANNEL};

// espflash needs an ESP-IDF application descriptor in the image, otherwise the
// second-stage bootloader rejects it.
esp_bootloader_esp_idf::esp_app_desc!();

// ---------------------------------------------------------------------------
// User configuration
// ---------------------------------------------------------------------------

/// RS-485 baud rate. The 3.5 character timeout is derived from this.
#[cfg(not(feature = "selftest"))]
const BAUD_RATE: u32 = 9_600;

/// Sniffer input pin: the RS-485 module's data output goes here (GPIO4 on the
/// DevKitC-1 header). This alias is the only place the pin is named - change it
/// to move the input to another GPIO.
#[cfg(not(feature = "selftest"))]
type RxPin = esp_hal::peripherals::GPIO4<'static>;

/// RX FIFO level that wakes `read_async`. Modest on purpose: the 128-byte
/// hardware FIFO then holds plenty of slack while the USB task is busy, and a
/// long frame is handed over in pieces instead of all at once.
#[cfg(not(feature = "selftest"))]
const RX_FIFO_THRESHOLD: u16 = 32;

/// Hardware RX idle timeout, in symbol (character) times. Two symbols is ~2.29
/// ms at 9600 8E1, comfortably shorter than the ~4.58 ms t3.5 frame timeout, so
/// `read_async` returns once per burst and the software timer below is the thing
/// that actually decides where a frame ends.
#[cfg(not(feature = "selftest"))]
const RX_IDLE_SYMBOLS: u8 = 2;

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[esp_hal::main]
async fn main(spawner: Spawner) {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // esp-rtos owns the embassy-time driver, so it has to be running before
    // anything awaits a Timer. The #[main] macro has already put us inside its
    // thread-mode executor; starting the scheduler from the main task is the
    // documented order.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let (_usb_rx, usb_tx) = UsbSerialJtag::new(peripherals.USB_DEVICE)
        .into_async()
        .split();
    spawner.spawn(usb_task(usb_tx).unwrap());

    #[cfg(not(feature = "selftest"))]
    spawner.spawn(sniffer_task(peripherals.UART1, peripherals.GPIO4).unwrap());

    #[cfg(feature = "selftest")]
    spawner.spawn(selftest_task().unwrap());
}

// ---------------------------------------------------------------------------
// USB-Serial-JTAG output task
// ---------------------------------------------------------------------------

#[embassy_executor::task]
async fn usb_task(mut tx: UsbSerialJtagTx<'static, Async>) {
    let mut line = Line::new();
    let mut marker = Marker::new();

    loop {
        let frame = FRAME_CHANNEL.receive().await;

        // Report any frames lost since the last successful send, inline and in
        // temporal order, before the frame that follows them.
        let dropped = sniffer::take_dropped();
        if dropped > 0 && sniffer::format_dropped_marker(dropped, &mut marker).is_ok() {
            let _ = tx.write_all(marker.as_bytes()).await;
        }

        if sniffer::format_frame(&frame, &mut line).is_err() {
            // Line buffer too small for this frame: count it as lost rather
            // than emitting a truncated, lying line.
            DROPPED.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // If no host is draining the port these awaits stall. That is fine: the
        // sniffer side never blocks on us (try_send + DROPPED), so the only
        // consequence is a "[DROPPED n frames]" marker once a host shows up.
        let _ = tx.write_all(line.as_bytes()).await;
        let _ = tx.flush().await;
    }
}

// ---------------------------------------------------------------------------
// RS-485 sniffer task
// ---------------------------------------------------------------------------

/// UART1, RX only. TX is never assigned - the sniffer must not drive the bus,
/// and the auto-direction RS-485 module has no DE/RE pin to hold down.
#[cfg(not(feature = "selftest"))]
#[embassy_executor::task]
async fn sniffer_task(uart: esp_hal::peripherals::UART1<'static>, rx_pin: RxPin) {
    let config = UartConfig::default()
        .with_baudrate(BAUD_RATE)
        .with_data_bits(DataBits::_8)
        .with_parity(Parity::Even)
        .with_stop_bits(StopBits::_1)
        .with_rx(
            RxConfig::default()
                .with_fifo_full_threshold(RX_FIFO_THRESHOLD)
                .with_timeout(RX_IDLE_SYMBOLS),
        );

    let rx = UartRx::new(uart, config)
        .unwrap()
        .with_rx(rx_pin)
        .into_async();

    frame_receiver(rx).await;
}

#[cfg(not(feature = "selftest"))]
async fn frame_receiver(mut rx: UartRx<'static, Async>) {
    // BAUD_RATE is a const and frame_timeout_us is a const fn, so this folds at
    // compile time.
    let frame_timeout = Duration::from_micros(sniffer::frame_timeout_us(BAUD_RATE));

    let mut buf = heapless::Vec::<u8, { sniffer::MAX_FRAME_LEN }>::new();
    let mut chunk = [0u8; 64];

    loop {
        match select(rx.read_async(&mut chunk), Timer::after(frame_timeout)).await {
            Either::First(Ok(n)) => {
                for &b in &chunk[..n] {
                    if buf.push(b).is_err() {
                        // Frame overflow; drop it and start over.
                        buf.clear();
                        let _ = buf.push(b);
                    }
                }
            }
            Either::First(Err(_)) => {
                // UART error (framing/noise/parity/overflow). Flush the partial
                // frame if any, matching the STM32 firmware exactly.
                if !buf.is_empty() {
                    sniffer::emit_frame(&buf, Instant::now().as_millis());
                    buf.clear();
                }
            }
            Either::Second(_) => {
                // Silence detected -> frame boundary.
                if !buf.is_empty() {
                    sniffer::emit_frame(&buf, Instant::now().as_millis());
                    buf.clear();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Self-test task (feature = "selftest")
//
// Injects canned Modbus RTU frames into the same channel the real sniffer uses,
// so the USB output path and frame formatting can be verified without a live
// RS-485 bus. The UART is not initialised at all in this build.
// ---------------------------------------------------------------------------

#[cfg(feature = "selftest")]
#[embassy_executor::task]
async fn selftest_task() {
    loop {
        // 1) Master request: read 10 holding registers from slave 1.
        sniffer::emit_frame(
            &sniffer::build_frame(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x0a]),
            Instant::now().as_millis(),
        );
        Timer::after(Duration::from_millis(500)).await;

        // 2) Slave response: 20 bytes of register data.
        sniffer::emit_frame(
            &sniffer::build_frame(&[
                0x01, 0x03, 0x14, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x05, 0x00,
                0x06, 0x00, 0x07, 0x00, 0x08, 0x00, 0x09, 0x00, 0x0a,
            ]),
            Instant::now().as_millis(),
        );
        Timer::after(Duration::from_millis(500)).await;

        // 3) Deliberately corrupted frame -> exercises the BAD_CRC path.
        let mut bad = sniffer::build_frame(&[0x02, 0x06, 0x00, 0x10, 0x12, 0x34]);
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        sniffer::emit_frame(&bad, Instant::now().as_millis());
        Timer::after(Duration::from_millis(1000)).await;
    }
}
