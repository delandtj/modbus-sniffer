# Handoff: modbus-sniffer

Last updated: 2026-10-03

## What this is

Rust/Embassy firmware that passively sniffs a Modbus RTU bus over an RS-485
transceiver and streams decoded frames out the board's native USB port as a
virtual serial port (a frame logger / "protocol analyzer"). Two boards, one
Cargo workspace, one shared decode core.

Git repo, branch `main`. Everything below compiles warning-free.

## Layout

```text
Cargo.toml              workspace (resolver 2), holds [profile.release]
core/                   modbus-sniffer-core - no_std, std only under cfg(test)
fw-stm32f411/           sniffer-stm32f411   - STM32F411CEU6 Black Pill
fw-esp32c6/             sniffer-esp32c6     - ESP32-C6-DevKitC-1 (WROOM-1)
```

There is NO `.cargo/config.toml` at the workspace root - that is deliberate, so
`cargo test -p modbus-sniffer-core` from the root runs on the host. Each firmware
crate has its own `.cargo/config.toml` with its target and runner, so you build
and flash from inside that directory.

`core/` holds: `crc16`, `format_function`, `Frame`, `MAX_FRAME_LEN`,
`FRAME_CHANNEL` (embassy-sync Channel, depth 8), `DROPPED` (AtomicU32),
`emit_frame(buf, timestamp_ms)` (non-async; only try_sends - the timestamp is
passed in so core needs no embassy-time dependency), `format_frame` (writes
straight into a caller-supplied `Line`, no double buffering),
`format_dropped_marker`, `build_frame` and `const fn frame_timeout_us(baud)`.
13 host unit tests cover the CRC vector, the OK / BAD_CRC / SHORT / no-payload /
unknown-function line formats, worst-case line length, the marker text, t3.5 at
9600 / 19200 / 115200, and `emit_frame`'s channel-overflow accounting.

## Current state (IMPORTANT - read first)

- STM32F411: unchanged behaviour, compiles warning-free in both build configs.
  Last hardware verification was of the pre-workspace code (selftest build
  verified end-to-end on /dev/ttyACM0; default build flashed but its decode was
  never re-confirmed because of the USB hub incident below). The restructure has
  NOT been re-flashed.
- ESP32-C6: selftest build VERIFIED on hardware 2026-10-03 (ESP32-C6 rev v0.2,
  16MB flash, DevKitC-1 "USB" port). /dev/ttyACM0 shows the three canned frames
  with correct decode, both OK and BAD_CRC. The DROPPED path was also seen for
  real: with no reader attached the USB writes stall, the 8-deep channel fills
  and "[DROPPED 4 frames]" appeared inline once a reader reattached.
  Real sniffer build VERIFIED on a live RS-485 link 2026-10-03: an FTDI FT232R
  USB-RS485 adapter (/dev/ttyUSB0) drove 9600 8E1 frames into the isolated
  module -> GPIO4. Requests, responses, exception (0x83), BAD_CRC and a burst
  at 20 ms gaps all decoded correctly; a zero-gap burst merges into one frame,
  which is correct RTU behaviour. Not yet tested on a real Modbus device bus.
  Wiring lessons: GPIO4 floating looks like silence (check with a pull-up);
  A/B reversed shows as the line idling LOW with the data inverted.
  Gotcha: right after `espflash flash` (no --monitor) the app produced nothing
  until `espflash monitor` reset the chip; `cargo run --release` (which uses
  --monitor) avoids this, or press RESET.
- The embassy dependencies moved from git (embassy-rs/embassy main) to the
  equivalent crates.io releases. The versions matched exactly; the only source
  change needed was the RCC enum variant casing (`PllSource::HSE`,
  `Sysclk::PLL1_P`, `PllMul::MUL192`, `AHBPrescaler::DIV1`, ...). The clock tree,
  `Driver::new_fs`, `RingBufferedUartRx`/`into_ring_buffered` and the
  `spawn(task().unwrap())` idiom are all unchanged.

### Pinned versions

| Crate | Version |
|---|---|
| embassy-stm32 | 0.6.0 (crates.io) |
| embassy-executor | 0.10.0 |
| embassy-time | 0.5.1 |
| embassy-sync | 0.8.0 |
| embassy-usb | 0.6.0 |
| embassy-futures | 0.1.2 |
| esp-hal | 1.2.2 |
| esp-rtos | 0.4.0 (features esp32c6, embassy) |
| esp-backtrace | 0.20.0 |
| esp-println | 0.18 (no default features; esp32c6, jtag-serial, critical-section) |
| esp-bootloader-esp-idf | 0.6.0 |
| embedded-io-async | 0.7 |

One lockfile holds both firmwares. The hazard was `embassy-time-driver`'s
`links = "embassy-time"`: the resolver permits only one copy of that per
workspace. esp-rtos 0.4 wants embassy-executor ^0.10 / embassy-sync ^0.8 /
embassy-time-driver ^0.2, which is exactly what embassy-stm32 0.6.0 wants, so a
single embassy-time-driver 0.2.2 satisfies both. The OLD esp-hal-embassy 0.9.1
would NOT have worked - it still pins embassy-executor ^0.7 and embassy-time
^0.4. If a future bump breaks this, the fallback is to make each firmware crate
its own workspace with a path dep on `core/`.

## Hardware

### STM32F411 Black Pill

- Programmer: ST-Link/V2 over SWD (JTAG fails, SWD works - normal).
- The Black Pill's own micro-USB carries the CDC output (PA11/PA12), separate
  from the ST-Link. Both cables must be plugged in.
- 2026-06-23 incident: after `probe-rs download` succeeded, `probe-rs reset`
  triggered a USB enumeration failure (-110) and the retry storm crashed the
  shared USB hub, taking down the ST-Link, the Black Pill and the user's
  mic/receiver. Recovery was a physical replug. **Never `probe-rs reset` this
  board; cold-boot it by replugging its USB cable.**

### ESP32-C6-DevKitC-1

- Two USB-C ports. The one labelled **"USB"** is the chip's own USB-Serial-JTAG:
  it both flashes and carries the sniffer output (/dev/ttyACM0). The one labelled
  "UART" is the CP2102N bridge and is unused.
- No separate programmer needed.

### RS-485 module (same one for both boards)

szhjw "3.3V/5V TTL UART to RS485 Isolated, single chip, automatic flow control":
NO DE/RE pin (auto-direction), galvanically isolated - do NOT tie the bus ground
to the MCU ground.

| Module (TTL side) | STM32F411 | ESP32-C6 | Note |
|---|---|---|---|
| VCC | 3V3 | 3V3 | 3.3 V so the data line into the MCU is 3.3 V logic |
| GND | GND | GND | TTL-side ground only |
| RXD | PA3 | GPIO4 | bus data into the MCU |
| TXD | n/c | n/c | the sniffer never transmits |

Module A/B -> bus A/B (swap if silent; if still silent swap module RXD/TXD, the
TTL-side labelling convention varies). On the STM32 build PA1 is still driven low
as a leftover DE/RE output - harmless, leave it unconnected.

Bus settings chosen by the user: 9600 8E1. There is still no real Modbus bus to
test against - hence the selftest feature.

## Build / test / flash recipes

```bash
# Host unit tests for the shared core (from the workspace root)
cargo test -p modbus-sniffer-core

# STM32F411
cd fw-stm32f411
cargo build --release
cargo build --release --features selftest
probe-rs download --chip STM32F411CE <target-dir>/thumbv7em-none-eabihf/release/sniffer-stm32f411
# then COLD-BOOT by physically replugging the Black Pill USB. Not `probe-rs reset`.

# ESP32-C6 (build only so far)
cd fw-esp32c6
cargo build --release
cargo build --release --features selftest
cargo run --release        # espflash flash --monitor --chip esp32c6, over the "USB" port

# Read output on either board
minicom -D /dev/ttyACM0    # baud irrelevant for USB-CDC / USB-Serial-JTAG
```

Note the user has a global `build.target-dir` in `~/.cargo/config.toml`, so the
ELF is not under `./target`. `cargo metadata --format-version 1 --no-deps` reports
the real `target_directory`.

## ESP32-C6 firmware notes

- esp-hal 1.2.2 + esp-rtos 0.4.0 `embassy` feature. `#[esp_hal::main] async fn
  main(spawner: Spawner)` expands to an `esp_rtos::embassy::Executor`, so
  `embassy-executor` must be a direct dependency (the macro names
  `::embassy_executor::task`) with NO `platform-*` feature enabled - esp-rtos
  provides `__pender`. `esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0)`
  is called from inside the main task, before anything awaits a Timer, because
  esp-rtos owns the embassy-time driver.
- `esp_bootloader_esp_idf::esp_app_desc!()` at crate root; without the app
  descriptor the second-stage bootloader rejects the image.
- `.cargo/config.toml` needs `-C link-arg=-Tlinkall.x` (esp-hal's build script
  only puts the scripts in OUT_DIR) and `-C force-frame-pointers` (esp-backtrace
  on RISC-V falls back to a raw stack dump without it).
- CPU at `CpuClock::max()` = 160 MHz. Watchdogs are disabled by `esp_hal::init`.
- Output: `UsbSerialJtag::new(peripherals.USB_DEVICE).into_async().split()`, Tx
  half handed to the output task. `write_async`/`flush_tx_async` are private, so
  the traits come from `embedded_io_async::Write` (esp-hal's `unstable` feature
  enables that impl).
- Input: `UartRx::new(UART1, config).with_rx(GPIO4).into_async()`, 9600 8E1, RX
  FIFO full threshold 1, hardware RX idle timeout 2 symbol times. The frame
  boundary is `select(read_async, Timer::after(frame_timeout_us(BAUD_RATE)))`,
  and the timer restarts each time a read returns, so reads MUST return per
  byte. The original threshold of 32 split every frame on hardware: the read
  stayed pending mid-frame, the t3.5 timer fired while bytes were still
  arriving, and the next read_async returned the partial FIFO immediately.
  Note `RxConfig::timeout` is in symbol times and the register caps
  `timeout * symbol_len` at 1023 bits.
- Panic: esp-backtrace `panic-handler` + `println`, with esp-println pinned to
  `jtag-serial` (NOT its default `auto`). Panics print on the same /dev/ttyACM0
  as the analyzer output. It coexists with the async driver because esp-println's
  serial-JTAG backend pokes the USB_DEVICE FIFO/CONF registers directly, never
  takes the peripheral singleton, polls rather than using the interrupt, and
  bails out after a bounded spin if no host is draining - so it cannot hang an
  unattached board. esp-backtrace then parks the core with interrupts off.
- Size, release build (section sizes; `.text_gap` and `.stack` are NOBITS, i.e.
  alignment padding and a reserved region, not flashed and not used):
  flash `.text` 42312 + `.rodata` 13520 + `.trap` 1996 + `.rwtext` 3584 +
  `.data` 1760 + `.flash.appdesc` 256 = about 63 KB. Static RAM
  (`.trap + .rwtext + .data + .bss`) about 12 KB of the 512 KB HP SRAM.

## Code review findings (verdict: solid at 9600/19200, degrades predictably above)

Ranked, all confirmed with arithmetic.

HIGH

1. **DONE** - Per-byte DMA read in frame_receiver -> overrun risk. F411 USART has
   no RX FIFO; single-byte DMA gives only ONE char time of slack to re-arm
   (95.5 us @115200 vs 1146 us @9600). Competes with USB task servicing. FIXED
   with `RingBufferedUartRx` (256-byte ring = 24.4 ms slack @115200, ~250x) and
   64-byte chunked reads. Headline fix.
2. **DONE** - emit_frame silently dropped frames when the channel was full
   (depth 8, ~16-40 ms buffering). An analyzer that lies by omission. FIXED with
   an `AtomicU32 DROPPED` counter; the output task does `take_dropped()` before
   each frame and writes a `[DROPPED n frames]` marker inline, in temporal order.
   Now lives in `core/` and is shared by both firmwares.
3. USB backpressure: the output task writes synchronously; a slow or
   non-draining host blocks the write -> channel fills -> #2 drops. Bandwidth is
   NOT the issue (~40 KB/s out vs USB FS >1 MB/s); it is latency/stall. Largely
   mitigated by #2 making the loss visible.

MEDIUM

4. **DONE** - Frame timeout `char_us * 4` was not spec-compliant above ~58.7
   kbaud. The spec fixes t1.5 = 750 us above 19200; the old boundary dropped
   below 750 us and so SPLIT compliant frames. Crossover: safe <=19200, marginal
   @57600 (760 us), broken @115200 (380 us). FIXED by clamping to the spec's
   fixed 1750 us above 19200. Now `core::frame_timeout_us`, a `const fn` with
   unit tests at 9600 / 19200 / 115200.
5. DONE 2026-10-03, verified on ESP32-C6 hardware. Exception responses now
   print e.g. `func=0x83 (Exception: Read Holding Registers) exception=0x02
   (Illegal Data Address)`; a flagged frame with a body other than one byte
   falls back to hex data. `core::format_exception`, with unit tests.
6. Everything is labeled "RX"; shallow decode (no register values). It is a frame
   logger, not a full analyzer. The README now says so instead of overselling.
7. Baud/parity are compile-time consts; the CDC RX direction is unused. Missed
   chance for runtime config over the existing USB channel.
8. panic-halt plus `panic!("USB buffer overflow")` = silent brick on the STM32.
   Use panic-reset plus an LED heartbeat. (The ESP32-C6 side is already covered:
   esp-backtrace prints the panic on the output port.)
9. Any UART error flushes the partial frame as a real frame -> noise creates
   phantom SHORT/BAD_CRC frames. Should discard the partial on error and only
   emit on silence. Both firmwares currently share this behaviour deliberately,
   for parity.

LOW: ~~format_frame double-buffers (String then copy, buf param redundant)~~ DONE
- it writes straight into the caller's `Line`. ~~cosmetic double-space before
"crc="~~ DONE. ~~only 1 CRC unit test (framing untested)~~ DONE - 13 host tests.
Worst line is ~830 bytes into the 1024-byte `Line`; still a silent-truncation
risk if the format grows, though `format_frame` now returns `Err(FormatError)`
and the callers count it as a dropped frame instead of emitting a lying line.

CONFIRMED GOOD (do not touch): the STM32 clock tree (96 MHz sysclk, exact 48 MHz
USB via M=25/N=192/Q=4 - M=25 is forced for exact USB from a 25 MHz crystal, NOT
a smell); `vbus_detection = false`; the CRC-16; the ZLP-on-64-multiple flush
handling; the task structure.

## Next steps

Needs hardware, in order:

1. ESP32-C6: (selftest and FTDI-driven traffic DONE 2026-10-03.) Sniff a real
   Modbus master/slave bus.
2. (ESP32-C6 RX pull-up: no change needed - esp-hal's `with_rx` already enables
   the internal pull-up; documented in a comment.)
3. ESP32-C6: confirm that a panic really does print on /dev/ttyACM0 while the
   async driver has been running.
4. STM32F411: replug the ST-Link, `probe-rs download` the default build, cold
   replug the Black Pill, confirm the decode still works after the restructure.
   The STM32 had the same frame-splitting bug as the ESP32-C6 (ring-buffered
   reads only woke on idle line / DMA half-full, so the t3.5 timer fired
   mid-frame; embassy's own docs warn about this). Fixed with
   `uart_cfg.eager_reads = Some(1)`, compile-verified only. Test it with an
   FTDI RS-485 adapter driving frames at 20 ms gaps, as was done for the C6.

Code work not started: review items #3, #6, #7, #8, #9.
