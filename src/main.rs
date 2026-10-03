#![no_std]
#![no_main]

use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
#[cfg(not(feature = "selftest"))]
use embassy_futures::select::{select, Either};
#[cfg(not(feature = "selftest"))]
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::peripherals;
use embassy_stm32::time::Hertz;
#[cfg(not(feature = "selftest"))]
use embassy_stm32::usart::{Config as UartConfig, RingBufferedUartRx, UartRx};
use embassy_stm32::usb::{self, Driver};
use embassy_stm32::{bind_interrupts, dma, usart, Config};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::driver::EndpointError;
use embassy_usb::Builder;
use heapless::String;
use panic_halt as _;
#[cfg(not(feature = "selftest"))]
use static_cell::StaticCell;

mod modbus;
use modbus::{crc16, format_function};

// ---------------------------------------------------------------------------
// User configuration
// ---------------------------------------------------------------------------

/// RS-485 baud rate. The 3.5 character timeout is derived from this.
#[cfg(not(feature = "selftest"))]
const BAUD_RATE: u32 = 9_600;

/// Max raw Modbus RTU frame size (address + function + 252 data + 2 CRC = 256).
const MAX_FRAME_LEN: usize = 256;

/// USB CDC output buffer for one formatted line.
const USB_BUF_LEN: usize = 1024;

// ---------------------------------------------------------------------------
// Shared channel: sniffed frames -> USB task
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Frame {
    pub timestamp_ms: u64,
    pub data: heapless::Vec<u8, MAX_FRAME_LEN>,
    pub crc_ok: bool,
}

static FRAME_CHANNEL: Channel<CriticalSectionRawMutex, Frame, 8> = Channel::new();

/// Frames lost because the channel was full when the sniffer tried to enqueue.
/// Surfaced inline on the USB stream as a "[DROPPED n frames]" marker so the
/// analyzer never lies by omission.
static DROPPED: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Interrupt bindings
// ---------------------------------------------------------------------------

bind_interrupts!(struct Irqs {
    OTG_FS => usb::InterruptHandler<peripherals::USB_OTG_FS>;
    USART2 => usart::InterruptHandler<peripherals::USART2>;
    DMA1_STREAM5 => dma::InterruptHandler<peripherals::DMA1_CH5>;
});

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config = Config::default();
    {
        use embassy_stm32::rcc::*;
        config.rcc.hse = Some(Hse {
            freq: Hertz(25_000_000),
            mode: HseMode::Oscillator,
        });
        config.rcc.pll_src = PllSource::Hse;
        config.rcc.pll = Some(Pll {
            prediv: PllPreDiv::Div25,
            mul: PllMul::Mul192,
            divp: Some(PllPDiv::Div2),
            divq: Some(PllQDiv::Div4),
            divr: None,
        });
        config.rcc.ahb_pre = AHBPrescaler::Div1;
        config.rcc.apb1_pre = APBPrescaler::Div2;
        config.rcc.apb2_pre = APBPrescaler::Div1;
        config.rcc.sys = Sysclk::Pll1P;
    }

    let p = embassy_stm32::init(config);

    spawner.spawn(usb_task(p.USB_OTG_FS, p.PA12, p.PA11).unwrap());

    #[cfg(not(feature = "selftest"))]
    spawner.spawn(sniffer_task(p.USART2, p.DMA1_CH5, p.PA3, p.PA1).unwrap());

    #[cfg(feature = "selftest")]
    {
        // UART peripherals are intentionally unused in self-test mode.
        let _ = (p.USART2, p.DMA1_CH5, p.PA3, p.PA1);
        spawner.spawn(selftest_task().unwrap());
    }
}

// ---------------------------------------------------------------------------
// USB CDC task
// ---------------------------------------------------------------------------

#[embassy_executor::task]
async fn usb_task(
    usb_otg_fs: embassy_stm32::Peri<'static, peripherals::USB_OTG_FS>,
    dp: embassy_stm32::Peri<'static, peripherals::PA12>,
    dm: embassy_stm32::Peri<'static, peripherals::PA11>,
) {
    let mut ep_out_buffer = [0u8; 256];
    let mut usb_config = embassy_stm32::usb::Config::default();
    usb_config.vbus_detection = false; // works on all boards

    let driver = Driver::new_fs(usb_otg_fs, Irqs, dp, dm, &mut ep_out_buffer, usb_config);

    let mut config_descriptor = [0u8; 256];
    let mut bos_descriptor = [0u8; 256];
    let mut msos_descriptor = [0u8; 256];
    let mut control_buf = [0u8; 64];
    let mut state = State::new();

    let mut usb_desc = embassy_usb::Config::new(0xc0de, 0xcafe);
    usb_desc.manufacturer = Some("prppl");
    usb_desc.product = Some("Modbus RTU Sniffer");
    usb_desc.serial_number = Some("modbus-sniffer-1");

    let mut builder = Builder::new(
        driver,
        usb_desc,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut msos_descriptor,
        &mut control_buf,
    );

    let mut class = CdcAcmClass::new(&mut builder, &mut state, 64);

    let mut usb = builder.build();
    let usb_fut = usb.run();
    let cdc_fut = async {
        loop {
            class.wait_connection().await;
            let _ = cdc_loop(&mut class).await;
        }
    };

    embassy_futures::join::join(usb_fut, cdc_fut).await;
}

const CDC_MAX_PACKET: usize = 64;

async fn cdc_loop<'d>(
    class: &mut CdcAcmClass<'d, Driver<'d, peripherals::USB_OTG_FS>>,
) -> Result<(), Disconnected> {
    let mut buf = [0u8; USB_BUF_LEN];

    loop {
        let frame = FRAME_CHANNEL.receive().await;

        // Report any frames lost since the last successful send, inline and in
        // temporal order, before the frame that follows them.
        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let mut marker: String<32> = String::new();
            let _ = core::write!(&mut marker, "[DROPPED {dropped} frames]\r\n");
            // Short packet (< 64 bytes) flushes on the host; no ZLP needed.
            for chunk in marker.as_bytes().chunks(CDC_MAX_PACKET) {
                class.write_packet(chunk).await?;
            }
        }

        let line = format_frame(&frame, &mut buf)?;

        // CDC bulk IN endpoint on full-speed is limited to 64 bytes/packet.
        for chunk in line.chunks(CDC_MAX_PACKET) {
            class.write_packet(chunk).await?;
        }

        // If the line is an exact multiple of the packet size, send a ZLP
        // so the host flushes the buffered line to the terminal.
        if line.len() % CDC_MAX_PACKET == 0 {
            class.write_packet(&[]).await?;
        }
    }
}

fn format_frame<'a>(
    frame: &Frame,
    buf: &'a mut [u8; USB_BUF_LEN],
) -> Result<&'a [u8], Disconnected> {
    let mut s: String<USB_BUF_LEN> = String::new();
    let secs = frame.timestamp_ms / 1000;
    let ms = frame.timestamp_ms % 1000;

    if frame.data.len() < 4 {
        core::write!(
            &mut s,
            "[{secs}.{ms:03}] SHORT ({} bytes): ",
            frame.data.len()
        )
        .map_err(|_| Disconnected)?;
        for b in &frame.data {
            core::write!(&mut s, "{b:02x} ").map_err(|_| Disconnected)?;
        }
        s.push('\r').map_err(|_| Disconnected)?;
        s.push('\n').map_err(|_| Disconnected)?;
        let line = s.as_bytes();
        let len = line.len().min(buf.len());
        buf[..len].copy_from_slice(&line[..len]);
        return Ok(&buf[..len]);
    }

    let slave = frame.data[0];
    let function = frame.data[1];
    let payload_len = frame.data.len().saturating_sub(4); // without addr, fn, 2 CRC
    let received_crc = u16::from_le_bytes([
        frame.data[frame.data.len() - 2],
        frame.data[frame.data.len() - 1],
    ]);
    let computed_crc = crc16(&frame.data[..frame.data.len() - 2]);

    core::write!(
        &mut s,
        "[{secs}.{ms:03}] RX slave=0x{slave:02x} func=0x{function:02x} ({})",
        format_function(function)
    )
    .map_err(|_| Disconnected)?;

    if payload_len > 0 {
        s.push_str(" data=").map_err(|_| Disconnected)?;
        for b in &frame.data[2..frame.data.len() - 2] {
            core::write!(&mut s, "{b:02x} ").map_err(|_| Disconnected)?;
        }
    }

    core::write!(
        &mut s,
        " crc=0x{received_crc:04x} computed=0x{computed_crc:04x} {}\r\n",
        if frame.crc_ok { "OK" } else { "BAD_CRC" }
    )
    .map_err(|_| Disconnected)?;

    let line = s.as_bytes();
    let len = line.len().min(buf.len());
    buf[..len].copy_from_slice(&line[..len]);
    Ok(&buf[..len])
}

#[derive(Debug, Clone, Copy)]
struct Disconnected;

impl From<EndpointError> for Disconnected {
    fn from(err: EndpointError) -> Self {
        match err {
            EndpointError::BufferOverflow => panic!("USB buffer overflow"),
            EndpointError::Disabled => Disconnected,
        }
    }
}

// ---------------------------------------------------------------------------
// RS-485 sniffer task
// ---------------------------------------------------------------------------

#[cfg(not(feature = "selftest"))]
#[embassy_executor::task]
async fn sniffer_task(
    usart: embassy_stm32::Peri<'static, peripherals::USART2>,
    dma: embassy_stm32::Peri<'static, peripherals::DMA1_CH5>,
    rx_pin: embassy_stm32::Peri<'static, peripherals::PA3>,
    de_pin: embassy_stm32::Peri<'static, peripherals::PA1>,
) {
    // Drive DE/RE low permanently -> receive-only mode.
    // For auto-direction modules you can leave this pin unconnected.
    let _de = Output::new(de_pin, Level::Low, Speed::Low);

    let mut uart_cfg = UartConfig::default();
    uart_cfg.baudrate = BAUD_RATE;
    uart_cfg.parity = embassy_stm32::usart::Parity::ParityEven;
    uart_cfg.stop_bits = embassy_stm32::usart::StopBits::STOP1;

    let rx = UartRx::new(usart, rx_pin, dma, Irqs, uart_cfg).unwrap();

    // Back the RX with a DMA ring buffer so the MCU can fall many character
    // times behind without losing bytes. The F411 USART has no hardware RX
    // FIFO; a 256-byte ring gives ~24 ms of slack at 115200 baud vs the single
    // character time a per-byte DMA read allowed.
    static RX_RING: StaticCell<[u8; 256]> = StaticCell::new();
    let mut rx = rx.into_ring_buffered(RX_RING.init([0u8; 256]));

    frame_receiver(&mut rx).await;
}

#[cfg(not(feature = "selftest"))]
async fn frame_receiver(rx: &mut RingBufferedUartRx<'static>) {
    // Modbus RTU inter-frame delay (t3.5) marks the frame boundary. The spec
    // defines it as 3.5 character times for baud <= 19200, but FIXES it at
    // 1750 us for higher rates (because 3.5 chars shrinks below the silence a
    // device needs). Use 11 bits/char (1 start + 8 data + even parity + 1 stop).
    // BAUD_RATE is a const, so this whole expression folds at compile time.
    let frame_timeout = if BAUD_RATE > 19_200 {
        Duration::from_micros(1_750)
    } else {
        let char_us: u64 = (11u64 * 1_000_000) / u64::from(BAUD_RATE);
        Duration::from_micros(char_us * 4) // 4 chars ~= 3.5 + margin
    };

    let mut buf = heapless::Vec::<u8, MAX_FRAME_LEN>::new();
    let mut chunk = [0u8; 64];

    loop {
        match select(rx.read(&mut chunk), Timer::after(frame_timeout)).await {
            Either::First(Ok(n)) => {
                for &b in &chunk[..n] {
                    if buf.push(b).is_err() {
                        // Frame overflow; drop it and start over.
                        buf.clear();
                        let _ = buf.push(b);
                    }
                }
            }
            Either::First(_) => {
                // UART error (framing/noise/parity). Background DMA is paused on
                // error; the next read() restarts it automatically. Flush the
                // partial frame if any, matching the original silence behaviour.
                if !buf.is_empty() {
                    emit_frame(&buf).await;
                    buf.clear();
                }
            }
            Either::Second(_) => {
                // Silence detected -> frame boundary.
                if !buf.is_empty() {
                    emit_frame(&buf).await;
                    buf.clear();
                }
            }
        }
    }
}

async fn emit_frame(buf: &heapless::Vec<u8, MAX_FRAME_LEN>) {
    let crc_ok = if buf.len() >= 4 {
        let received = u16::from_le_bytes([buf[buf.len() - 2], buf[buf.len() - 1]]);
        crc16(&buf[..buf.len() - 2]) == received
    } else {
        false
    };

    let mut data = heapless::Vec::<u8, MAX_FRAME_LEN>::new();
    for b in buf {
        let _ = data.push(*b);
    }

    let frame = Frame {
        timestamp_ms: Instant::now().as_millis(),
        data,
        crc_ok,
    };

    // Never block the sniffer on a stalled USB host: if the channel is full,
    // drop this frame and count it. The USB task emits a "[DROPPED n frames]"
    // marker so the loss is visible rather than silent.
    if FRAME_CHANNEL.try_send(frame).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Self-test task (feature = "selftest")
//
// Injects canned Modbus RTU frames into the same channel the real sniffer
// uses, so the USB output path and frame formatting can be verified without
// a live RS-485 bus. Builds a couple of valid frames (correct CRC) plus one
// deliberately corrupted frame to exercise the BAD_CRC path.
// ---------------------------------------------------------------------------

#[cfg(feature = "selftest")]
#[embassy_executor::task]
async fn selftest_task() {
    /// Build a frame from address+function+payload and append a valid CRC.
    fn build(bytes: &[u8]) -> heapless::Vec<u8, MAX_FRAME_LEN> {
        let mut v = heapless::Vec::<u8, MAX_FRAME_LEN>::new();
        for b in bytes {
            let _ = v.push(*b);
        }
        let crc = crc16(&v);
        let _ = v.push((crc & 0xff) as u8);
        let _ = v.push((crc >> 8) as u8);
        v
    }

    loop {
        // 1) Master request: read 10 holding registers from slave 1.
        emit_frame(&build(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x0a])).await;
        Timer::after(Duration::from_millis(500)).await;

        // 2) Slave response: 20 bytes of register data.
        emit_frame(&build(&[
            0x01, 0x03, 0x14, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00, 0x05, 0x00,
            0x06, 0x00, 0x07, 0x00, 0x08, 0x00, 0x09, 0x00, 0x0a,
        ]))
        .await;
        Timer::after(Duration::from_millis(500)).await;

        // 3) Deliberately corrupted frame -> exercises the BAD_CRC path.
        let mut bad = build(&[0x02, 0x06, 0x00, 0x10, 0x12, 0x34]);
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        emit_frame(&bad).await;
        Timer::after(Duration::from_millis(1000)).await;
    }
}
