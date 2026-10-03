# STM32 Black Pill Modbus RTU Sniffer

A small Rust/Embassy firmware for the STM32F411CEU6 "Black Pill" that listens to an RS-485 bus and streams decoded Modbus RTU frames over USB-CDC.

## What it does

- Receives Modbus RTU traffic on **USART2** via an RS-485 transceiver.
- Detects frame boundaries using the Modbus 3.5-character silence rule.
- Validates the CRC-16.
- Sends a human-readable line to the USB virtual serial port for every frame.

Example output:

```text
[0.012] RX slave=0x01 func=0x03 (Read Holding Registers) data=00 00 00 0a crc=0xc5cd computed=0xc5cd OK
[0.045] RX slave=0x01 func=0x03 (Read Holding Registers) data=14 00 01 00 02 00 03 ... crc=0x1234 computed=0x5678 BAD_CRC
```

## Hardware

Target board: **STM32F411CEU6 Black Pill**

### RS-485 module wiring

Your module is a **3.3 V / 5 V TTL UART to RS-485 isolated converter**. Most of these have auto direction control, so you only need to connect the TTL RX side to the Black Pill and A/B to the bus.

| Black Pill | Connection        | RS-485 module |
|------------|-------------------|---------------|
| 3.3 V      | →                 | VCC           |
| GND        | →                 | GND           |
| PA3        | ← module TTL RX   | TTL RX / RO   |
| PA1        | → DE/RE (optional)| DE / RE       |
| PA12       | USB DP            | —             |
| PA11       | USB DM            | —             |

Notes:

- **PA3** is USART2_RX. This is the only required data pin for pure sniffing.
- **PA1** is driven low to force receive mode. If your module is auto-direction, you can leave PA1 unconnected. If it has separate DE/RE pins, wire PA1 to both DE and RE (or tie them to GND manually).
- Connect the module A/B terminals to the Modbus bus in parallel with the other devices.

### USB

Connect the Black Pill's micro-USB to your PC. Most Black Pill clones already have the 1.5 kΩ D+ pull-up to 3.3 V, so no extra hardware is needed.

## Software setup

1. Install the Rust target:

   ```bash
   rustup target add thumbv7em-none-eabihf
   ```

2. Install `probe-rs` (for flashing via ST-Link):

   ```bash
   cargo install probe-rs --features cli
   ```

3. Build:

   ```bash
   cargo build --release
   ```

## Flash

The project is configured to flash with `probe-rs` through the `.cargo/config.toml` runner:

```bash
cargo run --release
```

Or manually:

```bash
probe-rs run --chip STM32F411CE target/thumbv7em-none-eabihf/release/stm32-modbus-sniffer
```

## View output

After flashing, the Black Pill enumerates as a CDC ACM serial port (usually `/dev/ttyACM0` on Linux). Open it with any serial terminal at **115200 8N1**:

```bash
picocom -b 115200 /dev/ttyACM0
# or
minicom -D /dev/ttyACM0 -b 115200
```

The baud rate here is only the *USB-CDC virtual serial* speed; the actual Modbus bus baud rate is configured in `src/main.rs` (`BAUD_RATE`).

## Configuration

Edit `src/main.rs`:

```rust
const BAUD_RATE: u32 = 9_600; // change to your Modbus bus speed
```

Common Modbus settings:

- 9600 baud, even parity, 1 stop bit (default in this firmware)
- 19200 baud
- 115200 baud

The frame-timeout (3.5 character times) is computed automatically from `BAUD_RATE`.

## Project structure

```text
.
├── Cargo.toml
├── memory.x
├── build.rs
├── .cargo/config.toml
└── src
    ├── main.rs
    └── modbus.rs
```

## Troubleshooting

- **USB does not enumerate**: make sure `vbus_detection = false` is set and the board has the D+ pull-up. Some clones need a 1.5 kΩ resistor from PA12 to 3.3 V.
- **Garbage / no frames**: verify the RS-485 module GND is tied to Black Pill GND and the A/B wires are not swapped.
- **CRC always BAD**: check that the bus parity matches the firmware (`ParityEven` by default). Many Modbus devices use even parity, but some use none.
