//! Status LED: the DevKitC-1's on-board addressable RGB LED (WS2812-type) on
//! GPIO8, driven by RMT TX channel 0 through `esp-hal-smartled`.
//!
//! The rule here is that nothing on the capture path may ever wait for the LED.
//! Producers only touch atomics - [`flash`] is one `fetch_or` plus a `Signal`
//! poke - and the single [`led_task`] is the only code that talks to the RMT
//! peripheral. If the LED cannot be brought up at all, the task returns and the
//! sniffer carries on exactly as before.
//!
//! GPIO8 is a strapping pin, but it is also the board's LED data line: it is
//! only sampled at reset, so driving it afterwards is harmless.
//!
//! States (see the README table):
//!
//! - WiFi configured but not associated (also: booting, reconnecting): blue
//!   blink at ~2 Hz.
//! - Associated, no TCP client: steady dim amber.
//! - Associated, TCP client connected: steady dim green.
//! - WiFi not available in this build (or the radio refused to start): steady
//!   dim cyan.
//! - On every captured frame, a short flash over the steady colour: bright green
//!   for a good CRC, bright red for BAD_CRC/SHORT frames and UART errors.
//!
//! Panics do NOT turn the LED red: the panic handler comes from esp-backtrace
//! and the RMT channel is owned by the LED task, so colouring the LED from a
//! panic would mean replacing that handler (and losing the backtrace) for a
//! cosmetic win. Not done on purpose.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::time::Rate;
use esp_hal_smartled::{buffer_size, color_order, RmtSmartLeds, WS2812_TIMING};
use smart_leds::{SmartLedsWriteAsync, RGB8};

/// Steady-state brightness, out of 255. Low on purpose: this board sits in a
/// dark poolhouse, and the LED is a status indicator, not a lamp. Every colour
/// below is derived from this one number.
const DIM: u8 = 20;

/// Brightness for the per-frame flashes, so they stand out against the steady
/// colour without lighting up the room.
const BRIGHT: u8 = 140;

/// How long one frame flash is shown.
const FLASH_MS: u64 = 35;

/// Shortest interval between the starts of two flashes. Continuous bus traffic
/// therefore reads as a flicker (35 ms lit, 45 ms steady) instead of a solid
/// colour, and the flash rate stops telling you anything above ~12 frames/s -
/// which is the point: it says "traffic", not "how much".
const FLASH_PERIOD_MS: u64 = 80;

/// Half period of the "not associated" blink: 250 ms on, 250 ms off = 2 Hz.
/// Doubles as the state poll interval, so a WiFi or client change shows up
/// within this long at worst.
const BLINK_HALF_MS: u64 = 250;

/// RMT counter clock. The driver converts the WS2812 timings into pulse widths
/// with the frequency the peripheral actually settled on, so this only has to be
/// reachable from the RMT source clock.
const RMT_FREQ: Rate = Rate::from_mhz(80);

/// One LED on the data line.
const LED_COUNT: usize = 1;

/// Pin the board wires to the RGB LED's data input.
type LedPin = esp_hal::peripherals::GPIO8<'static>;

const OFF: RGB8 = RGB8 { r: 0, g: 0, b: 0 };
const BLINK_BLUE: RGB8 = scaled(0, 0, 255, DIM);
const STEADY_AMBER: RGB8 = scaled(255, 140, 0, DIM);
const STEADY_GREEN: RGB8 = scaled(0, 255, 0, DIM);
const STEADY_CYAN: RGB8 = scaled(0, 255, 255, DIM);
const FLASH_GOOD: RGB8 = scaled(0, 255, 0, BRIGHT);
const FLASH_BAD: RGB8 = scaled(255, 0, 0, BRIGHT);

/// Scale a full-brightness colour down to `level` out of 255.
const fn scaled(r: u8, g: u8, b: u8, level: u8) -> RGB8 {
    RGB8 {
        r: ((r as u16 * level as u16) / 255) as u8,
        g: ((g as u16 * level as u16) / 255) as u8,
        b: ((b as u16 * level as u16) / 255) as u8,
    }
}

/// Pending-flash flags, OR-ed together by the producers and cleared by the LED
/// task. Bad beats good when both land inside one flash window.
const PENDING_GOOD: u8 = 1 << 0;
const PENDING_BAD: u8 = 1 << 1;

static PENDING: AtomicU8 = AtomicU8::new(0);
static WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// False for a build with no WiFi credentials, or if the radio refused to start.
static WIFI_AVAILABLE: AtomicBool = AtomicBool::new(false);
/// True while a TCP client is being served on the line server.
static CLIENT_CONNECTED: AtomicBool = AtomicBool::new(false);

/// Record whether this firmware has WiFi at all.
pub fn set_wifi_available(available: bool) {
    WIFI_AVAILABLE.store(available, Ordering::Relaxed);
    WAKE.signal(());
}

/// Record whether the TCP line server currently has a client.
pub fn set_client_connected(connected: bool) {
    CLIENT_CONNECTED.store(connected, Ordering::Relaxed);
    WAKE.signal(());
}

/// Ask for one frame flash. `bad` picks red (bad CRC, short frame, UART error)
/// over green. Called straight from the capture path, so it does no I/O and
/// never waits.
pub fn flash(bad: bool) {
    let flag = if bad { PENDING_BAD } else { PENDING_GOOD };
    PENDING.fetch_or(flag, Ordering::Relaxed);
    WAKE.signal(());
}

/// Start the LED task. Takes the RMT peripheral and the LED pin for good.
pub fn start(spawner: &Spawner, rmt: esp_hal::peripherals::RMT<'static>, pin: LedPin) {
    spawner.spawn(led_task(rmt, pin).unwrap());
}

/// The steady colour for the current state, given the blink phase.
fn steady_colour(blink_on: bool) -> RGB8 {
    if !WIFI_AVAILABLE.load(Ordering::Relaxed) {
        return STEADY_CYAN;
    }
    if !crate::WIFI_UP.load(Ordering::Relaxed) {
        return if blink_on { BLINK_BLUE } else { OFF };
    }
    if CLIENT_CONNECTED.load(Ordering::Relaxed) {
        STEADY_GREEN
    } else {
        STEADY_AMBER
    }
}

/// Blink phase derived from the clock rather than from a counter, so the blink
/// keeps time across however many flashes interrupt it.
fn blink_on(now_ms: u64) -> bool {
    (now_ms / BLINK_HALF_MS).is_multiple_of(2)
}

#[embassy_executor::task]
async fn led_task(rmt: esp_hal::peripherals::RMT<'static>, pin: LedPin) {
    let Ok(rmt) = esp_hal::rmt::Rmt::new(rmt, RMT_FREQ) else {
        return;
    };
    let rmt = rmt.into_async();
    // Read the settled counter clock before channel0 is moved out.
    let frequency = rmt.frequency();

    // The on-board LED takes its channels green-red-blue on the wire.
    let Ok(mut led) = RmtSmartLeds::<
        'static,
        { buffer_size::<RGB8>(LED_COUNT) },
        esp_hal::Async,
        RGB8,
        color_order::Grb,
    >::new(WS2812_TIMING, rmt.channel0, pin, frequency) else {
        return;
    };

    // Last colour actually sent, so a steady state is written once and then left
    // alone instead of being re-transmitted four times a second.
    let mut shown: Option<RGB8> = None;
    let mut next_flash = Instant::from_millis(0);

    loop {
        let now = Instant::now();
        let steady = steady_colour(blink_on(now.as_millis()));

        // A flash pending, and far enough from the last one to be seen as one?
        let pending = PENDING.swap(0, Ordering::Relaxed);
        if pending != 0 && now >= next_flash {
            let colour = if pending & PENDING_BAD != 0 {
                FLASH_BAD
            } else {
                FLASH_GOOD
            };
            show(&mut led, &mut shown, colour).await;
            Timer::after(Duration::from_millis(FLASH_MS)).await;
            // Back to the steady colour for the rest of the window, so the next
            // flash has something to stand out against.
            let after = Instant::now();
            let steady = steady_colour(blink_on(after.as_millis()));
            show(&mut led, &mut shown, steady).await;
            next_flash = now + Duration::from_millis(FLASH_PERIOD_MS);
            continue;
        }

        show(&mut led, &mut shown, steady).await;

        // Sleep until the next blink edge at the latest; a producer can cut the
        // wait short. Flashes that arrive while rate-limited are dropped by the
        // swap above, not queued.
        let to_edge = BLINK_HALF_MS - (now.as_millis() % BLINK_HALF_MS);
        let _ = select(WAKE.wait(), Timer::after(Duration::from_millis(to_edge))).await;
    }
}

/// Transmit one colour, unless it is already on the LED.
async fn show<const N: usize>(
    led: &mut RmtSmartLeds<'static, N, esp_hal::Async, RGB8, color_order::Grb>,
    shown: &mut Option<RGB8>,
    colour: RGB8,
) {
    if *shown == Some(colour) {
        return;
    }
    // A failed transmit is not worth reporting into the capture stream; retry
    // on the next pass by leaving `shown` alone.
    if led.write([colour]).await.is_ok() {
        *shown = Some(colour);
    }
}
