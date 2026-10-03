#![no_std]
#![no_main]

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
use embassy_time::{Duration, Instant, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::driver::EndpointError;
use embassy_usb::Builder;
use panic_halt as _;
#[cfg(not(feature = "selftest"))]
use static_cell::StaticCell;

use modbus_sniffer_core as sniffer;
use sniffer::{Line, Marker, DROPPED, FRAME_CHANNEL};

// ---------------------------------------------------------------------------
// User configuration
// ---------------------------------------------------------------------------

/// RS-485 baud rate. The 3.5 character timeout is derived from this.
#[cfg(not(feature = "selftest"))]
const BAUD_RATE: u32 = 9_600;

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
        config.rcc.pll_src = PllSource::HSE;
        config.rcc.pll = Some(Pll {
            prediv: PllPreDiv::DIV25,
            mul: PllMul::MUL192,
            divp: Some(PllPDiv::DIV2),
            divq: Some(PllQDiv::DIV4),
            divr: None,
        });
        config.rcc.ahb_pre = AHBPrescaler::DIV1;
        config.rcc.apb1_pre = APBPrescaler::DIV2;
        config.rcc.apb2_pre = APBPrescaler::DIV1;
        config.rcc.sys = Sysclk::PLL1_P;
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
    let mut line = Line::new();
    let mut marker = Marker::new();

    loop {
        let frame = FRAME_CHANNEL.receive().await;

        // Report any frames lost since the last successful send, inline and in
        // temporal order, before the frame that follows them.
        let dropped = sniffer::take_dropped();
        if dropped > 0 && sniffer::format_dropped_marker(dropped, &mut marker).is_ok() {
            // Short packet (< 64 bytes) flushes on the host; no ZLP needed.
            for chunk in marker.as_bytes().chunks(CDC_MAX_PACKET) {
                class.write_packet(chunk).await?;
            }
        }

        if sniffer::format_frame(&frame, &mut line).is_err() {
            // Line buffer too small for this frame: count it as lost rather
            // than emitting a truncated, lying line.
            DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            continue;
        }
        let bytes = line.as_bytes();

        // CDC bulk IN endpoint on full-speed is limited to 64 bytes/packet.
        for chunk in bytes.chunks(CDC_MAX_PACKET) {
            class.write_packet(chunk).await?;
        }

        // If the line is an exact multiple of the packet size, send a ZLP
        // so the host flushes the buffered line to the terminal.
        if bytes.len().is_multiple_of(CDC_MAX_PACKET) {
            class.write_packet(&[]).await?;
        }
    }
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
    // BAUD_RATE is a const and frame_timeout_us is a const fn, so this folds at
    // compile time.
    let frame_timeout = Duration::from_micros(sniffer::frame_timeout_us(BAUD_RATE));

    let mut buf = heapless::Vec::<u8, { sniffer::MAX_FRAME_LEN }>::new();
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
// Injects canned Modbus RTU frames into the same channel the real sniffer
// uses, so the USB output path and frame formatting can be verified without
// a live RS-485 bus. Builds a couple of valid frames (correct CRC) plus one
// deliberately corrupted frame to exercise the BAD_CRC path.
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
