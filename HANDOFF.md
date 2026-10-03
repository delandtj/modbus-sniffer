# Handoff: modbus-sniffer

Last updated: 2026-10-03

## What this is

Rust/Embassy firmware that passively sniffs a Modbus RTU bus over an RS-485
transceiver and streams decoded frames out the board's native USB port as a
virtual serial port (a frame logger / "protocol analyzer"). Two boards, one
Cargo workspace, one shared decode core. The ESP32-C6 build also streams over
WiFi to a TCP client on port 4000, takes runtime commands (`bus`, `status`,
`note`) on both transports, and keeps its bus settings in flash - it is meant to
sit in a poolhouse for days while a Linux box logs a heat-pump controller bus.

Git repo, branch `main`. Everything below compiles warning-free.

## Layout

```text
Cargo.toml              workspace (resolver 2), holds [profile.release]
core/                   modbus-sniffer-core - no_std, std only under cfg(test)
fw-stm32f411/           sniffer-stm32f411   - STM32F411CEU6 Black Pill
fw-esp32c6/             sniffer-esp32c6     - ESP32-C6-DevKitC-1 (WROOM-1)
  src/main.rs           init, line producer, USB consumer + commands, sniffer
  src/net.rs            WiFi station, DHCP, TCP line server on port 4000
  src/led.rs            RGB status LED, GPIO8 over RMT TX channel 0
  src/settings.rs       bus settings record in the nvs partition
```

There is NO `.cargo/config.toml` at the workspace root - that is deliberate, so
`cargo test -p modbus-sniffer-core` from the root runs on the host. Each firmware
crate has its own `.cargo/config.toml` with its target and runner, so you build
and flash from inside that directory.

`core/` holds, all `no_std` and all host-testable:

- Decode: `crc16`, `format_function`, `format_exception`, `Frame`,
  `MAX_FRAME_LEN`, `make_frame(buf, ts)`, `build_frame`,
  `const fn frame_timeout_us(baud)`.
- The STM32 path: `FRAME_CHANNEL` (embassy-sync Channel, depth 8), `DROPPED`
  (AtomicU32), `emit_frame(buf, timestamp_ms)` (non-async, only try_sends).
- The ESP path: `LineRing<N, M>` + `LineBus<N, M>` (`SnifferBus` =
  `LineBus<65536, 1024>`), `Chunk`, `CONSUMER_USB` / `CONSUMER_TCP`.
- Protocol: `BusConfig` / `BusFormat`, `encode_bus_record` /
  `decode_bus_record`, `parse_command` -> `Command`, and the formatters
  `format_frame` (writes straight into a caller-supplied `Line`),
  `format_dropped_marker`, `format_dropped_lines`, `format_status`,
  `format_hello`, `format_bus_line`, `format_note`, `format_command_ok`,
  `format_command_err`.

35 host unit tests. The line-ring ones are the interesting ones: they use a
deliberately tiny `LineRing<32, 4>` so slot exhaustion, byte-store exhaustion,
a line that wraps the end of the byte store, cursor overrun with the exact
`Dropped(n)` count, independent cursors, and `pending()` (the `replay=` count)
are all reachable.

### The line ring, and why

The old single-consumer `FRAME_CHANNEL` could not feed USB and TCP at once, and
a USB host that was not reading stalled the writes and therefore everything.
`LineRing` stores whole *formatted* lines in a byte ring with a parallel slot
table, numbers them with a monotonic `u64`, and keeps the cursors OUTSIDE the
ring - a cursor is just "the sequence number I want next". Consequences:

- `push` never blocks and never fails on a slow consumer; it evicts the oldest
  lines to make room.
- A consumer that was overtaken sees `cursor < oldest_seq` and learns exactly how
  many lines it missed -> `[DROPPED n lines]`.
- The TCP cursor lives in `tcp_task`, so it outlives every client; that is the
  whole replay mechanism, no extra buffer needed.
- `LineBus` adds a `CriticalSectionRawMutex` + one `Signal` per consumer, so
  waking one consumer does not touch the other.

## Current state (IMPORTANT - read first)

- STM32F411: unchanged behaviour, compiles warning-free in both build configs.
  Last hardware verification was of the pre-workspace code (selftest build
  verified end-to-end on /dev/ttyACM0; default build flashed but its decode was
  never re-confirmed because of the USB hub incident below). The restructure has
  NOT been re-flashed.
- ESP32-C6: selftest build VERIFIED on hardware 2026-10-03 (ESP32-C6 rev v0.2,
  16MB flash, DevKitC-1 "USB" port). /dev/ttyACM0 shows the three canned frames
  with correct decode, both OK and BAD_CRC.
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
- ESP32-C6 WiFi / TCP / ring / commands / persistence, 2026-10-03. VERIFIED over
  USB on hardware (no credentials were available, so the firmware was built
  WITHOUT `SNIFFER_WIFI_SSID`):
  - `status`, `note`, `# err` for all four rejection paths (unknown keyword,
    baud out of range, bad format, and the note/ok echo).
  - `bus 9600 8N1` -> drove real 8N1 traffic -> decoded; `bus 9600 8E1` ->
    drove 8E1 -> decoded; `bus 19200 8O1` accepted. Mismatched parity shows up
    as `uart_errors` plus one-byte SHORT lines, which is the honest answer.
  - Persistence: set 8N1, reset the chip through `espflash monitor`, `status`
    came back `bus=9600 8N1` with uptime ~5 s. nvs partition confirmed at
    0x9000/0x6000 in the bootloader's own partition dump.
  - Ring overrun: closed the USB reader, drove 1204 frames for 16 s, reattached
    -> `[DROPPED 332 lines]` inline, then the rest of the ring, and `status`
    afterwards reported `dropped=332`. (A `status` issued in the same instant as
    the reattach can still read 0: the reply is formatted when the command is
    parsed, before the pump gets to the marker. Not a bug.)
  - Boot line `# wifi disabled (no SNIFFER_WIFI_SSID at build time)` confirmed
    in the monitor capture.
  - WiFi VERIFIED on hardware 2026-10-03 with the user's credentials (in the
    git-ignored root .cargo/config.toml [env]): joins the AP (2.4 GHz only - the
    C6 has no 5 GHz), DHCP lease 192.168.64.161, hostname `modbus-sniffer`
    resolves on the LAN, rssi -59 dBm on the bench. Over TCP 4000: hello,
    `status`, `note`, live frames, and replay (3 frames sent with no client
    arrived on reconnect with their original uptime stamps). Two-socket
    takeover and keepalive are still untested.
  - `status` now reports `dropped=` (TCP consumer + ring refusals = real gaps
    in the server capture) and `usb_dropped=` separately, because the USB
    cursor falls behind forever when the board runs off a charger.
  - Left on the board: the default (non-selftest) build, bus 9600 8E1.
- Status LED (`fw-esp32c6/src/led.rs`, 2026-10-03): the DevKitC-1's on-board
  WS2812-type RGB LED on GPIO8, driven with `esp-hal-smartled` 0.18 (the first
  release that takes esp-hal `~1.2`) on RMT TX channel 0 in async mode. Blue
  blink = not associated, dim amber = WiFi up with no TCP client, dim green =
  client connected, dim cyan = no WiFi in this build or the radio refused;
  bright-green flash per good frame, bright red for BAD_CRC/SHORT/UART errors,
  rate-limited to one 35 ms flash per 80 ms. All steady colours derive from one
  `DIM` const (~8%). Producers only do `fetch_or` + `Signal::signal`; the one
  `led_task` is the only code that touches RMT, so the capture, USB and TCP
  paths cannot be slowed by the LED, and if the RMT setup fails the task just
  returns. Deliberately NOT hooked into the panic path - esp-backtrace owns the
  panic handler and its message on /dev/ttyACM0 is worth more than a red LED.
  Colour table is in the README.
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
| esp-rtos | 0.4.0 (features esp32c6, embassy, esp-radio, esp-alloc) |
| esp-radio | 1.0.0-beta.1 (no default features; esp32c6, wifi, esp-alloc, unstable) |
| esp-alloc | 0.11 (esp32c6) |
| esp-storage | 0.10 (esp32c6) |
| embassy-net | 0.9 (no default features; tcp, dhcpv4, dhcpv4-hostname, medium-ethernet, proto-ipv4) |
| esp-backtrace | 0.20.0 |
| esp-println | 0.18 (no default features; esp32c6, jtag-serial, critical-section) |
| esp-bootloader-esp-idf | 0.6.0 |
| embedded-io-async | 0.7 |
| static_cell | 2 |
| heapless | 0.8 (and 0.9 as `heapless09`, see below) |

`esp-wifi` is gone from the ecosystem; the WiFi driver is **esp-radio** now, and
esp-radio 1.0.0-beta.1 is the release that matches esp-hal ~1.2 and
esp-radio-rtos-driver ^0.4 (which esp-rtos 0.4.0 provides through its `esp-radio`
feature). `esp_radio::wifi::Interface` is itself the embassy-net driver
(embassy-net-driver 0.2, which is what embassy-net 0.9 wants), so there is no
glue type: `embassy_net::new(Interface::station(), ...)`. The old
`esp_wifi::init(...)` / `WifiDevice` split is gone - `WifiController::new(WIFI,
ControllerConfig)` initialises the radio and `Interface::station()` is a separate
singleton.

`heapless` appears twice ON PURPOSE. `core/` and the firmwares are on 0.8, but
`embassy_net::DhcpConfig::hostname` is a `heapless` 0.9 `String`, so
`fw-esp32c6` also depends on 0.9 renamed to `heapless09` purely to build that
one hostname. Two versions of heapless coexist fine (no `links` key); bumping the
whole workspace to 0.9 would have dragged the STM32 firmware along for no gain.

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

Bus settings chosen by the user: 9600 8E1 - on the ESP32-C6 that is now the
power-on default AND whatever the last `bus` command persisted. There is still no
real Modbus bus to test against - hence the selftest feature.

WiFi was verified on hardware after the WiFi work landed (see above).

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

# ESP32-C6
cd fw-esp32c6
cargo build --release
cargo build --release --features selftest
cargo run --release        # espflash flash --monitor --chip esp32c6, over the "USB" port

# ...with WiFi (credentials are build-time only, never committed)
SNIFFER_WIFI_SSID=myap SNIFFER_WIFI_PASS=mysecret cargo run --release

# Flash without opening a monitor, then reset (a plain flash leaves the app
# silent until something resets the chip)
espflash flash --chip esp32c6 --port /dev/ttyACM0 <target-dir>/riscv32imac-unknown-none-elf/release/sniffer-esp32c6
timeout 3 espflash monitor --chip esp32c6 --port /dev/ttyACM0 --non-interactive

# Read output on either board
minicom -D /dev/ttyACM0    # baud irrelevant for USB-CDC / USB-Serial-JTAG
nc modbus-sniffer 4000     # ...or over the network, on the ESP32-C6
```

Commands go in on the same connection (or on /dev/ttyACM0): `status`,
`note <text>`, `bus <baud> <8N1|8E1|8O1|8N2>`. See the README for the protocol.

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
- Output: `UsbSerialJtag::new(peripherals.USB_DEVICE).into_async().split()`.
  BOTH halves are used now: Tx by the USB line consumer, Rx by the command
  reader. `write_async`/`flush_tx_async` are private, so the traits come from
  `embedded_io_async::{Read, Write}` (esp-hal's `unstable` feature enables those
  impls).
- Input: `Uart::new(UART1, config).with_rx(GPIO4).into_async()`, RX FIFO full
  threshold 1, hardware RX idle timeout 2 symbol times. The frame boundary is
  `select3(read_async, Timer::after(bus.frame_timeout_us()), BUS_CHANGED.wait())`,
  and the timer restarts each time a read returns, so reads MUST return per
  byte. The original threshold of 32 split every frame on hardware: the read
  stayed pending mid-frame, the t3.5 timer fired while bytes were still
  arriving, and the next read_async returned the partial FIFO immediately.
  Note `RxConfig::timeout` is in symbol times and the register caps
  `timeout * symbol_len` at 1023 bits.
- **GOTCHA, cost an hour on hardware**: the driver is the full `Uart`, not
  `UartRx`, because only `Uart::apply_config` applies the *common* settings
  (baud rate, data bits, parity, stop bits) - it calls the private
  `info().apply_config()` first. `UartRx::apply_config` applies the RX-side
  settings ONLY (fifo threshold, timeout, discard-erroneous-bytes) and silently
  leaves baud and parity alone, so `bus 9600 8N1` appeared to succeed and the
  next frames came back as parity errors and one-byte SHORT lines. No TX pin is
  ever assigned, so the sniffer still cannot drive the bus.
- Runtime bus changes: the command handler stores the new `BusConfig` in a
  critical-section `Cell` and fires `BUS_CHANGED`; `frame_receiver` returns on
  that signal (after flushing the frame in hand at the OLD settings) and
  `sniffer_task` re-applies the UART config. If the hardware ever rejects it,
  the old setting is put back and re-announced with a second `# bus` line, so
  the log never claims a configuration the UART is not using.
- WiFi: `WifiController::new(WIFI, ControllerConfig::default())` then
  `set_config(&Config::Station(...))`, with `Interface::station()` handed
  straight to `embassy_net::new` as the driver. `wifi_task` loops on
  `connect_async` / `wait_for_disconnect_async` forever and samples `rssi()`
  every 5 s (both take `&self`, so they can run concurrently in a `select`).
  `link_task` watches `wait_config_up` / `wait_config_down` and is what emits
  `# wifi connected ip=...` / `# wifi disconnected` - the IP comes from DHCP, not
  from the association event.
- TCP server: TWO `TcpSocket`s. Whichever is idle sits in `accept(4000)` while
  the other serves, so a new connection is always accepted and then REPLACES the
  old one (`abort()` + `flush()`, a RST rather than TIME_WAIT). That is the
  deliberate answer to a half-dead session after the logging server reboots.
  `set_keep_alive(10 s)` + `set_timeout(30 s)` catch clients that vanish without
  a FIN. Per connection the task runs `select3(write pump, command reader,
  idle.accept())`; replies travel from the reader to the writer through a
  stack-local `Channel<NoopRawMutex, Line, 2>` so they reach only the requester.
  **Subtlety worth keeping**: embassy-net's `accept()` puts the socket in LISTEN
  *before* it awaits, so cancelling that future (which `select3` does on every
  client disconnect) leaves the idle socket listening. If both sockets end up
  listening, a connection can land on the one nobody is serving and just hang.
  So after each client the loop looks at `idle.state()` rather than at which
  select branch won: `Closed | Listen` means nobody took over, and the idle
  socket is `abort()`ed back out of LISTEN; anything else means a new client
  landed there and it becomes the active socket. Checking the state rather than
  the branch also covers the race where the old client leaves in the same
  instant the new one arrives.
- Settings: `src/settings.rs`. One 16-byte CRC-checked record at offset 0 of the
  `nvs` data partition (0x9000/0x6000 in the espflash default table), found via
  `esp_bootloader_esp_idf::partitions::read_partition_table` ->
  `find_partition(PartitionType::Data(DataPartitionSubType::Nvs))` ->
  `as_flash_region`, written with `erase(0, 4096)` + `write(0, &record)`. IDF's
  NVS library is never used, so the raw space is ours. The `FlashStorage` lives
  behind an async `embassy_sync::mutex::Mutex` because a write takes tens of ms.
- Heap: `esp_alloc::heap_allocator!(size: 96 * 1024)`, needed by esp-radio (its
  WiFi buffers and the esp-rtos task stacks it creates). esp-radio's build script
  accepts opt-level z, so the workspace `opt-level = "z"` is left alone.
- Panic: esp-backtrace `panic-handler` + `println`, with esp-println pinned to
  `jtag-serial` (NOT its default `auto`). Panics print on the same /dev/ttyACM0
  as the analyzer output. It coexists with the async driver because esp-println's
  serial-JTAG backend pokes the USB_DEVICE FIFO/CONF registers directly, never
  takes the peripheral singleton, polls rather than using the interrupt, and
  bails out after a bounded spin if no host is draining - so it cannot hang an
  unattached board. esp-backtrace then parks the core with interrupts off.
- Size, release build with WiFi (section sizes; `.text_gap` and `.stack` are
  NOBITS, i.e. alignment padding and a reserved region, not flashed):
  flash `.text` 113452 + `.rodata` 25920 + `.rwtext` 4580 + `.rwtext.wifi` 5812 +
  `.data` 3272 + `.trap` 1996 + `.rodata.wifi` 216 + `.flash.appdesc` 256 =
  about 152 KB; espflash reports the image as 179,072 bytes. Static RAM
  (`.trap + .rwtext* + .data* + .bss`) about 207 KB of the 512 KB HP SRAM, of
  which 96 KB is the esp-radio heap and 72 KB is the line ring (64 KB of line
  bytes + an 8 KB slot table). The pre-WiFi build was 63 KB flash / 12 KB RAM.

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
3. **DONE on the ESP32-C6** - USB backpressure: the output task writes
   synchronously; a slow or non-draining host blocks the write -> channel fills
   -> #2 drops. Bandwidth is NOT the issue (~40 KB/s out vs USB FS >1 MB/s); it
   is latency/stall. FIXED by the `core::LineBus` line ring: the producer never
   waits, and each consumer has its own cursor, so a stalled USB host only
   affects the USB cursor (confirmed on hardware - 332 lines dropped on the USB
   side while the ring kept filling). Still open on the STM32F411, which keeps
   the single-consumer channel.

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
7. **DONE on the ESP32-C6** - Baud/parity are compile-time consts; the CDC RX
   direction is unused. FIXED: `bus <baud> <8N1|8E1|8O1|8N2>`, `status` and
   `note <text>` are accepted on BOTH the USB-Serial-JTAG RX half and the TCP
   connection, parsed by `core::parse_command`, and the bus setting is persisted
   in the `nvs` partition. Still open on the STM32F411.
8. panic-halt plus `panic!("USB buffer overflow")` = silent brick on the STM32.
   Use panic-reset plus an LED heartbeat. (The ESP32-C6 side is already covered:
   esp-backtrace prints the panic on the output port.)
9. Any UART error flushes the partial frame as a real frame -> noise creates
   phantom SHORT/BAD_CRC frames. Should discard the partial on error and only
   emit on silence. Both firmwares currently share this behaviour deliberately,
   for parity.

LOW: ~~format_frame double-buffers (String then copy, buf param redundant)~~ DONE
- it writes straight into the caller's `Line`. ~~cosmetic double-space before
"crc="~~ DONE. ~~only 1 CRC unit test (framing untested)~~ DONE - 35 host tests.
Worst line is ~830 bytes into the 1024-byte `Line`; still a silent-truncation
risk if the format grows, though `format_frame` now returns `Err(FormatError)`
and the callers count it as a dropped frame instead of emitting a lying line.

CONFIRMED GOOD (do not touch): the STM32 clock tree (96 MHz sysclk, exact 48 MHz
USB via M=25/N=192/Q=4 - M=25 is forced for exact USB from a 25 MHz crystal, NOT
a smell); `vbus_detection = false`; the CRC-16; the ZLP-on-64-multiple flush
handling; the task structure.

## Next steps

Needs hardware, in order:

1. **ESP32-C6 WiFi, the big one.** Build with real credentials
   (`SNIFFER_WIFI_SSID=... SNIFFER_WIFI_PASS=... cargo run --release`) and check,
   in this order: association and the `# wifi connected ip=...` line; `nc
   modbus-sniffer 4000` and the single `# hello ...` line; the replay on a
   reconnect (disconnect, let frames accumulate, reconnect, confirm `replay=n`
   matches what then arrives); the takeover (connect a second client and watch
   the first get RST); commands over TCP; `rssi=` in `status`; and finally
   whether pulling the AP's power really does lead to an automatic reconnect.
   Also worth checking: writing the settings record while WiFi is up (the flash
   write disables the cache for tens of ms).
2. ESP32-C6: (selftest and FTDI-driven traffic DONE 2026-10-03.) Sniff a real
   Modbus master/slave bus.
3. ESP32-C6: confirm that a panic really does print on /dev/ttyACM0 while the
   async driver has been running.
4. STM32F411: replug the ST-Link, `probe-rs download` the default build, cold
   replug the Black Pill, confirm the decode still works after the restructure.
   The STM32 had the same frame-splitting bug as the ESP32-C6 (ring-buffered
   reads only woke on idle line / DMA half-full, so the t3.5 timer fired
   mid-frame; embassy's own docs warn about this). Fixed with
   `uart_cfg.eager_reads = Some(1)`, compile-verified only. Test it with an
   FTDI RS-485 adapter driving frames at 20 ms gaps, as was done for the C6.

Code work not started: review items #6, #8, #9, and #3 / #7 on the STM32 side.

Known rough edges in the new code, none of them blocking:

- `ControllerConfig`'s default country is `CN` (channels 1-13), which covers
  Europe, so it is left alone rather than set to `BE`.
- A `status` reply is formatted when the command is parsed, so a `status` issued
  in the same instant as a reconnect can report a `dropped` count from just
  before the `[DROPPED n lines]` marker that is about to go out.
- `dropped` counts lines reported in markers summed over BOTH consumers, so a
  USB port nobody reads inflates it. Documented in the README; the alternative
  (per-consumer counters) would need a protocol change.
- The TCP server is plaintext and unauthenticated. Fine on a home LAN, which is
  the stated deployment; do not port-forward it.
