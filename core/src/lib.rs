//! Board-independent Modbus RTU sniffer core.
//!
//! Everything in here is target-agnostic: CRC, function-code names, frame
//! framing constants, the sniffer -> output channel, the multi-consumer line
//! ring, the bus-configuration record and the line formatters.
//! It deliberately does NOT depend on `embassy-time`; timestamps are passed in
//! by the firmware so this crate builds and unit-tests on the host.

#![cfg_attr(not(test), no_std)]

use core::cell::RefCell;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use heapless::String;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Max raw Modbus RTU frame size (address + function + 252 data + 2 CRC = 256).
pub const MAX_FRAME_LEN: usize = 256;

/// Depth of the sniffer -> output channel, in frames.
pub const FRAME_CHANNEL_DEPTH: usize = 8;

/// Capacity of one formatted output line. Worst case for a 256-byte frame is
/// around 830 bytes, so 1024 leaves headroom.
pub const MAX_LINE_LEN: usize = 1024;

/// Capacity of the dropped-frame / dropped-line marker line. The longest is
/// "[DROPPED 18446744073709551615 lines]\r\n" at 38 bytes.
pub const MAX_MARKER_LEN: usize = 48;

/// Longest accepted command line, excluding the terminator.
pub const MAX_COMMAND_LEN: usize = 128;

/// A formatted output line.
pub type Line = String<MAX_LINE_LEN>;

/// A formatted dropped-frame marker.
pub type Marker = String<MAX_MARKER_LEN>;

/// Formatting ran out of buffer space. Should not happen with the capacities
/// above; the caller decides whether to drop the line or panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatError;

// ---------------------------------------------------------------------------
// Frame + shared channel
// ---------------------------------------------------------------------------

/// One sniffed frame, as captured between two inter-frame silences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Milliseconds since boot, taken when the frame was closed.
    pub timestamp_ms: u64,
    pub data: heapless::Vec<u8, MAX_FRAME_LEN>,
    pub crc_ok: bool,
}

/// Sniffed frames on their way to whatever output transport the board has.
pub static FRAME_CHANNEL: Channel<CriticalSectionRawMutex, Frame, FRAME_CHANNEL_DEPTH> =
    Channel::new();

/// Frames lost because the channel was full when the sniffer tried to enqueue.
/// Surfaced inline on the output stream as a "[DROPPED n frames]" marker so the
/// analyzer never lies by omission.
pub static DROPPED: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Modbus RTU inter-frame delay (t3.5) in microseconds, as a frame boundary.
///
/// The spec defines it as 3.5 character times for baud <= 19200, but FIXES it
/// at 1750 us for higher rates (because 3.5 chars shrinks below the silence a
/// device needs). 11 bits/char is assumed (1 start + 8 data + parity + 1 stop).
pub const fn frame_timeout_us(baud: u32) -> u64 {
    if baud > 19_200 {
        1_750
    } else {
        let char_us: u64 = (11u64 * 1_000_000) / baud as u64;
        char_us * 4 // 4 chars ~= 3.5 + margin
    }
}

/// Modbus RTU CRC-16 (CRC-16-IBM, polynomial 0x8005, init 0xFFFF).
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for byte in data {
        crc ^= u16::from(*byte);
        for _ in 0..8 {
            let lsb = crc & 1;
            crc >>= 1;
            if lsb != 0 {
                crc ^= 0xA001;
            }
        }
    }
    crc
}

/// Human-readable name for common Modbus function codes.
pub fn format_function(code: u8) -> &'static str {
    match code {
        0x01 => "Read Coils",
        0x02 => "Read Discrete Inputs",
        0x03 => "Read Holding Registers",
        0x04 => "Read Input Registers",
        0x05 => "Write Single Coil",
        0x06 => "Write Single Register",
        0x07 => "Read Exception Status",
        0x08 => "Diagnostics",
        0x0B => "Get Comm Event Counter",
        0x0C => "Get Comm Event Log",
        0x0F => "Write Multiple Coils",
        0x10 => "Write Multiple Registers",
        0x11 => "Report Slave ID",
        0x14 => "Read File Record",
        0x15 => "Write File Record",
        0x16 => "Mask Write Register",
        0x17 => "Read/Write Multiple Registers",
        0x18 => "Read FIFO Queue",
        0x2B => "Encapsulated Interface Transport",
        _ => "Unknown",
    }
}

/// Bit set in the function code of an exception response (e.g. 0x83 answers 0x03).
pub const EXCEPTION_FLAG: u8 = 0x80;

/// Human-readable name for Modbus exception codes (the single data byte of an
/// exception response).
pub fn format_exception(code: u8) -> &'static str {
    match code {
        0x01 => "Illegal Function",
        0x02 => "Illegal Data Address",
        0x03 => "Illegal Data Value",
        0x04 => "Server Device Failure",
        0x05 => "Acknowledge",
        0x06 => "Server Device Busy",
        0x08 => "Memory Parity Error",
        0x0A => "Gateway Path Unavailable",
        0x0B => "Gateway Target Device Failed to Respond",
        _ => "Unknown",
    }
}

// ---------------------------------------------------------------------------
// Enqueue
// ---------------------------------------------------------------------------

/// Validate and enqueue one captured frame.
///
/// Never blocks the sniffer on a stalled output host: if the channel is full,
/// the frame is dropped and counted in [`DROPPED`] so the loss shows up on the
/// stream as a marker instead of vanishing.
pub fn emit_frame(buf: &[u8], timestamp_ms: u64) {
    if FRAME_CHANNEL
        .try_send(make_frame(buf, timestamp_ms))
        .is_err()
    {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Validate a captured byte run and wrap it in a [`Frame`], without enqueueing
/// it anywhere. Used by boards that publish straight into a [`LineBus`].
pub fn make_frame(buf: &[u8], timestamp_ms: u64) -> Frame {
    let crc_ok = if buf.len() >= 4 {
        let received = u16::from_le_bytes([buf[buf.len() - 2], buf[buf.len() - 1]]);
        crc16(&buf[..buf.len() - 2]) == received
    } else {
        false
    };

    let mut data = heapless::Vec::<u8, MAX_FRAME_LEN>::new();
    let keep = buf.len().min(MAX_FRAME_LEN);
    // Infallible: `keep` is clamped to the vector capacity.
    let _ = data.extend_from_slice(&buf[..keep]);

    Frame {
        timestamp_ms,
        data,
        crc_ok,
    }
}

/// Take and reset the dropped-frame counter.
pub fn take_dropped() -> u32 {
    DROPPED.swap(0, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Format the inline "frames were lost here" marker into `out`.
pub fn format_dropped_marker(count: u32, out: &mut Marker) -> Result<(), FormatError> {
    out.clear();
    write!(out, "[DROPPED {count} frames]\r\n").map_err(|_| FormatError)
}

/// Format one frame as a single CRLF-terminated analyzer line into `out`.
///
/// Writes straight into the caller's buffer; there is no intermediate copy.
pub fn format_frame(frame: &Frame, out: &mut Line) -> Result<(), FormatError> {
    out.clear();

    let secs = frame.timestamp_ms / 1000;
    let ms = frame.timestamp_ms % 1000;

    if frame.data.len() < 4 {
        write!(out, "[{secs}.{ms:03}] SHORT ({} bytes): ", frame.data.len())
            .map_err(|_| FormatError)?;
        for b in &frame.data {
            write!(out, "{b:02x} ").map_err(|_| FormatError)?;
        }
        out.push_str("\r\n").map_err(|_| FormatError)?;
        return Ok(());
    }

    let slave = frame.data[0];
    let function = frame.data[1];
    let body = &frame.data[2..frame.data.len() - 2]; // without addr, fn, 2 CRC
    let received_crc = u16::from_le_bytes([
        frame.data[frame.data.len() - 2],
        frame.data[frame.data.len() - 1],
    ]);
    let computed_crc = crc16(&frame.data[..frame.data.len() - 2]);

    write!(
        out,
        "[{secs}.{ms:03}] RX slave=0x{slave:02x} func=0x{function:02x} "
    )
    .map_err(|_| FormatError)?;

    if function & EXCEPTION_FLAG != 0 {
        write!(
            out,
            "(Exception: {})",
            format_function(function & !EXCEPTION_FLAG)
        )
        .map_err(|_| FormatError)?;
    } else {
        write!(out, "({})", format_function(function)).map_err(|_| FormatError)?;
    }

    if function & EXCEPTION_FLAG != 0 && body.len() == 1 {
        // A well-formed exception response carries exactly one exception code.
        let code = body[0];
        write!(out, " exception=0x{code:02x} ({})", format_exception(code))
            .map_err(|_| FormatError)?;
    } else if !body.is_empty() {
        out.push_str(" data=").map_err(|_| FormatError)?;
        for (i, b) in body.iter().enumerate() {
            if i > 0 {
                out.push(' ').map_err(|_| FormatError)?;
            }
            write!(out, "{b:02x}").map_err(|_| FormatError)?;
        }
    }

    write!(
        out,
        " crc=0x{received_crc:04x} computed=0x{computed_crc:04x} {}\r\n",
        if frame.crc_ok { "OK" } else { "BAD_CRC" }
    )
    .map_err(|_| FormatError)?;

    Ok(())
}

/// Build a frame from address+function+payload and append a valid CRC.
/// Shared by the self-test tasks and the unit tests.
pub fn build_frame(bytes: &[u8]) -> heapless::Vec<u8, MAX_FRAME_LEN> {
    let mut v = heapless::Vec::<u8, MAX_FRAME_LEN>::new();
    let keep = bytes.len().min(MAX_FRAME_LEN - 2);
    let _ = v.extend_from_slice(&bytes[..keep]);
    let crc = crc16(&v);
    let _ = v.push((crc & 0xff) as u8);
    let _ = v.push((crc >> 8) as u8);
    v
}

// ---------------------------------------------------------------------------
// Bus configuration
// ---------------------------------------------------------------------------

/// Character framing of the sniffed bus. 8 data bits always; the sniffer has
/// never supported anything else and no Modbus RTU device uses 7.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusFormat {
    /// 8 data bits, no parity, 1 stop bit.
    N1,
    /// 8 data bits, even parity, 1 stop bit.
    E1,
    /// 8 data bits, odd parity, 1 stop bit.
    O1,
    /// 8 data bits, no parity, 2 stop bits.
    N2,
}

impl BusFormat {
    /// The wire spelling used on the protocol and in commands.
    pub const fn as_str(self) -> &'static str {
        match self {
            BusFormat::N1 => "8N1",
            BusFormat::E1 => "8E1",
            BusFormat::O1 => "8O1",
            BusFormat::N2 => "8N2",
        }
    }

    /// Parse the wire spelling, case-insensitively.
    pub fn parse(text: &str) -> Option<Self> {
        let b = text.as_bytes();
        if b.len() != 3 || b[0] != b'8' {
            return None;
        }
        match (b[1].to_ascii_uppercase(), b[2]) {
            (b'N', b'1') => Some(BusFormat::N1),
            (b'E', b'1') => Some(BusFormat::E1),
            (b'O', b'1') => Some(BusFormat::O1),
            (b'N', b'2') => Some(BusFormat::N2),
            _ => None,
        }
    }

    /// Stable on-flash encoding. Never renumber these.
    pub const fn code(self) -> u8 {
        match self {
            BusFormat::N1 => 0,
            BusFormat::E1 => 1,
            BusFormat::O1 => 2,
            BusFormat::N2 => 3,
        }
    }

    /// Inverse of [`BusFormat::code`].
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(BusFormat::N1),
            1 => Some(BusFormat::E1),
            2 => Some(BusFormat::O1),
            3 => Some(BusFormat::N2),
            _ => None,
        }
    }
}

/// Runtime-configurable bus parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusConfig {
    pub baud: u32,
    pub format: BusFormat,
}

/// What the firmware runs with when flash holds no valid record.
pub const DEFAULT_BUS: BusConfig = BusConfig {
    baud: 9_600,
    format: BusFormat::E1,
};

/// Lowest accepted baud rate.
pub const MIN_BAUD: u32 = 1_200;
/// Highest accepted baud rate.
pub const MAX_BAUD: u32 = 115_200;

impl BusConfig {
    /// t3.5 frame boundary for this configuration.
    pub const fn frame_timeout_us(&self) -> u64 {
        frame_timeout_us(self.baud)
    }
}

// ---------------------------------------------------------------------------
// Bus configuration persistence record
// ---------------------------------------------------------------------------

/// Size of the on-flash bus record. A multiple of the flash word size (4) so it
/// can be written in one go.
pub const BUS_RECORD_LEN: usize = 16;

/// Magic at the start of the record: "MSB1" (Modbus Sniffer Bus, rev 1).
const BUS_RECORD_MAGIC: u32 = u32::from_le_bytes(*b"MSB1");

/// Record format version. Bump when the layout changes.
const BUS_RECORD_VERSION: u8 = 1;

/// Serialise a bus configuration into its on-flash form.
///
/// Layout, little endian: magic u32, version u8, format u8, reserved u16,
/// baud u32, CRC-16 of bytes 0..12, padding u16.
pub fn encode_bus_record(bus: BusConfig) -> [u8; BUS_RECORD_LEN] {
    let mut raw = [0u8; BUS_RECORD_LEN];
    raw[0..4].copy_from_slice(&BUS_RECORD_MAGIC.to_le_bytes());
    raw[4] = BUS_RECORD_VERSION;
    raw[5] = bus.format.code();
    raw[8..12].copy_from_slice(&bus.baud.to_le_bytes());
    let crc = crc16(&raw[0..12]);
    raw[12..14].copy_from_slice(&crc.to_le_bytes());
    raw
}

/// Parse an on-flash bus record. Returns `None` for anything that is not a
/// valid, current, in-range record - including erased (all-0xff) flash.
pub fn decode_bus_record(raw: &[u8]) -> Option<BusConfig> {
    if raw.len() < BUS_RECORD_LEN {
        return None;
    }
    if u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) != BUS_RECORD_MAGIC {
        return None;
    }
    if raw[4] != BUS_RECORD_VERSION {
        return None;
    }
    if u16::from_le_bytes([raw[12], raw[13]]) != crc16(&raw[0..12]) {
        return None;
    }
    let format = BusFormat::from_code(raw[5])?;
    let baud = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
    if !(MIN_BAUD..=MAX_BAUD).contains(&baud) {
        return None;
    }
    Some(BusConfig { baud, format })
}

// ---------------------------------------------------------------------------
// Commands (client -> device)
// ---------------------------------------------------------------------------

/// A parsed client command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command<'a> {
    /// Reconfigure the sniffed bus and persist the setting.
    Bus(BusConfig),
    /// Ask for a one-line status report.
    Status,
    /// Inject an operator note into the log.
    Note(&'a str),
}

/// Parse one command line (without its terminator).
///
/// The keyword is case-insensitive. On failure the `Err` payload is the text
/// that goes out as `# err <reason>`.
pub fn parse_command(line: &str) -> Result<Command<'_>, &'static str> {
    let line = line.trim_matches([' ', '\t', '\r', '\n']);
    if line.is_empty() {
        return Err("empty command");
    }
    if line.len() > MAX_COMMAND_LEN {
        return Err("command too long");
    }

    let (keyword, rest) = match line.find([' ', '\t']) {
        Some(i) => (&line[..i], line[i + 1..].trim_start_matches([' ', '\t'])),
        None => (line, ""),
    };

    if keyword.eq_ignore_ascii_case("status") {
        if !rest.is_empty() {
            return Err("usage: status");
        }
        return Ok(Command::Status);
    }

    if keyword.eq_ignore_ascii_case("note") {
        if rest.is_empty() {
            return Err("usage: note <text>");
        }
        return Ok(Command::Note(rest));
    }

    if keyword.eq_ignore_ascii_case("bus") {
        let mut parts = rest.split([' ', '\t']).filter(|p| !p.is_empty());
        let baud_text = parts.next().ok_or("usage: bus <baud> <8N1|8E1|8O1|8N2>")?;
        let format_text = parts.next().ok_or("usage: bus <baud> <8N1|8E1|8O1|8N2>")?;
        if parts.next().is_some() {
            return Err("usage: bus <baud> <8N1|8E1|8O1|8N2>");
        }
        let baud: u32 = baud_text.parse().map_err(|_| "baud is not a number")?;
        if !(MIN_BAUD..=MAX_BAUD).contains(&baud) {
            return Err("baud out of range 1200..=115200");
        }
        let format = BusFormat::parse(format_text).ok_or("format must be 8N1, 8E1, 8O1 or 8N2")?;
        return Ok(Command::Bus(BusConfig { baud, format }));
    }

    Err("unknown command (bus, status, note)")
}

// ---------------------------------------------------------------------------
// Status / hello / note / bus line formatters
// ---------------------------------------------------------------------------

/// Everything the `# status` line reports.
#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub uptime_ms: u64,
    pub bus: BusConfig,
    pub wifi_up: bool,
    /// `None` renders as `?`.
    pub ip: Option<[u8; 4]>,
    /// `None` renders as `?`.
    pub rssi: Option<i32>,
    pub frames: u32,
    pub bad_crc: u32,
    pub uart_errors: u32,
    /// Lines lost to the TCP consumer or refused by the ring: real gaps in the
    /// server-side capture.
    pub dropped: u32,
    /// Lines the USB consumer skipped. Grows constantly when no USB host is
    /// reading (e.g. powered from a charger), so it is kept out of `dropped`.
    pub usb_dropped: u32,
}

/// Format the reply to the `status` command.
pub fn format_status(status: &Status, out: &mut Line) -> Result<(), FormatError> {
    out.clear();
    write!(
        out,
        "# status uptime_ms={} bus={} {} wifi={} ip=",
        status.uptime_ms,
        status.bus.baud,
        status.bus.format.as_str(),
        if status.wifi_up { "up" } else { "down" }
    )
    .map_err(|_| FormatError)?;
    match status.ip {
        Some([a, b, c, d]) => write!(out, "{a}.{b}.{c}.{d}").map_err(|_| FormatError)?,
        None => out.push('?').map_err(|_| FormatError)?,
    }
    out.push_str(" rssi=").map_err(|_| FormatError)?;
    match status.rssi {
        Some(rssi) => write!(out, "{rssi}").map_err(|_| FormatError)?,
        None => out.push('?').map_err(|_| FormatError)?,
    }
    write!(
        out,
        " frames={} bad_crc={} uart_errors={} dropped={} usb_dropped={}\r\n",
        status.frames, status.bad_crc, status.uart_errors, status.dropped, status.usb_dropped
    )
    .map_err(|_| FormatError)
}

/// Format the one hello line every new TCP client gets before its replay.
pub fn format_hello(
    name: &str,
    version: &str,
    uptime_ms: u64,
    bus: BusConfig,
    replay_lines: u64,
    out: &mut Line,
) -> Result<(), FormatError> {
    out.clear();
    write!(
        out,
        "# hello {name} {version} uptime_ms={uptime_ms} bus={} {} replay={replay_lines}\r\n",
        bus.baud,
        bus.format.as_str()
    )
    .map_err(|_| FormatError)
}

/// Format the `# bus <baud> <fmt>` line that a bus change puts into the ring.
pub fn format_bus_line(bus: BusConfig, out: &mut Line) -> Result<(), FormatError> {
    out.clear();
    write!(out, "# bus {} {}\r\n", bus.baud, bus.format.as_str()).map_err(|_| FormatError)
}

/// Format the `# note [secs.ms] <text>` line a note puts into the ring.
pub fn format_note(timestamp_ms: u64, text: &str, out: &mut Line) -> Result<(), FormatError> {
    out.clear();
    let secs = timestamp_ms / 1000;
    let ms = timestamp_ms % 1000;
    write!(out, "# note [{secs}.{ms:03}] {text}\r\n").map_err(|_| FormatError)
}

/// Format the `# ok <normalized command>` acknowledgement.
pub fn format_command_ok(command: &Command<'_>, out: &mut Line) -> Result<(), FormatError> {
    out.clear();
    match command {
        Command::Bus(bus) => write!(out, "# ok bus {} {}\r\n", bus.baud, bus.format.as_str()),
        Command::Status => write!(out, "# ok status\r\n"),
        Command::Note(text) => write!(out, "# ok note {text}\r\n"),
    }
    .map_err(|_| FormatError)
}

/// Format the `# err <reason>` rejection.
pub fn format_command_err(reason: &str, out: &mut Line) -> Result<(), FormatError> {
    out.clear();
    write!(out, "# err {reason}\r\n").map_err(|_| FormatError)
}

/// Format the inline "lines were lost here" marker, for a consumer whose cursor
/// fell behind the ring.
pub fn format_dropped_lines(count: u64, out: &mut Marker) -> Result<(), FormatError> {
    out.clear();
    write!(out, "[DROPPED {count} lines]\r\n").map_err(|_| FormatError)
}

// ---------------------------------------------------------------------------
// Multi-consumer line ring
// ---------------------------------------------------------------------------

/// Byte capacity of the firmware's line ring.
pub const RING_BYTES: usize = 64 * 1024;

/// Line-slot capacity of the firmware's line ring. At the ~90 bytes a typical
/// frame line takes, the byte store runs out first, which is the intent: the
/// slot table is never the binding limit.
pub const RING_LINES: usize = 1024;

/// Number of independent cursors a [`LineBus`] can wake.
pub const RING_CONSUMERS: usize = 2;

/// Cursor index of the USB-Serial-JTAG consumer.
pub const CONSUMER_USB: usize = 0;

/// Cursor index of the TCP consumer.
pub const CONSUMER_TCP: usize = 1;

/// The line ring as the firmwares instantiate it.
pub type SnifferBus = LineBus<RING_BYTES, RING_LINES>;

/// What a consumer got out of the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chunk {
    /// Nothing new; the cursor is at the head.
    Empty,
    /// `usize` bytes of one line were written to the output buffer.
    Line(usize),
    /// The cursor had fallen behind the oldest stored line; it skipped ahead
    /// over this many lines. The consumer must emit a marker before the next
    /// line it writes out.
    Dropped(u64),
}

#[derive(Clone, Copy)]
struct Slot {
    start: u32,
    len: u32,
}

/// A ring of complete text lines with per-consumer cursors.
///
/// Lines are stored back to back in a byte ring; a parallel slot table records
/// where each line starts. Lines are numbered with a monotonically increasing
/// `u64` sequence number, and a cursor is just the sequence number the consumer
/// wants next. Pushing never blocks and never fails on a slow consumer: it
/// evicts the oldest lines until the new one fits, and a cursor that gets
/// overtaken notices because its sequence number is below the oldest stored one.
pub struct LineRing<const N: usize, const M: usize> {
    bytes: [u8; N],
    slots: [Slot; M],
    /// Write position in `bytes`.
    head: usize,
    /// Bytes currently occupied.
    used: usize,
    /// Sequence number of the oldest stored line.
    oldest: u64,
    /// Sequence number the next pushed line will get.
    next: u64,
    /// Lines evicted since boot, whether or not anybody had read them.
    evicted: u64,
}

impl<const N: usize, const M: usize> Default for LineRing<N, M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const M: usize> LineRing<N, M> {
    /// An empty ring. `const` so it can live in a `static` and land in `.bss`.
    pub const fn new() -> Self {
        Self {
            bytes: [0u8; N],
            slots: [Slot { start: 0, len: 0 }; M],
            head: 0,
            used: 0,
            oldest: 0,
            next: 0,
            evicted: 0,
        }
    }

    /// Sequence number of the oldest line still held.
    pub const fn oldest_seq(&self) -> u64 {
        self.oldest
    }

    /// Sequence number the next pushed line will get, i.e. one past the newest.
    pub const fn next_seq(&self) -> u64 {
        self.next
    }

    /// Lines currently held.
    pub const fn len(&self) -> usize {
        (self.next - self.oldest) as usize
    }

    /// True when no line is held.
    pub const fn is_empty(&self) -> bool {
        self.next == self.oldest
    }

    /// Lines evicted since boot.
    pub const fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Lines a consumer at `cursor` would still get out of the ring.
    pub const fn pending(&self, cursor: u64) -> u64 {
        let from = if cursor < self.oldest {
            self.oldest
        } else {
            cursor
        };
        self.next.saturating_sub(from)
    }

    fn drop_oldest(&mut self) {
        if self.next == self.oldest {
            return;
        }
        let slot = self.slots[(self.oldest % M as u64) as usize];
        self.used -= slot.len as usize;
        self.oldest += 1;
        self.evicted += 1;
    }

    /// Append one line, evicting as many old ones as it takes to make room.
    ///
    /// Returns `false` only for a line that can never fit (empty, or longer
    /// than the whole byte store); nothing is stored in that case.
    pub fn push(&mut self, line: &[u8]) -> bool {
        if line.is_empty() || line.len() > N {
            return false;
        }

        while self.len() == M || self.used + line.len() > N {
            self.drop_oldest();
        }

        let start = self.head;
        let first = core::cmp::min(line.len(), N - start);
        self.bytes[start..start + first].copy_from_slice(&line[..first]);
        if first < line.len() {
            self.bytes[..line.len() - first].copy_from_slice(&line[first..]);
        }
        self.head = (start + line.len()) % N;
        self.used += line.len();

        self.slots[(self.next % M as u64) as usize] = Slot {
            start: start as u32,
            len: line.len() as u32,
        };
        self.next += 1;
        true
    }

    /// Read the line at `cursor` into `out`, advancing `cursor`.
    ///
    /// A cursor that has fallen behind is snapped to the oldest stored line and
    /// the skipped count is reported; a cursor ahead of the head (which cannot
    /// happen through this API) is snapped back to the head.
    pub fn read(&self, cursor: &mut u64, out: &mut [u8]) -> Chunk {
        if *cursor < self.oldest {
            let missed = self.oldest - *cursor;
            *cursor = self.oldest;
            return Chunk::Dropped(missed);
        }
        if *cursor >= self.next {
            *cursor = self.next;
            return Chunk::Empty;
        }

        let slot = self.slots[(*cursor % M as u64) as usize];
        let len = slot.len as usize;
        *cursor += 1;

        if out.len() < len {
            // Can only happen if the caller's buffer is smaller than MAX_LINE_LEN.
            // Never emit a truncated line; report it as lost instead.
            return Chunk::Dropped(1);
        }

        let start = slot.start as usize;
        let first = core::cmp::min(len, N - start);
        out[..first].copy_from_slice(&self.bytes[start..start + first]);
        if first < len {
            out[first..len].copy_from_slice(&self.bytes[..len - first]);
        }
        Chunk::Line(len)
    }
}

/// A [`LineRing`] shared between one producer and [`RING_CONSUMERS`] consumers.
///
/// The producer side is non-blocking and non-async: a consumer that is not
/// draining can never stall it, and one stalled consumer cannot stall another,
/// because each consumer owns its cursor and its own wakeup signal.
pub struct LineBus<const N: usize, const M: usize> {
    ring: BlockingMutex<CriticalSectionRawMutex, RefCell<LineRing<N, M>>>,
    signals: [Signal<CriticalSectionRawMutex, ()>; RING_CONSUMERS],
}

impl<const N: usize, const M: usize> Default for LineBus<N, M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const M: usize> LineBus<N, M> {
    /// An empty bus. `const` so it can live in a `static`.
    pub const fn new() -> Self {
        Self {
            ring: BlockingMutex::new(RefCell::new(LineRing::new())),
            signals: [const { Signal::new() }; RING_CONSUMERS],
        }
    }

    /// Append one line and wake every consumer. Never blocks.
    pub fn publish(&self, line: &[u8]) -> bool {
        let stored = self.ring.lock(|ring| ring.borrow_mut().push(line));
        if stored {
            for signal in &self.signals {
                signal.signal(());
            }
        }
        stored
    }

    /// Sequence number one past the newest stored line.
    pub fn next_seq(&self) -> u64 {
        self.ring.lock(|ring| ring.borrow().next_seq())
    }

    /// Sequence number of the oldest stored line.
    pub fn oldest_seq(&self) -> u64 {
        self.ring.lock(|ring| ring.borrow().oldest_seq())
    }

    /// Lines a consumer at `cursor` would still get out of the ring.
    pub fn pending(&self, cursor: u64) -> u64 {
        self.ring.lock(|ring| ring.borrow().pending(cursor))
    }

    /// Non-blocking read at `cursor`.
    pub fn try_next(&self, cursor: &mut u64, out: &mut [u8]) -> Chunk {
        self.ring.lock(|ring| ring.borrow().read(cursor, out))
    }

    /// Read at `cursor`, waiting for the producer if the ring is caught up.
    ///
    /// `consumer` selects which wakeup signal to wait on and must be below
    /// [`RING_CONSUMERS`]; indices are [`CONSUMER_USB`] and [`CONSUMER_TCP`].
    pub async fn next(&self, consumer: usize, cursor: &mut u64, out: &mut [u8]) -> Chunk {
        loop {
            match self.try_next(cursor, out) {
                Chunk::Empty => self.signals[consumer].wait().await,
                other => return other,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Host unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(data: &[u8], timestamp_ms: u64) -> Frame {
        let mut v = heapless::Vec::<u8, MAX_FRAME_LEN>::new();
        v.extend_from_slice(data).unwrap();
        let crc_ok = if v.len() >= 4 {
            let received = u16::from_le_bytes([v[v.len() - 2], v[v.len() - 1]]);
            crc16(&v[..v.len() - 2]) == received
        } else {
            false
        };
        Frame {
            timestamp_ms,
            data: v,
            crc_ok,
        }
    }

    fn line_of(f: &Frame) -> Line {
        let mut out = Line::new();
        format_frame(f, &mut out).unwrap();
        out
    }

    #[test]
    fn crc16_known_vector() {
        // slave 01, function 03, read 2 regs from 0000 -> 01 03 00 00 00 02 C4 0B
        let f = [0x01, 0x03, 0x00, 0x00, 0x00, 0x02];
        assert_eq!(crc16(&f), 0x0BC4);
    }

    #[test]
    fn crc16_empty_is_init_value() {
        assert_eq!(crc16(&[]), 0xFFFF);
    }

    #[test]
    fn format_frame_ok() {
        let f = frame(&build_frame(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x0a]), 12);
        assert!(f.crc_ok);
        assert_eq!(
            line_of(&f).as_str(),
            "[0.012] RX slave=0x01 func=0x03 (Read Holding Registers) \
             data=00 00 00 0a crc=0xcdc5 computed=0xcdc5 OK\r\n"
        );
    }

    #[test]
    fn format_frame_bad_crc() {
        let mut bad = build_frame(&[0x02, 0x06, 0x00, 0x10, 0x12, 0x34]);
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        let f = frame(&bad, 1_045);
        assert!(!f.crc_ok);
        let line = line_of(&f);
        assert!(line.starts_with(
            "[1.045] RX slave=0x02 func=0x06 (Write Single Register) data=00 10 12 34 crc=0x"
        ));
        assert!(line.ends_with(" BAD_CRC\r\n"));
    }

    #[test]
    fn format_frame_no_payload_has_no_data_section() {
        // addr + func + CRC only: nothing between them.
        let f = frame(&build_frame(&[0x11, 0x07]), 0);
        assert_eq!(
            line_of(&f).as_str(),
            "[0.000] RX slave=0x11 func=0x07 (Read Exception Status) \
             crc=0x224c computed=0x224c OK\r\n"
        );
    }

    #[test]
    fn format_frame_short() {
        let f = frame(&[0xaa, 0xbb, 0xcc], 2_500);
        assert!(!f.crc_ok);
        assert_eq!(
            line_of(&f).as_str(),
            "[2.500] SHORT (3 bytes): aa bb cc \r\n"
        );
    }

    #[test]
    fn format_frame_single_space_before_crc() {
        let f = frame(&build_frame(&[0x01, 0x03, 0x02, 0x00, 0x2a]), 0);
        assert!(!line_of(&f).contains("  crc="));
    }

    #[test]
    fn format_frame_unknown_function() {
        let f = frame(&build_frame(&[0x01, 0x42, 0x02]), 0);
        assert!(line_of(&f).contains("func=0x42 (Unknown) data=02 crc="));
    }

    #[test]
    fn format_frame_exception_response() {
        let f = frame(&build_frame(&[0x01, 0x83, 0x02]), 0);
        assert_eq!(
            line_of(&f).as_str(),
            "[0.000] RX slave=0x01 func=0x83 (Exception: Read Holding Registers) \
             exception=0x02 (Illegal Data Address) crc=0xf1c0 computed=0xf1c0 OK\r\n"
        );
    }

    #[test]
    fn format_frame_exception_unknown_code_and_function() {
        let f = frame(&build_frame(&[0x07, 0xc2, 0x77]), 0);
        assert!(
            line_of(&f).contains("func=0xc2 (Exception: Unknown) exception=0x77 (Unknown) crc=")
        );
    }

    #[test]
    fn format_frame_malformed_exception_falls_back_to_hex() {
        // Exception flag set but two body bytes: not a valid exception PDU.
        let f = frame(&build_frame(&[0x01, 0x83, 0x02, 0x03]), 0);
        assert!(line_of(&f).contains("(Exception: Read Holding Registers) data=02 03 crc="));
    }

    #[test]
    fn format_frame_max_length_fits_buffer() {
        let payload = [0x5au8; MAX_FRAME_LEN - 2];
        let f = frame(&build_frame(&payload), u64::from(u32::MAX));
        let mut out = Line::new();
        assert_eq!(format_frame(&f, &mut out), Ok(()));
        assert!(out.len() < MAX_LINE_LEN);
    }

    #[test]
    fn dropped_marker_text() {
        let mut m = Marker::new();
        format_dropped_marker(7, &mut m).unwrap();
        assert_eq!(m.as_str(), "[DROPPED 7 frames]\r\n");
    }

    #[test]
    fn dropped_marker_fits_u32_max() {
        let mut m = Marker::new();
        assert_eq!(format_dropped_marker(u32::MAX, &mut m), Ok(()));
    }

    #[test]
    fn dropped_lines_marker_text_and_worst_case() {
        let mut m = Marker::new();
        format_dropped_lines(3, &mut m).unwrap();
        assert_eq!(m.as_str(), "[DROPPED 3 lines]\r\n");
        assert_eq!(format_dropped_lines(u64::MAX, &mut m), Ok(()));
    }

    // -----------------------------------------------------------------------
    // Bus configuration, persistence record and commands
    // -----------------------------------------------------------------------

    #[test]
    fn bus_format_round_trips_text() {
        for (text, fmt) in [
            ("8N1", BusFormat::N1),
            ("8E1", BusFormat::E1),
            ("8O1", BusFormat::O1),
            ("8N2", BusFormat::N2),
        ] {
            assert_eq!(BusFormat::parse(text), Some(fmt));
            assert_eq!(BusFormat::parse(&text.to_lowercase()), Some(fmt));
            assert_eq!(fmt.as_str(), text);
            assert_eq!(BusFormat::from_code(fmt.code()), Some(fmt));
        }
        assert_eq!(BusFormat::parse("7E1"), None);
        assert_eq!(BusFormat::parse("8E2"), None);
        assert_eq!(BusFormat::parse(""), None);
        assert_eq!(BusFormat::from_code(4), None);
    }

    #[test]
    fn bus_record_round_trips() {
        for bus in [
            DEFAULT_BUS,
            BusConfig {
                baud: 115_200,
                format: BusFormat::N2,
            },
            BusConfig {
                baud: 1_200,
                format: BusFormat::O1,
            },
        ] {
            let raw = encode_bus_record(bus);
            assert_eq!(raw.len() % 4, 0, "must be flash-word aligned");
            assert_eq!(decode_bus_record(&raw), Some(bus));
        }
    }

    #[test]
    fn bus_record_rejects_garbage() {
        assert_eq!(decode_bus_record(&[0xff; BUS_RECORD_LEN]), None);
        assert_eq!(decode_bus_record(&[0x00; BUS_RECORD_LEN]), None);
        assert_eq!(decode_bus_record(&[]), None);

        let mut raw = encode_bus_record(DEFAULT_BUS);
        raw[9] ^= 0xff; // corrupt the baud rate
        assert_eq!(decode_bus_record(&raw), None, "CRC must catch bit rot");

        let mut raw = encode_bus_record(DEFAULT_BUS);
        raw[4] = 2; // future version
        assert_eq!(decode_bus_record(&raw), None);

        // In-CRC but out-of-range baud rate.
        let mut raw = encode_bus_record(DEFAULT_BUS);
        raw[8..12].copy_from_slice(&230_400u32.to_le_bytes());
        let crc = crc16(&raw[0..12]);
        raw[12..14].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(decode_bus_record(&raw), None);
    }

    #[test]
    fn parse_command_accepts_the_three_commands() {
        assert_eq!(parse_command("status"), Ok(Command::Status));
        assert_eq!(parse_command("STATUS"), Ok(Command::Status));
        assert_eq!(parse_command("  status \r\n"), Ok(Command::Status));
        assert_eq!(
            parse_command("note hello world"),
            Ok(Command::Note("hello world"))
        );
        assert_eq!(parse_command("NoTe  padded"), Ok(Command::Note("padded")));
        assert_eq!(
            parse_command("bus 19200 8n1"),
            Ok(Command::Bus(BusConfig {
                baud: 19_200,
                format: BusFormat::N1
            }))
        );
        assert_eq!(
            parse_command("BUS   9600   8E1"),
            Ok(Command::Bus(DEFAULT_BUS))
        );
    }

    #[test]
    fn parse_command_rejects_bad_input() {
        assert!(parse_command("").is_err());
        assert!(parse_command("   ").is_err());
        assert!(parse_command("reboot").is_err());
        assert!(parse_command("note").is_err());
        assert!(parse_command("status now").is_err());
        assert!(parse_command("bus").is_err());
        assert!(parse_command("bus 9600").is_err());
        assert!(parse_command("bus 9600 8E1 extra").is_err());
        assert!(parse_command("bus abc 8E1").is_err());
        assert!(parse_command("bus 300 8E1").is_err());
        assert!(parse_command("bus 230400 8E1").is_err());
        assert!(parse_command("bus 9600 7E1").is_err());

        let long = "note ".to_string() + &"x".repeat(MAX_COMMAND_LEN);
        assert!(parse_command(&long).is_err());
    }

    #[test]
    fn status_line_format() {
        let mut out = Line::new();
        format_status(
            &Status {
                uptime_ms: 123_456,
                bus: DEFAULT_BUS,
                wifi_up: true,
                ip: Some([192, 168, 1, 42]),
                rssi: Some(-57),
                frames: 9,
                bad_crc: 1,
                uart_errors: 2,
                dropped: 3,
                usb_dropped: 4,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(
            out.as_str(),
            "# status uptime_ms=123456 bus=9600 8E1 wifi=up ip=192.168.1.42 rssi=-57 \
             frames=9 bad_crc=1 uart_errors=2 dropped=3 usb_dropped=4\r\n"
        );
    }

    #[test]
    fn status_line_renders_unknowns_as_question_marks() {
        let mut out = Line::new();
        format_status(
            &Status {
                uptime_ms: 0,
                bus: BusConfig {
                    baud: 115_200,
                    format: BusFormat::N2,
                },
                wifi_up: false,
                ip: None,
                rssi: None,
                frames: 0,
                bad_crc: 0,
                uart_errors: 0,
                dropped: 0,
                usb_dropped: 0,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(
            out.as_str(),
            "# status uptime_ms=0 bus=115200 8N2 wifi=down ip=? rssi=? \
             frames=0 bad_crc=0 uart_errors=0 dropped=0 usb_dropped=0\r\n"
        );
    }

    #[test]
    fn hello_note_bus_and_reply_lines() {
        let mut out = Line::new();
        format_hello("sniffer-esp32c6", "0.1.0", 4_711, DEFAULT_BUS, 12, &mut out).unwrap();
        assert_eq!(
            out.as_str(),
            "# hello sniffer-esp32c6 0.1.0 uptime_ms=4711 bus=9600 8E1 replay=12\r\n"
        );

        format_note(62_045, "pump started", &mut out).unwrap();
        assert_eq!(out.as_str(), "# note [62.045] pump started\r\n");

        format_bus_line(
            BusConfig {
                baud: 19_200,
                format: BusFormat::O1,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(out.as_str(), "# bus 19200 8O1\r\n");

        format_command_ok(&Command::Bus(DEFAULT_BUS), &mut out).unwrap();
        assert_eq!(out.as_str(), "# ok bus 9600 8E1\r\n");
        format_command_ok(&Command::Status, &mut out).unwrap();
        assert_eq!(out.as_str(), "# ok status\r\n");
        format_command_ok(&Command::Note("hi"), &mut out).unwrap();
        assert_eq!(out.as_str(), "# ok note hi\r\n");
        format_command_err("unknown command", &mut out).unwrap();
        assert_eq!(out.as_str(), "# err unknown command\r\n");
    }

    // -----------------------------------------------------------------------
    // Line ring
    // -----------------------------------------------------------------------

    /// Small ring so overrun is reachable in a test: 32 bytes, 4 slots.
    type TestRing = LineRing<32, 4>;

    fn take(ring: &TestRing, cursor: &mut u64) -> Chunk {
        let mut buf = [0u8; 64];
        ring.read(cursor, &mut buf)
    }

    fn take_text(ring: &TestRing, cursor: &mut u64) -> String<64> {
        let mut buf = [0u8; 64];
        match ring.read(cursor, &mut buf) {
            Chunk::Line(n) => String::try_from(core::str::from_utf8(&buf[..n]).unwrap()).unwrap(),
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn ring_fifo_and_empty() {
        let mut ring = TestRing::new();
        let mut cursor = 0u64;
        assert_eq!(take(&ring, &mut cursor), Chunk::Empty);
        assert!(ring.is_empty());

        assert!(ring.push(b"a\r\n"));
        assert!(ring.push(b"bb\r\n"));
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.pending(0), 2);
        assert_eq!(take_text(&ring, &mut cursor).as_str(), "a\r\n");
        assert_eq!(take_text(&ring, &mut cursor).as_str(), "bb\r\n");
        assert_eq!(take(&ring, &mut cursor), Chunk::Empty);
        assert_eq!(ring.pending(cursor), 0);
    }

    #[test]
    fn ring_rejects_unstorable_lines() {
        let mut ring = TestRing::new();
        assert!(!ring.push(b""));
        assert!(!ring.push(&[b'x'; 33]));
        assert_eq!(ring.len(), 0);
        assert!(ring.push(&[b'x'; 32]), "a line exactly the ring size fits");
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn ring_slot_table_limit_evicts() {
        let mut ring = TestRing::new();
        // 4 slots, 2 bytes each: the slot table runs out before the bytes do.
        for i in 0..6u8 {
            assert!(ring.push(&[b'0' + i, b'\n']));
        }
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.oldest_seq(), 2);
        assert_eq!(ring.next_seq(), 6);
        assert_eq!(ring.evicted(), 2);
    }

    #[test]
    fn ring_byte_store_limit_evicts_and_wraps() {
        let mut ring = TestRing::new();
        // 10-byte lines: three fit in 32 bytes, the fourth evicts the first and
        // wraps around the end of the byte store.
        for i in 0..4u8 {
            let mut line = [b'-'; 10];
            line[0] = b'A' + i;
            assert!(ring.push(&line));
        }
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.oldest_seq(), 1);

        let mut cursor = 1u64;
        assert_eq!(take_text(&ring, &mut cursor).as_str(), "B---------");
        assert_eq!(take_text(&ring, &mut cursor).as_str(), "C---------");
        // This one straddles the end of the byte store.
        assert_eq!(take_text(&ring, &mut cursor).as_str(), "D---------");
        assert_eq!(take(&ring, &mut cursor), Chunk::Empty);
    }

    #[test]
    fn ring_overrun_cursor_reports_then_resumes() {
        let mut ring = TestRing::new();
        let mut cursor = 0u64;

        assert!(ring.push(b"1\r\n"));
        assert_eq!(take_text(&ring, &mut cursor).as_str(), "1\r\n");

        // Push 6 more into a 4-slot ring: lines 1 and 2 are evicted unread.
        for i in 2..8u8 {
            assert!(ring.push(&[b'0' + i, b'\r', b'\n']));
        }
        assert_eq!(cursor, 1);
        assert_eq!(ring.oldest_seq(), 3);

        // The consumer missed sequence numbers 1 and 2.
        assert_eq!(take(&ring, &mut cursor), Chunk::Dropped(2));
        assert_eq!(cursor, 3);
        // ...and then carries on from the oldest line still held.
        assert_eq!(take_text(&ring, &mut cursor).as_str(), "4\r\n");
        assert_eq!(take_text(&ring, &mut cursor).as_str(), "5\r\n");
        assert_eq!(take_text(&ring, &mut cursor).as_str(), "6\r\n");
        assert_eq!(take_text(&ring, &mut cursor).as_str(), "7\r\n");
        assert_eq!(take(&ring, &mut cursor), Chunk::Empty);
    }

    #[test]
    fn ring_cursors_are_independent() {
        let mut ring = TestRing::new();
        let mut fast = 0u64;
        let mut slow = 0u64;

        assert!(ring.push(b"x\r\n"));
        assert_eq!(take_text(&ring, &mut fast).as_str(), "x\r\n");
        assert_eq!(take(&ring, &mut fast), Chunk::Empty);

        // The slow consumer has not moved, and that does not hold anything back:
        // a fifth line into a 4-slot ring evicts sequence number 0.
        for i in 0..4u8 {
            assert!(ring.push(&[b'a' + i, b'\r', b'\n']));
        }
        assert_eq!(ring.oldest_seq(), 1);
        assert_eq!(take(&ring, &mut slow), Chunk::Dropped(1));
        // The fast cursor was not behind the oldest line, so it lost nothing.
        assert!(matches!(take(&ring, &mut fast), Chunk::Line(3)));
    }

    #[test]
    fn ring_pending_counts_replay() {
        let mut ring = TestRing::new();
        for i in 0..6u8 {
            assert!(ring.push(&[b'0' + i, b'\n']));
        }
        // 4 slots held, numbered 2..6.
        assert_eq!(ring.pending(0), 4, "a cursor left behind replays the ring");
        assert_eq!(ring.pending(4), 2);
        assert_eq!(ring.pending(6), 0);
        assert_eq!(
            ring.pending(99),
            0,
            "a cursor past the head replays nothing"
        );
    }

    #[test]
    fn ring_refuses_to_truncate_into_a_small_buffer() {
        let mut ring = TestRing::new();
        assert!(ring.push(b"0123456789"));
        let mut cursor = 0u64;
        let mut small = [0u8; 4];
        assert_eq!(ring.read(&mut cursor, &mut small), Chunk::Dropped(1));
        assert_eq!(cursor, 1, "the unreadable line is skipped, not retried");
    }

    #[test]
    fn bus_publishes_and_serves_two_cursors() {
        let bus = LineBus::<32, 4>::new();
        let mut usb = 0u64;
        let mut tcp = 0u64;
        let mut out = [0u8; 64];

        assert!(bus.publish(b"one\r\n"));
        assert_eq!(bus.next_seq(), 1);
        assert_eq!(bus.pending(0), 1);
        assert_eq!(bus.try_next(&mut usb, &mut out), Chunk::Line(5));
        assert_eq!(&out[..5], b"one\r\n");
        assert_eq!(bus.try_next(&mut usb, &mut out), Chunk::Empty);
        // The TCP cursor still has the line waiting for it.
        assert_eq!(bus.try_next(&mut tcp, &mut out), Chunk::Line(5));
        assert_eq!(bus.oldest_seq(), 0);
    }

    #[test]
    fn full_frame_line_fits_the_ring() {
        // The worst-case formatted frame must be storable, otherwise `push`
        // would silently refuse the very frames we most want to see.
        let payload = [0x5au8; MAX_FRAME_LEN - 2];
        let f = make_frame(&build_frame(&payload), 1_234);
        let mut line = Line::new();
        format_frame(&f, &mut line).unwrap();
        let mut ring = LineRing::<RING_BYTES, RING_LINES>::new();
        assert!(ring.push(line.as_bytes()));
        assert!(line.len() <= MAX_LINE_LEN);
    }

    #[test]
    fn frame_timeout_matches_spec() {
        // 9600 8E1: 11 bits / 9600 = 1145.8 us per char -> 1145 * 4.
        assert_eq!(frame_timeout_us(9_600), 4_580);
        // 19200 8E1: 11 bits / 19200 = 572.9 us per char -> 572 * 4.
        assert_eq!(frame_timeout_us(19_200), 2_288);
        // Above 19200 the spec fixes t3.5 at 1750 us.
        assert_eq!(frame_timeout_us(115_200), 1_750);
        assert_eq!(frame_timeout_us(57_600), 1_750);
        assert_eq!(frame_timeout_us(19_201), 1_750);
    }

    /// Fills the channel, then checks that the overflow is counted rather than
    /// blocking or silently vanishing. Runs last-ish but must not race with
    /// other tests, so it owns the globals: Rust runs tests in threads, hence
    /// the serialising mutex.
    #[test]
    fn emit_frame_counts_overflow() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap();

        while FRAME_CHANNEL.try_receive().is_ok() {}
        let _ = take_dropped();

        let f = build_frame(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x01]);
        for i in 0..FRAME_CHANNEL_DEPTH {
            emit_frame(&f, i as u64);
        }
        assert_eq!(DROPPED.load(Ordering::Relaxed), 0);

        // Channel is full now: the next three are dropped and counted.
        for _ in 0..3 {
            emit_frame(&f, 99);
        }
        assert_eq!(take_dropped(), 3);
        assert_eq!(take_dropped(), 0, "take_dropped must reset the counter");

        // The frames that did make it are intact and in order.
        for i in 0..FRAME_CHANNEL_DEPTH {
            let got = FRAME_CHANNEL.try_receive().expect("queued frame");
            assert_eq!(got.timestamp_ms, i as u64);
            assert!(got.crc_ok);
        }
        assert!(FRAME_CHANNEL.try_receive().is_err());
    }
}
