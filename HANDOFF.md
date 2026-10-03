# Handoff: stm32-modbus-sniffer

Last updated: 2026-06-23

## What this is

Rust/Embassy firmware for an STM32F411CEU6 "Black Pill" that passively sniffs a
Modbus RTU bus over an RS-485 transceiver and streams decoded frames out the
Black Pill's native USB port as a CDC-ACM virtual serial port (a frame logger /
"protocol analyzer").

Target: thumbv7em-none-eabihf. Builds clean. Not a git repo (consider `git init`).

## Current state (IMPORTANT - read first)

- 2026-06-23 (later session): Code-review fixes #1, #4, #2 IMPLEMENTED and
  compile-verified warning-free in BOTH build configs. NOT yet flashed/verified
  on hardware (ST-Link was unplugged - see below). Details under "Fixes applied".
- Firmware is COMPLETE and compiles warning-free in both build configs.
- A `selftest` cargo feature was added this session (see below) and was flashed
  and VERIFIED working end-to-end on real hardware: frames appeared on
  /dev/ttyACM0 with correct decode, CRC OK and BAD_CRC paths both confirmed.
- The DEFAULT (real sniffer) build was then flashed back. Its `probe-rs download`
  completed, but the subsequent `probe-rs reset` triggered a USB enumeration
  failure (error -110), and retry storms crashed the shared USB hub (bus 5),
  which took down the ST-Link AND the Black Pill (and the user's mic/receiver).
- LAST KNOWN HARDWARE STATE: hub still crashed. `probe-rs list` finds no probe;
  neither ST-Link (0483:3748) nor Black Pill (c0de:cafe) enumerate; no ttyACM.
  RECOVERY REQUIRED BY USER: physically unplug Black Pill USB + ST-Link, wait
  ~5s, replug (ST-Link ideally into a direct PC port, not the hub).

## Hardware

- Programmer: ST-Link/V2 dongle over SWD (JTAG fails, SWD works - normal).
- Black Pill's own micro-USB/USB-C carries the CDC output (PA11/PA12). This is
  SEPARATE from the ST-Link. Both must be plugged in: ST-Link for flashing,
  Black Pill USB for reading output.
- RS-485 module the user owns: szhjw "3.3V/5V TTL UART to RS485 Isolated, single
  chip, automatic flow control" => NO DE/RE pin (auto-direction), galvanically
  isolated (do NOT tie bus ground to Black Pill ground).

### Wiring for sniffing (3 wires to Black Pill + A/B to bus)

| Module (TTL side) | Black Pill | Note |
|-------------------|-----------|------|
| VCC | 3V3 | use 3.3V so data line to PA3 stays 3.3V logic |
| GND | GND | TTL-side ground only |
| RXD | PA3 | USART2_RX, bus data into MCU. If silent, try module TXD instead (label convention varies) |
| TXD | (nc) | sniffer never transmits |

Module A/B -> bus A/B (swap if garbage/silent). PA1 (firmware DE pin) left
unconnected for this auto-direction module.

NOTE: README still documents the older DE/RE-on-PA1 wiring. User asked whether to
update it to the auto-direction wiring above - NOT yet done. Pending.

## Build / flash recipes

```bash
# Real sniffer (default)
cargo build --release
# Self-test (inject canned frames, no UART - verifies USB path without a bus)
cargo build --release --features selftest

# Flash (do NOT use `probe-rs reset` after - it caused the hub crash):
probe-rs download --chip STM32F411CE target/thumbv7em-none-eabihf/release/stm32-modbus-sniffer
# Then COLD-BOOT by physically replugging the Black Pill USB, not probe-rs reset.

# Read output:
ls /dev/ttyACM0          # user is in `uucp` group, perms OK
minicom -D /dev/ttyACM0  # baud irrelevant for USB-CDC
```

Bus settings chosen by user: 9600 8E1 (firmware default, src/main.rs:31 BAUD_RATE,
parity ParityEven). User has NO Modbus bus to test against right now - hence the
selftest feature.

## What was added this session

`selftest` cargo feature (Cargo.toml [features], mutually exclusive with the real
sniffer at compile time per user's global rule). When enabled, `selftest_task()`
(bottom of src/main.rs) injects 3 canned frames (valid request, valid 20-byte
response, deliberately CRC-corrupted frame) into FRAME_CHANNEL on a loop, so the
USB/format/CRC path is exercised with no hardware. Sniffer-only items
(sniffer_task, frame_receiver, BAUD_RATE, several imports) are cfg-gated off in
selftest builds so both configs are warning-free.

Also corrected (then reverted): the `spawner.spawn(task().unwrap())` idiom. NOTE
this embassy version's #[task] macro makes the task fn return
Result<SpawnToken, SpawnError> and spawn() returns () - so `.unwrap()` goes on the
TASK CALL, inside spawn(), NOT on spawn()'s result. The original code was correct;
do not "fix" it.

## Fixes applied (2026-06-23 later session) - compile-verified, not yet on HW

#1 DONE - RingBufferedUartRx. sniffer_task now does
   `UartRx::new(...).into_ring_buffered(RX_RING.init([0u8;256]))` (256-byte DMA
   ring via static_cell::StaticCell). frame_receiver signature is now
   `&mut RingBufferedUartRx<'static>` and reads in 64-byte chunks instead of
   per byte. Default config (eager_reads=0) makes read() wake on the UART idle
   line, i.e. it returns a whole frame at once; the t3.5 timer (measured from
   that return) still fires before any properly-gapped next frame can complete,
   so framing is preserved (verified by timing analysis: even a 1-byte frame at
   the minimum 3.5-char gap stays separated). Overrun window eliminated.

#4 DONE - spec-correct frame timeout. frame_receiver computes:
   `if BAUD_RATE > 19_200 { 1750 us } else { char_us * 4 }`. BAUD_RATE is const
   so this folds at compile time. At the 9600 default this is unchanged
   behaviour; it only matters if BAUD_RATE is raised above 19200.

#2 DONE - dropped-frame counter. `static DROPPED: AtomicU32`. emit_frame no
   longer evicts-oldest; on a full channel it drops the new frame and does
   `DROPPED.fetch_add(1, Relaxed)`. cdc_loop does `DROPPED.swap(0)` before each
   frame and, if nonzero, writes a `[DROPPED n frames]\r\n` marker inline (in
   temporal order). The selftest path shares emit_frame so the counter is
   exercised there too (harmless).

Build/RAM after fixes: text 25296, data 224, bss 6560 (ring + channel). Fits
F411CE easily. Both `cargo build --release` and `--features selftest` are
warning-free (forced full rebuild, grepped clean).

NEXT ON HARDWARE: replug ST-Link, `probe-rs download` the default build, cold
replug Black Pill, confirm /dev/ttyACM0 still decodes. The selftest build can
validate the USB/DROPPED-marker path without a bus.

## Code review findings (verdict: solid at 9600/19200, degrades predictably above)

Ranked, all confirmed with arithmetic:

HIGH
1. Per-byte DMA read in frame_receiver -> overrun risk. F411 USART has no RX FIFO;
   single-byte DMA gives only ONE char time of slack to re-arm (95.5 us @115200 vs
   1146 us @9600). Competes with USB task servicing. FIX: RingBufferedUartRx
   (256-byte ring = 24.4 ms slack @115200, ~250x). Headline fix.
2. emit_frame (src/main.rs ~330) silently drops frames when channel full (depth 8,
   ~16-40 ms buffering). An analyzer that lies by omission. FIX: AtomicU32 dropped
   counter + emit "[DROPPED n]" marker.
3. USB backpressure: cdc_loop writes synchronously; slow/non-draining host blocks
   write_packet -> channel fills -> #2 drops. Bandwidth is NOT the issue (~40 KB/s
   out vs USB FS >1 MB/s); it's latency/stall.

MEDIUM
4. Frame timeout `char_us * 4` not spec-compliant above ~58.7 kbaud. Spec fixes
   t1.5=750us above 19200; code boundary drops below 750us => SPLITS compliant
   frames. Crossover: safe <=19200, marginal @57600 (760us), broken @115200 (380us).
   FIX: clamp to spec's fixed 1750us above 19200.
5. Exception responses (func | 0x80, e.g. 0x83) decode as "Unknown" - the most
   interesting frames are invisible. modbus.rs format_function.
6. Everything labeled "RX"; shallow decode (no register values). It's a frame
   logger, not a full analyzer - README oversells.
7. Baud/parity are compile-time consts; CDC RX direction is unused. Missed chance
   for runtime config over the existing USB channel.
8. panic-halt + panic!("USB buffer overflow") (line 242) = silent brick. Use
   panic-reset + LED heartbeat.
9. Any UART error flushes partial frame as a real frame -> noise creates phantom
   SHORT/BAD_CRC frames. Should discard partial on error vs emit on silence.

LOW: format_frame double-buffers (String then copy, buf param redundant); worst
line ~831 bytes into 1024 buf = silent truncation if format grows; only 1 CRC
unit test (framing untested); cosmetic double-space before "crc=".

CONFIRMED GOOD (do not touch): clock tree (96 MHz sysclk, exact 48 MHz USB via
M=25/N=192/Q=4 - M=25 is forced for exact USB from a 25 MHz crystal, NOT a smell);
vbus_detection=false; CRC-16 correct; ZLP-on-64-multiple flush handling; task
structure.

## Pending decision / next step

Fixes #1, #4, #2 are DONE in code (see "Fixes applied" above) but NOT yet flashed
or verified on hardware - the ST-Link was unplugged after the hub crash. Next
hardware step: replug ST-Link, flash the default build, cold replug, verify
/dev/ttyACM0 decode still works. Remaining review items not started: #3 (USB
backpressure - largely mitigated by #2's drop+counter), #5 (exception-response
decode), #6-#9, and the LOW items.

Open follow-ups also raised, not yet done:
- Update README wiring to the auto-direction (no DE/RE) module.
- Fold the two flash recipes (default vs selftest) into README.
- `git init` the repo.
