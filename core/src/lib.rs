//! Board-independent Modbus RTU sniffer core.
//!
//! Everything in here is target-agnostic: CRC, function-code names, frame
//! framing constants, the sniffer -> output channel, and the line formatter.
//! It deliberately does NOT depend on `embassy-time`; timestamps are passed in
//! by the firmware so this crate builds and unit-tests on the host.

#![cfg_attr(not(test), no_std)]

use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
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

/// Capacity of the dropped-frame marker line.
pub const MAX_MARKER_LEN: usize = 32;

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

    let frame = Frame {
        timestamp_ms,
        data,
        crc_ok,
    };

    if FRAME_CHANNEL.try_send(frame).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
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
