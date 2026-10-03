//! Modbus RTU sniffer for an ESP32-C6-DevKitC-1 (ESP32-C6-WROOM-1).
//!
//! Sniffs an RS-485 bus on UART1 RX and streams decoded frames to two outputs
//! at once:
//!
//! - the chip's built-in USB-Serial-JTAG peripheral, which enumerates as
//!   /dev/ttyACM0 on the port labelled "USB" (not the "UART" port, which is the
//!   CP2102N bridge), and
//! - a TCP server on port 4000 over WiFi, for unattended multi-day captures.
//!
//! Both carry the same line protocol and both accept the same commands
//! (`bus`, `status`, `note`). See the README for the protocol.
//!
//! Everything the sniffer produces goes through one ~64 KB ring of text lines
//! in `modbus-sniffer-core`. Each output has its own cursor into that ring, so
//! a USB host that is not reading cannot stall the TCP client (which is what
//! used to happen with the single-consumer channel), the TCP cursor survives
//! client disconnects to give a reconnecting client its replay, and the sniffer
//! itself never waits for either of them.
//!
//! Framing, CRC, line formatting, the ring and the command parser live in
//! `modbus-sniffer-core`; the framing and formatting are shared verbatim with
//! the STM32F411 firmware.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
#[cfg(not(feature = "selftest"))]
use embassy_futures::select::{select3, Either3};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::{Read as _, Write as _};
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
#[cfg(not(feature = "selftest"))]
use esp_hal::uart::{Config as UartConfig, DataBits, Parity, RxConfig, StopBits, Uart};
use esp_hal::usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx};
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
use sniffer::{BusConfig, Chunk, Command, Line, Marker, SnifferBus};

mod net;
mod settings;

// espflash needs an ESP-IDF application descriptor in the image, otherwise the
// second-stage bootloader rejects it.
esp_bootloader_esp_idf::esp_app_desc!();

// ---------------------------------------------------------------------------
// Build-time configuration
// ---------------------------------------------------------------------------

/// WiFi credentials come from the environment at build time and are never
/// committed:
///
/// ```sh
/// SNIFFER_WIFI_SSID=myap SNIFFER_WIFI_PASS=secret cargo run --release
/// ```
///
/// With no SSID the build still succeeds and WiFi is simply not started.
const WIFI_SSID: Option<&str> = option_env!("SNIFFER_WIFI_SSID");
const WIFI_PASS: Option<&str> = option_env!("SNIFFER_WIFI_PASS");

/// Name this device reports in its hello line.
pub const DEVICE_NAME: &str = "sniffer-esp32c6";

/// Heap for esp-radio: the WiFi driver's buffers and the RTOS task stacks it
/// creates. The radio needs a few tens of KB; the rest of the firmware never
/// allocates.
const HEAP_SIZE: usize = 96 * 1024;

/// Sniffer input pin: the RS-485 module's data output goes here (GPIO4 on the
/// DevKitC-1 header). This alias is the only place the pin is named - change it
/// to move the input to another GPIO.
#[cfg(not(feature = "selftest"))]
type RxPin = esp_hal::peripherals::GPIO4<'static>;

/// RX FIFO level that wakes `read_async`. Must be 1: the frame timer in
/// `frame_receiver` restarts every time a read returns, so a read has to return
/// as soon as any byte arrives. With a higher threshold the read stays pending
/// mid-frame, the t3.5 timer fires while bytes are still streaming in, and
/// frames get split (seen on hardware with 32). The 128-byte hardware FIFO still
/// absorbs any latency in servicing the interrupt.
#[cfg(not(feature = "selftest"))]
const RX_FIFO_THRESHOLD: u16 = 1;

/// Hardware RX idle timeout, in symbol (character) times. With a FIFO threshold
/// of 1 this rarely matters; it only flushes stragglers. The software timer in
/// `frame_receiver` is what decides where a frame ends.
#[cfg(not(feature = "selftest"))]
const RX_IDLE_SYMBOLS: u8 = 2;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// The one ring every output reads from. ~64 KB of line storage plus a slot
/// table; all-zero, so it lands in .bss.
static LINES: SnifferBus = SnifferBus::new();

/// The bus parameters currently in force.
static BUS: BlockingMutex<CriticalSectionRawMutex, core::cell::Cell<BusConfig>> =
    BlockingMutex::new(core::cell::Cell::new(sniffer::DEFAULT_BUS));

/// Tells the sniffer task to re-apply the UART configuration.
static BUS_CHANGED: Signal<CriticalSectionRawMutex, BusConfig> = Signal::new();

/// Command replies on their way back to the USB host that asked.
static USB_REPLIES: Channel<CriticalSectionRawMutex, Line, 2> = Channel::new();

/// Frames seen since boot, including short and bad-CRC ones.
static FRAMES: AtomicU32 = AtomicU32::new(0);
/// Frames of 4 bytes or more whose CRC did not check out. Short frames are not
/// counted here - they have no CRC to be wrong.
static BAD_CRC: AtomicU32 = AtomicU32::new(0);
/// UART receive errors (framing, parity, noise, overrun).
static UART_ERRORS: AtomicU32 = AtomicU32::new(0);
/// Lines the TCP consumer had to skip, plus lines the ring refused outright:
/// real gaps in the server-side capture.
static DROPPED_LINES: AtomicU32 = AtomicU32::new(0);
/// Lines the USB consumer had to skip. Kept separate because it grows forever
/// when no USB host is reading (board powered from a charger in the field).
static USB_DROPPED_LINES: AtomicU32 = AtomicU32::new(0);

/// True while the stack holds a DHCP lease.
pub static WIFI_UP: AtomicBool = AtomicBool::new(false);
/// Current IPv4 address in big-endian byte order, or 0 for "none".
pub static WIFI_IP: AtomicU32 = AtomicU32::new(0);
/// Sentinel for "no RSSI reading available".
pub const RSSI_UNKNOWN: i32 = i32::MIN;
/// Last RSSI sample in dBm, or [`RSSI_UNKNOWN`].
pub static WIFI_RSSI: AtomicI32 = AtomicI32::new(RSSI_UNKNOWN);

/// The shared line ring.
pub fn line_bus() -> &'static SnifferBus {
    &LINES
}

/// The bus parameters currently in force.
pub fn current_bus() -> BusConfig {
    BUS.lock(|bus| bus.get())
}

/// Count lines the TCP consumer had to skip past.
pub fn count_dropped_lines(missed: u64) {
    DROPPED_LINES.fetch_add(u32::try_from(missed).unwrap_or(u32::MAX), Ordering::Relaxed);
}

/// Publish one ready-made line (including its CRLF) into the ring.
pub fn publish_text(line: &str) {
    if !LINES.publish(line.as_bytes()) {
        DROPPED_LINES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Format one captured frame and publish it, with any pending frame-loss marker
/// ahead of it so the loss shows up in temporal order.
fn publish_frame(buf: &[u8], timestamp_ms: u64) {
    let frame = sniffer::make_frame(buf, timestamp_ms);
    FRAMES.fetch_add(1, Ordering::Relaxed);
    if frame.data.len() >= 4 && !frame.crc_ok {
        BAD_CRC.fetch_add(1, Ordering::Relaxed);
    }

    let mut line = Line::new();
    if sniffer::format_frame(&frame, &mut line).is_err() {
        // Line buffer too small for this frame: count it as lost rather than
        // emitting a truncated, lying line. The marker goes out with the next
        // frame that does format.
        sniffer::DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let dropped = sniffer::take_dropped();
    if dropped > 0 {
        let mut marker = Marker::new();
        if sniffer::format_dropped_marker(dropped, &mut marker).is_ok() {
            publish_text(&marker);
        }
    }
    publish_text(&line);
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[esp_hal::main]
async fn main(spawner: Spawner) {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // esp-radio allocates; the allocator has to exist before the radio starts.
    esp_alloc::heap_allocator!(size: HEAP_SIZE);

    // esp-rtos owns the embassy-time driver, so it has to be running before
    // anything awaits a Timer, and before esp-radio initialises (the radio
    // driver needs the scheduler). The #[main] macro has already put us inside
    // its thread-mode executor; starting the scheduler from the main task is
    // the documented order.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // Bus settings from flash (or the default) before anything uses them.
    let bus = settings::init(peripherals.FLASH).await;
    BUS.lock(|current| current.set(bus));

    let (usb_rx, usb_tx) = UsbSerialJtag::new(peripherals.USB_DEVICE)
        .into_async()
        .split();
    spawner.spawn(usb_tx_task(usb_tx).unwrap());
    spawner.spawn(usb_rx_task(usb_rx).unwrap());

    match WIFI_SSID {
        Some(ssid) => {
            net::start(&spawner, peripherals.WIFI, ssid, WIFI_PASS.unwrap_or(""));
        }
        None => publish_text("# wifi disabled (no SNIFFER_WIFI_SSID at build time)\r\n"),
    }

    #[cfg(not(feature = "selftest"))]
    spawner.spawn(sniffer_task(peripherals.UART1, peripherals.GPIO4, bus).unwrap());

    #[cfg(feature = "selftest")]
    spawner.spawn(selftest_task().unwrap());
}

// ---------------------------------------------------------------------------
// USB-Serial-JTAG consumer
// ---------------------------------------------------------------------------

#[embassy_executor::task]
async fn usb_tx_task(mut tx: UsbSerialJtagTx<'static, Async>) {
    let mut cursor = 0u64;
    let mut buf = [0u8; sniffer::MAX_LINE_LEN];
    let mut marker = Marker::new();

    loop {
        // If no host is draining the port the writes below stall. That only
        // holds up THIS cursor: the sniffer and the TCP consumer carry on, and
        // the ring eventually overtakes us, which shows up as a
        // "[DROPPED n lines]" marker once a host shows up again.
        let out = select(
            LINES.next(sniffer::CONSUMER_USB, &mut cursor, &mut buf),
            USB_REPLIES.receive(),
        )
        .await;

        let written = match out {
            Either::First(Chunk::Line(len)) => tx.write_all(&buf[..len]).await,
            Either::First(Chunk::Dropped(missed)) => {
                USB_DROPPED_LINES
                    .fetch_add(u32::try_from(missed).unwrap_or(u32::MAX), Ordering::Relaxed);
                if sniffer::format_dropped_lines(missed, &mut marker).is_ok() {
                    tx.write_all(marker.as_bytes()).await
                } else {
                    Ok(())
                }
            }
            // `next` only returns Empty if it is told not to wait.
            Either::First(Chunk::Empty) => Ok(()),
            Either::Second(reply) => tx.write_all(reply.as_bytes()).await,
        };
        if written.is_ok() {
            let _ = tx.flush().await;
        }
    }
}

#[embassy_executor::task]
async fn usb_rx_task(mut rx: UsbSerialJtagRx<'static, Async>) {
    let mut command = CommandBuffer::new();
    let mut chunk = [0u8; 64];

    loop {
        match rx.read(&mut chunk).await {
            Ok(n) => {
                for &byte in &chunk[..n] {
                    if let Some(reply) = command.feed(byte).await {
                        USB_REPLIES.send(reply).await;
                    }
                }
            }
            // A receive error here is not fatal; back off so a stuck peripheral
            // cannot spin the CPU.
            Err(_) => Timer::after(Duration::from_millis(100)).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Reassembles LF- or CRLF-terminated command lines from a byte stream.
pub struct CommandBuffer {
    buf: [u8; sniffer::MAX_COMMAND_LEN],
    len: usize,
    /// Set when the line overran the buffer, so the whole line is rejected
    /// instead of being silently truncated into a different command.
    overflow: bool,
}

impl CommandBuffer {
    pub const fn new() -> Self {
        Self {
            buf: [0u8; sniffer::MAX_COMMAND_LEN],
            len: 0,
            overflow: false,
        }
    }

    /// Feed one received byte. Returns the reply line once a complete command
    /// has been handled.
    pub async fn feed(&mut self, byte: u8) -> Option<Line> {
        if byte != b'\r' && byte != b'\n' {
            if self.len == self.buf.len() {
                self.overflow = true;
            } else {
                self.buf[self.len] = byte;
                self.len += 1;
            }
            return None;
        }

        let len = self.len;
        let overflow = self.overflow;
        self.len = 0;
        self.overflow = false;

        if overflow {
            return Some(reply_err("command too long"));
        }
        if len == 0 {
            // The second half of a CRLF, or a blank line. Not a command.
            return None;
        }

        match core::str::from_utf8(&self.buf[..len]) {
            Ok(text) => Some(execute(text).await),
            Err(_) => Some(reply_err("command is not valid UTF-8")),
        }
    }
}

impl Default for CommandBuffer {
    fn default() -> Self {
        Self::new()
    }
}

fn reply_err(reason: &str) -> Line {
    let mut reply = Line::new();
    if sniffer::format_command_err(reason, &mut reply).is_err() {
        reply.clear();
        let _ = reply.push_str("# err reply too long\r\n");
    }
    reply
}

/// Parse and run one command line, returning the reply for its requester.
async fn execute(text: &str) -> Line {
    let command = match sniffer::parse_command(text) {
        Ok(command) => command,
        Err(reason) => return reply_err(reason),
    };

    let mut reply = Line::new();

    match command {
        Command::Status => {
            let status = sniffer::Status {
                uptime_ms: Instant::now().as_millis(),
                bus: current_bus(),
                wifi_up: WIFI_UP.load(Ordering::Relaxed),
                ip: match WIFI_IP.load(Ordering::Relaxed) {
                    0 => None,
                    raw => Some(raw.to_be_bytes()),
                },
                rssi: match WIFI_RSSI.load(Ordering::Relaxed) {
                    RSSI_UNKNOWN => None,
                    rssi => Some(rssi),
                },
                frames: FRAMES.load(Ordering::Relaxed),
                bad_crc: BAD_CRC.load(Ordering::Relaxed),
                uart_errors: UART_ERRORS.load(Ordering::Relaxed),
                dropped: DROPPED_LINES.load(Ordering::Relaxed),
                usb_dropped: USB_DROPPED_LINES.load(Ordering::Relaxed),
            };
            if sniffer::format_status(&status, &mut reply).is_err() {
                return reply_err("status line too long");
            }
        }

        Command::Note(text) => {
            let mut line = Line::new();
            if sniffer::format_note(Instant::now().as_millis(), text, &mut line).is_ok() {
                publish_text(&line);
            }
            if sniffer::format_command_ok(&command, &mut reply).is_err() {
                return reply_err("note too long");
            }
        }

        Command::Bus(new_bus) => {
            // Apply first: the live effect is the point of the command. The
            // sniffer task picks the change up through BUS_CHANGED and
            // re-applies the UART configuration; in a selftest build nobody is
            // listening, which is harmless.
            BUS.lock(|current| current.set(new_bus));
            BUS_CHANGED.signal(new_bus);

            let mut line = Line::new();
            if sniffer::format_bus_line(new_bus, &mut line).is_ok() {
                publish_text(&line);
            }

            // Then persist. A board with no settings partition still honours
            // the change for this session and says that it did not stick.
            if let Err(reason) = settings::store(new_bus).await {
                return reply_err(reason);
            }
            if sniffer::format_command_ok(&command, &mut reply).is_err() {
                return reply_err("reply too long");
            }
        }
    }

    reply
}

// ---------------------------------------------------------------------------
// RS-485 sniffer task
// ---------------------------------------------------------------------------

/// UART1 settings for a bus configuration.
#[cfg(not(feature = "selftest"))]
fn uart_config(bus: BusConfig) -> UartConfig {
    let (parity, stop_bits) = match bus.format {
        sniffer::BusFormat::N1 => (Parity::None, StopBits::_1),
        sniffer::BusFormat::E1 => (Parity::Even, StopBits::_1),
        sniffer::BusFormat::O1 => (Parity::Odd, StopBits::_1),
        sniffer::BusFormat::N2 => (Parity::None, StopBits::_2),
    };
    UartConfig::default()
        .with_baudrate(bus.baud)
        .with_data_bits(DataBits::_8)
        .with_parity(parity)
        .with_stop_bits(stop_bits)
        .with_rx(
            RxConfig::default()
                .with_fifo_full_threshold(RX_FIFO_THRESHOLD)
                .with_timeout(RX_IDLE_SYMBOLS),
        )
}

/// UART1 with no TX pin assigned - the sniffer must not drive the bus, and the
/// auto-direction RS-485 module has no DE/RE pin to hold down.
///
/// This uses the full `Uart` driver rather than `UartRx` on purpose: only
/// `Uart::apply_config` applies the *common* settings (baud rate, data bits,
/// parity, stop bits). `UartRx::apply_config` touches the RX-side settings
/// only, so a `bus` command through it changed nothing and the next frames came
/// back as parity errors and one-byte SHORT lines - seen on hardware.
#[cfg(not(feature = "selftest"))]
#[embassy_executor::task]
async fn sniffer_task(
    uart: esp_hal::peripherals::UART1<'static>,
    rx_pin: RxPin,
    initial: BusConfig,
) {
    // with_rx() also enables the pin's internal pull-up, so a disconnected or
    // loose RX wire idles high (silence) instead of floating into noise frames.
    let mut uart = Uart::new(uart, uart_config(initial))
        .unwrap()
        .with_rx(rx_pin)
        .into_async();

    let mut bus = initial;
    loop {
        let requested = frame_receiver(&mut uart, bus).await;
        if uart.apply_config(&uart_config(requested)).is_ok() {
            bus = requested;
        } else {
            // The hardware refused it. Put the old setting back where it is
            // visible - in every consumer's log and in `status` - rather than
            // reporting a configuration the UART is not actually using.
            BUS.lock(|current| current.set(bus));
            let mut line = Line::new();
            if sniffer::format_bus_line(bus, &mut line).is_ok() {
                publish_text(&line);
            }
        }
    }
}

/// Capture frames until the bus configuration changes; returns the new one.
#[cfg(not(feature = "selftest"))]
async fn frame_receiver(uart: &mut Uart<'static, Async>, bus: BusConfig) -> BusConfig {
    let frame_timeout = Duration::from_micros(bus.frame_timeout_us());

    let mut buf = heapless::Vec::<u8, { sniffer::MAX_FRAME_LEN }>::new();
    let mut chunk = [0u8; 64];

    loop {
        match select3(
            uart.read_async(&mut chunk),
            Timer::after(frame_timeout),
            BUS_CHANGED.wait(),
        )
        .await
        {
            Either3::First(Ok(n)) => {
                for &b in &chunk[..n] {
                    if buf.push(b).is_err() {
                        // Frame overflow; drop it and start over.
                        buf.clear();
                        let _ = buf.push(b);
                    }
                }
            }
            Either3::First(Err(_)) => {
                // UART error (framing/noise/parity/overflow). Flush the partial
                // frame if any, matching the STM32 firmware exactly.
                UART_ERRORS.fetch_add(1, Ordering::Relaxed);
                if !buf.is_empty() {
                    publish_frame(&buf, Instant::now().as_millis());
                    buf.clear();
                }
            }
            Either3::Second(_) => {
                // Silence detected -> frame boundary.
                if !buf.is_empty() {
                    publish_frame(&buf, Instant::now().as_millis());
                    buf.clear();
                }
            }
            Either3::Third(requested) => {
                // Close out whatever is in hand at the old settings first.
                if !buf.is_empty() {
                    publish_frame(&buf, Instant::now().as_millis());
                }
                return requested;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Self-test task (feature = "selftest")
//
// Injects canned Modbus RTU frames through the same producer the real sniffer
// uses, so the output paths and frame formatting can be verified without a live
// RS-485 bus. The UART is not initialised at all in this build; WiFi, the TCP
// server and the commands all still work.
// ---------------------------------------------------------------------------

#[cfg(feature = "selftest")]
#[embassy_executor::task]
async fn selftest_task() {
    loop {
        // 1) Master request: read 10 holding registers from slave 1.
        publish_frame(
            &sniffer::build_frame(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x0a]),
            Instant::now().as_millis(),
        );
        Timer::after(Duration::from_millis(500)).await;

        // 2) Slave response: 20 bytes of register data.
        publish_frame(
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
        publish_frame(&bad, Instant::now().as_millis());
        Timer::after(Duration::from_millis(1000)).await;
    }
}
