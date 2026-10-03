# Modbus RTU Sniffer

Rust/Embassy firmware that listens passively to an RS-485 bus, decodes Modbus RTU
frames and streams one human-readable line per frame out of the board's native USB
port as a virtual serial device. Two boards are supported from one workspace:

| Crate | Board | Input | Output |
|---|---|---|---|
| `fw-stm32f411` | STM32F411CEU6 "Black Pill" | USART2 RX on PA3 | USB CDC-ACM (PA11/PA12) |
| `fw-esp32c6` | ESP32-C6-DevKitC-1 (WROOM-1) | UART1 RX on GPIO4 | built-in USB-Serial-JTAG **and** TCP over WiFi |

Both produce byte-identical frame lines, because the framing, CRC and formatting
live in a shared `no_std` crate. The ESP32-C6 build adds WiFi, a TCP line server
on port 4000 and runtime bus configuration, which is what makes it usable for an
unattended multi-day capture with a Linux box doing the logging.

## What it does

- Receives Modbus RTU traffic through an RS-485 transceiver.
- Detects frame boundaries using the Modbus 3.5-character silence rule (t3.5), with
  the spec's fixed 1750 us above 19200 baud.
- Validates the CRC-16.
- Sends one line per frame to every attached output.
- Counts lines and frames it had to drop, and says so inline rather than lying by
  omission.

Example output:

```text
[0.012] RX slave=0x01 func=0x03 (Read Holding Registers) data=00 00 00 0a crc=0xcdc5 computed=0xcdc5 OK
[0.045] RX slave=0x01 func=0x03 (Read Holding Registers) data=14 00 01 00 02 crc=0x1234 computed=0x5678 BAD_CRC
[1.500] SHORT (3 bytes): aa bb cc
[DROPPED 4 frames]
```

## Workspace layout

```text
.
|-- Cargo.toml              workspace, [profile.release]
|-- core/                   modbus-sniffer-core: no_std, host-testable
|   \-- src/lib.rs          CRC, t3.5 rule, Frame, line ring, formatters,
|                           bus config + flash record, command parser
|-- fw-stm32f411/           crate sniffer-stm32f411
|   |-- .cargo/config.toml  thumbv7em-none-eabihf, probe-rs runner
|   |-- build.rs, memory.x
|   \-- src/main.rs
|-- fw-esp32c6/             crate sniffer-esp32c6
|   |-- .cargo/config.toml  riscv32imac-unknown-none-elf, espflash runner
|   |-- src/main.rs         init, line producer, USB consumer, commands
|   |-- src/net.rs          WiFi station, DHCP, TCP line server
|   \-- src/settings.rs     bus settings in the nvs partition
\-- tools/                  modbus-sniffer-tools: host crate, see tools/README.md
    |-- src/bin/            sniffer-capture, sniffer-analyze, fake-device
    \-- systemd/            modbus-sniffer-capture.service
```

There is deliberately no `.cargo/config.toml` at the workspace root, so the core
crate's unit tests and the host tools build for the host:

```bash
cargo test -p modbus-sniffer-core
cargo test -p modbus-sniffer-tools
```

Each firmware crate carries its own target in its own `.cargo/config.toml`, so you
build and flash from inside that directory.

## Wiring (both boards)

The RS-485 module in use is an **auto-direction, galvanically isolated TTL-to-RS-485
converter** (szhjw "3.3V/5V TTL UART to RS485 Isolated, single chip, automatic flow
control"). It has **no DE/RE pin** - direction is handled on the module - so sniffing
needs three wires on the TTL side plus A/B to the bus.

| Module (TTL side) | STM32F411 | ESP32-C6 | Note |
|---|---|---|---|
| VCC | 3V3 | 3V3 | use 3.3 V so the data line into the MCU stays 3.3 V logic |
| GND | GND | GND | **TTL-side ground only** |
| RXD | PA3 | GPIO4 | bus data into the MCU |
| TXD | n/c | n/c | the sniffer never transmits |

Module A/B go to the bus A/B, in parallel with the other devices.

- If the line stays silent, swap A/B, and if that does not help swap RXD/TXD on the
  module - the TTL-side labelling convention varies between these boards.
- The module is isolated: **do not** tie the bus ground to the MCU ground. Only the
  TTL-side GND connects.
- On the STM32 build, PA1 is still configured as an output driven low. It is a
  leftover DE/RE line and is harmless; leave it unconnected.
- If the CRC is always bad, check the parity. The firmware defaults to 9600 8E1;
  plenty of Modbus devices use 8N1 instead.

## STM32F411 Black Pill

Programmed over SWD with an ST-Link/V2; the Black Pill's own micro-USB carries the
output and is a separate cable. Both need to be plugged in.

```bash
rustup target add thumbv7em-none-eabihf
cargo install probe-rs-tools        # provides the probe-rs CLI

cd fw-stm32f411
cargo build --release                      # real sniffer
cargo build --release --features selftest   # canned frames, no UART
```

Flash with `probe-rs download`, then **cold-boot the board by physically replugging
its USB cable**:

```bash
ELF="$(cargo metadata --format-version 1 --no-deps \
       | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
probe-rs download --chip STM32F411CE \
  "$ELF/thumbv7em-none-eabihf/release/sniffer-stm32f411"
```

(The binary is normally at `target/thumbv7em-none-eabihf/release/sniffer-stm32f411`
under the workspace root; the command above also works if you have a global
`build.target-dir` set in `~/.cargo/config.toml`.)

**Do not run `probe-rs reset` afterwards.** On this setup it triggered a USB
enumeration failure (-110) whose retry storm took down the whole USB hub, including
the ST-Link, the Black Pill and unrelated devices on the same hub. Replugging the
Black Pill is the safe way to restart it.

Read the output:

```bash
picocom -b 115200 /dev/ttyACM0     # the baud rate is irrelevant for USB-CDC
```

## ESP32-C6-DevKitC-1

The DevKitC-1 has two USB-C ports. Use the one labelled **"USB"** - that is the
chip's own USB-Serial-JTAG, which both flashes the board and carries the sniffer
output. The port labelled "UART" is the CP2102N bridge and is not used here.

```bash
rustup target add riscv32imac-unknown-none-elf
cargo install espflash

cd fw-esp32c6
cargo build --release                       # real sniffer
cargo build --release --features selftest   # canned frames, no UART

cargo run --release                         # flash + monitor via espflash
```

`cargo run` uses the runner in `fw-esp32c6/.cargo/config.toml`
(`espflash flash --monitor --chip esp32c6`), which flashes and then opens the
monitor on the same port. To just read the output later:

```bash
picocom -b 115200 /dev/ttyACM0
```

A plain `espflash flash` without `--monitor` leaves the application silent until
something resets the chip, so either use `cargo run`, press RESET, or:

```bash
timeout 3 espflash monitor --chip esp32c6 --port /dev/ttyACM0 --non-interactive
```

Panic messages and backtraces are printed to this same port by esp-backtrace, so a
crash is visible where you are already looking rather than being a silent hang.

### WiFi build

Credentials are read from the environment **at build time**, so nothing has to be
committed and nothing has to be typed into the device:

```bash
cd fw-esp32c6
SNIFFER_WIFI_SSID=myap SNIFFER_WIFI_PASS=mysecret cargo run --release
```

With no `SNIFFER_WIFI_SSID` the build still succeeds, WiFi is simply never started,
and the status line `# wifi disabled (no SNIFFER_WIFI_SSID at build time)` goes into
the log. An empty or absent `SNIFFER_WIFI_PASS` means an open network.

The station reconnects by itself, forever, if the AP disappears. The DHCP hostname
is `modbus-sniffer`, so for a quick look:

```bash
nc modbus-sniffer 4000
```

**Reserve a DHCP lease** for the sniffer's MAC on your router. The device is meant
to sit somewhere inconvenient for days; a fixed address means the logger's
reconnect loop never has to rediscover it, and the hostname keeps working even on
networks whose DHCP server does not register names in DNS.

## Protocol

The same line protocol runs over TCP (port 4000) and over USB-Serial-JTAG. Device
to client, everything is text with CRLF line endings.

| Line | Meaning |
|---|---|
| `[secs.ms] RX slave=0x.. func=0x.. (...) data=.. crc=0x.... computed=0x.... OK\|BAD_CRC` | one decoded frame; `secs.ms` is **device uptime**, not wall clock |
| `[secs.ms] SHORT (n bytes): ...` | a byte run too short to be a frame |
| `[DROPPED n frames]` | frames the firmware could not even format |
| `[DROPPED n lines]` | lines this consumer fell behind on and skipped |
| `# ...` | status, replies and notes |

Status and reply lines:

| Line | When |
|---|---|
| `# hello sniffer-esp32c6 <version> uptime_ms=<n> bus=<baud> <fmt> replay=<n>` | exactly once per TCP connection, before anything else |
| `# wifi connected ip=<a.b.c.d>` | DHCP lease obtained (into the log, all consumers) |
| `# wifi disconnected` | lease lost (into the log) |
| `# bus <baud> <fmt>` | the bus settings changed (into the log) |
| `# note [secs.ms] <text>` | an operator note (into the log) |
| `# status uptime_ms=<n> bus=<baud> <fmt> wifi=<up\|down> ip=<a.b.c.d\|?> rssi=<dBm\|?> frames=<n> bad_crc=<n> uart_errors=<n> dropped=<n> usb_dropped=<n>` | reply to `status`, to the requester only |
| `# ok <normalized command>` | reply to `bus` / `note`, to the requester only |
| `# err <reason>` | rejected command, to the requester only |

Notes on the exact shapes, because a log consumer has to parse them:

- `replay=<n>` and `frames=<n>` and friends are plain decimal counts. `replay` is
  the number of lines that will be replayed right after the hello line.
- `uptime_ms` in the hello line is sampled at the moment it is sent, so a client
  can anchor every later `[secs.ms]` timestamp to its own wall clock.
- `ip=` and `rssi=` render as a literal `?` when the value is unknown (no lease, no
  association, or a build with WiFi disabled).
- `status` answers with its `# status` line only - there is no additional
  `# ok status`. `bus` and `note` answer with `# ok ...`.
- `bad_crc` counts frames of 4 bytes or more whose CRC did not check out; `SHORT`
  lines have no CRC and are not counted there. `dropped` is the running total of
  lines reported in `[DROPPED n lines]` markers, summed over both consumers - so a
  USB port nobody is reading does inflate it, by design.

Client to device, commands are LF- or CRLF-terminated, at most 128 bytes, keyword
case-insensitive:

| Command | Effect |
|---|---|
| `bus <baud> <8N1\|8E1\|8O1\|8N2>` | reconfigure UART1 now, recompute t3.5, save to flash. Baud 1200..=115200. |
| `status` | the `# status` line above |
| `note <text>` | puts `# note [secs.ms] <text>` into the log, in order with the frames |

A `bus` change is applied first and saved second, so if the board has no settings
partition the change still takes effect for this session and the reply is an
`# err` saying it did not stick.

### Line ring, replay and backpressure

Everything the ESP32-C6 produces goes into one ~64 KB RAM ring of text lines
(1024 line slots), and each output reads it through its own cursor:

- The sniffer never waits for an output. When the ring is full the oldest lines are
  evicted; a consumer that gets overtaken emits `[DROPPED n lines]` and carries on
  from the oldest line still held.
- A USB host that is not draining the port can no longer stall the TCP client, and
  vice versa.
- The TCP cursor survives client disconnects. A reconnecting client therefore gets
  everything produced while it was away, as far back as the ring still holds it,
  and the hello line says how much (`replay=`). The first client after boot gets
  the whole ring.
- One client at a time on port 4000, and a **new connection replaces the old one**.
  That is deliberate: after the logging server reboots, its old session is often a
  half-dead socket, and without this the sniffer would be locked out until someone
  walked over to it. TCP keepalive (10 s) and a 30 s transmit timeout catch clients
  that vanish without a FIN.

## Configuration

On the **ESP32-C6** the bus parameters are set at runtime with the `bus` command and
survive a power cut. They are stored as one 16-byte CRC-checked record (magic,
version, baud, format) at offset 0 of the ESP-IDF `nvs` data partition - 0x9000,
size 0x6000, in the espflash default partition table. We never use IDF's NVS
library, so that partition's raw space is ours. The partition is located at boot
through esp-bootloader-esp-idf's partition-table API rather than by hard-coding the
address, so a custom partition table moves the record with it. An absent or invalid
record means **9600 8E1**.

On the **STM32F411** the bus parameters are still compile-time constants at the top
of `src/main.rs`:

```rust
const BAUD_RATE: u32 = 9_600;   // change to your Modbus bus speed
```

Parity and stop bits are set next to the UART config in the same file (even parity,
1 stop bit by default). The frame timeout is derived from `BAUD_RATE` by
`modbus_sniffer_core::frame_timeout_us`, which is a `const fn`, so changing the baud
rate is enough.

On the ESP32-C6 the input pin is named once, as the `RxPin` type alias in
`fw-esp32c6/src/main.rs`; change that and the matching `peripherals.GPIO4` in
`main` to move the input.

## Self-test

Both firmwares have a `selftest` cargo feature. It leaves the UART uninitialised and
instead injects three canned frames on a loop - a valid request, a valid 20-byte
response, and a deliberately CRC-corrupted frame - through the same producer the
real sniffer uses. That exercises the whole output path, the CRC decode and the
BAD_CRC branch with no bus attached. WiFi, the TCP server and the commands all still
work in a selftest build. The two builds are mutually exclusive at compile time.

## Host tools

For multi-day captures there is a host crate, `tools/`
(package `modbus-sniffer-tools`), with two binaries:

- `sniffer-capture` - a daemon for an always-on Linux server. It holds one TCP
  connection to the sniffer, reconnects forever, maps the device's uptime stamps
  onto wall-clock time and writes one log file per local day. A unix control
  socket lets you mark the log while you change a setting on the controller:
  `sniffer-capture note "setpoint 26 -> 27"`.
- `sniffer-analyze` - reads those logs back and writes a markdown protocol map:
  the poll loop with its periods and latencies, a register map in hex / u16 /
  s16 / tenths, the writes, the exceptions, and - the useful part - which
  registers changed around each note.

```bash
cargo install --path tools
cargo test -p modbus-sniffer-tools     # drives the daemon against a fake device
```

See [tools/README.md](tools/README.md) for the systemd unit, the control CLI and
the mapping workflow.

## Scope

The **firmware** is a frame logger, not a protocol analyzer. It decodes the
address, function code, exception code and CRC, and hex-dumps the payload; it
does not decode register values, pair requests with responses, or label master
vs slave (everything reads `RX`). That decoding is deliberately left to
`sniffer-analyze` on the host, where it can look at days of traffic at once
instead of one frame at a time.

The TCP side is plaintext with no authentication: anything that can reach port 4000
can read the bus traffic and change the bus settings. That is fine on a trusted home
LAN, which is what it is for; do not expose it to the internet.
