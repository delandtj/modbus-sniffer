//! Parsing of the lines the sniffer device sends over TCP port 4000.
//!
//! The device speaks a line protocol (CRLF out, LF in). Everything in here is
//! pure string work with no I/O, so it is cheap to unit-test and is shared by
//! the capture daemon (which only needs the uptime stamp and the hello) and the
//! analyzer (which needs the decoded frame bytes).

/// A `# hello ...` greeting, sent as the first line of every connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub firmware: String,
    pub version: String,
    pub uptime_ms: u64,
    pub baud: Option<u32>,
    pub format: Option<String>,
    pub replay: Option<u64>,
}

/// One decoded `RX ...` frame line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameLine {
    pub slave: u8,
    pub func: u8,
    /// PDU body: everything between the function code and the CRC. For a
    /// well-formed exception response this is the single exception code byte.
    pub body: Vec<u8>,
    pub crc: u16,
    pub computed: u16,
    pub crc_ok: bool,
}

/// What a single device line turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceLine {
    Frame(FrameLine),
    Short(Vec<u8>),
    /// `[DROPPED n frames]` / `[DROPPED n lines]`.
    Dropped {
        count: u64,
        unit: String,
    },
    Hello(Hello),
    /// `# bus <baud> <fmt>`.
    Bus {
        baud: u32,
        format: String,
    },
    /// `# note [secs.ms] <text>` - the text only.
    Note(String),
    /// Any other `# ` status line, verbatim (without the `# `).
    Status(String),
    /// Something we could not classify.
    Unknown(String),
}

/// Milliseconds of device uptime carried by this line, if it carries one.
///
/// Frame, SHORT and `# note` lines are stamped `[<secs>.<ms3>]`; markers and
/// status lines are not. `[DROPPED ...]` also starts with `[` but has no
/// numeric stamp, so the digit check matters.
pub fn uptime_ms(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("# note ").unwrap_or(line);
    let inner = rest.strip_prefix('[')?;
    let end = inner.find(']')?;
    parse_stamp(&inner[..end])
}

fn parse_stamp(s: &str) -> Option<u64> {
    let (secs, millis) = s.split_once('.')?;
    if secs.is_empty() || millis.is_empty() {
        return None;
    }
    if !secs.bytes().all(|b| b.is_ascii_digit()) || !millis.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs: u64 = secs.parse().ok()?;
    let millis: u64 = millis.parse().ok()?;
    secs.checked_mul(1000)?.checked_add(millis)
}

/// Strip a leading `[<secs>.<ms3>] ` stamp, returning the remainder.
pub fn strip_stamp(line: &str) -> &str {
    if let Some(inner) = line.strip_prefix('[') {
        if let Some(end) = inner.find(']') {
            if parse_stamp(&inner[..end]).is_some() {
                return inner[end + 1..].trim_start();
            }
        }
    }
    line
}

/// Parse `# hello <firmware> <version> uptime_ms=<n> bus=<baud> <fmt> replay=<n>`.
///
/// Deliberately lenient about field order and about missing trailing fields, so
/// a firmware that grows another key does not stop the daemon from working.
pub fn parse_hello(line: &str) -> Option<Hello> {
    let rest = line.strip_prefix("# hello ")?;
    let tokens: Vec<&str> = rest.split_whitespace().collect();

    let mut uptime_ms = None;
    let mut baud = None;
    let mut format = None;
    let mut replay = None;
    let mut positional: Vec<&str> = Vec::new();

    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if let Some(v) = t.strip_prefix("uptime_ms=") {
            uptime_ms = v.parse().ok();
        } else if let Some(v) = t.strip_prefix("bus=") {
            baud = v.parse().ok();
            // The frame format is the bare token right after `bus=`.
            if let Some(next) = tokens.get(i + 1) {
                if is_frame_format(next) {
                    format = Some((*next).to_string());
                    i += 1;
                }
            }
        } else if let Some(v) = t.strip_prefix("replay=") {
            replay = v.parse().ok();
        } else if !t.contains('=') {
            positional.push(t);
        }
        i += 1;
    }

    Some(Hello {
        firmware: positional.first().copied().unwrap_or("unknown").to_string(),
        version: positional.get(1).copied().unwrap_or("unknown").to_string(),
        uptime_ms: uptime_ms?,
        baud,
        format,
        replay,
    })
}

/// `8N1`, `8E1`, `8O1`, `8N2` and friends.
pub fn is_frame_format(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 3
        && b[0].is_ascii_digit()
        && matches!(b[1], b'N' | b'E' | b'O')
        && b[2].is_ascii_digit()
}

/// Classify one device line (with its `[stamp]` still attached, if any).
pub fn parse_line(line: &str) -> DeviceLine {
    let line = line.trim_end_matches(['\r', '\n']);

    if let Some(hello) = parse_hello(line) {
        return DeviceLine::Hello(hello);
    }
    if let Some(rest) = line.strip_prefix("# note ") {
        return DeviceLine::Note(strip_stamp(rest).to_string());
    }
    if let Some(rest) = line.strip_prefix("# bus ") {
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if let (Some(baud), Some(fmt)) = (
            tokens.first().and_then(|t| t.parse::<u32>().ok()),
            tokens.get(1),
        ) {
            return DeviceLine::Bus {
                baud,
                format: (*fmt).to_string(),
            };
        }
        return DeviceLine::Status(format!("bus {rest}"));
    }
    if let Some(rest) = line.strip_prefix("# ") {
        return DeviceLine::Status(rest.to_string());
    }
    if let Some(d) = parse_dropped(line) {
        return d;
    }

    let payload = strip_stamp(line);
    if let Some(rest) = payload.strip_prefix("SHORT ") {
        if let Some((_, hex)) = rest.split_once(':') {
            if let Some(bytes) = parse_hex_bytes(hex.split_whitespace()) {
                return DeviceLine::Short(bytes);
            }
        }
        return DeviceLine::Unknown(line.to_string());
    }
    if payload.starts_with("RX ") {
        if let Some(f) = parse_frame(payload) {
            return DeviceLine::Frame(f);
        }
    }
    DeviceLine::Unknown(line.to_string())
}

fn parse_dropped(line: &str) -> Option<DeviceLine> {
    let inner = line.strip_prefix("[DROPPED ")?.strip_suffix(']')?;
    let (count, unit) = inner.split_once(' ')?;
    Some(DeviceLine::Dropped {
        count: count.parse().ok()?,
        unit: unit.to_string(),
    })
}

fn parse_hex_bytes<'a>(it: impl Iterator<Item = &'a str>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for t in it {
        out.push(u8::from_str_radix(t, 16).ok()?);
    }
    Some(out)
}

/// Parse the body of an `RX ...` line (stamp already stripped).
fn parse_frame(payload: &str) -> Option<FrameLine> {
    let tokens: Vec<&str> = payload.split_whitespace().collect();

    let mut slave = None;
    let mut func = None;
    let mut crc = None;
    let mut computed = None;
    let mut exception = None;
    let mut data_at = None;
    let mut crc_at = None;

    for (i, t) in tokens.iter().enumerate() {
        if let Some(v) = t.strip_prefix("slave=0x") {
            slave = u8::from_str_radix(v, 16).ok();
        } else if let Some(v) = t.strip_prefix("func=0x") {
            func = u8::from_str_radix(v, 16).ok();
        } else if let Some(v) = t.strip_prefix("exception=0x") {
            exception = u8::from_str_radix(v, 16).ok();
        } else if let Some(v) = t.strip_prefix("crc=0x") {
            crc = u16::from_str_radix(v, 16).ok();
            crc_at = Some(i);
        } else if let Some(v) = t.strip_prefix("computed=0x") {
            computed = u16::from_str_radix(v, 16).ok();
        } else if let Some(v) = t.strip_prefix("data=") {
            data_at = Some(i);
            // `data=` is followed by the first byte on the same token.
            if u8::from_str_radix(v, 16).is_err() {
                return None;
            }
        }
    }

    let body = match (data_at, crc_at) {
        (Some(start), Some(end)) if end > start => {
            let mut hex: Vec<&str> = Vec::with_capacity(end - start);
            hex.push(tokens[start].trim_start_matches("data="));
            hex.extend_from_slice(&tokens[start + 1..end]);
            parse_hex_bytes(hex.into_iter())?
        }
        _ => match exception {
            Some(code) => vec![code],
            None => Vec::new(),
        },
    };

    Some(FrameLine {
        slave: slave?,
        func: func?,
        body,
        crc: crc?,
        computed: computed?,
        crc_ok: tokens.last().is_some_and(|t| *t == "OK"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK_LINE: &str = "[12.345] RX slave=0x01 func=0x03 (Read Holding Registers) \
         data=00 00 00 0a crc=0xcdc5 computed=0xcdc5 OK";

    #[test]
    fn uptime_from_frame_line() {
        assert_eq!(uptime_ms(OK_LINE), Some(12_345));
        assert_eq!(uptime_ms("[0.000] SHORT (3 bytes): aa bb cc "), Some(0));
    }

    #[test]
    fn uptime_from_note_line() {
        assert_eq!(uptime_ms("# note [7.002] setpoint 26 -> 27"), Some(7_002));
    }

    #[test]
    fn markers_and_status_have_no_uptime() {
        assert_eq!(uptime_ms("[DROPPED 4 frames]"), None);
        assert_eq!(uptime_ms("# status uptime_ms=900 bus=9600"), None);
        assert_eq!(uptime_ms("# ok note stored"), None);
    }

    #[test]
    fn hello_round_trip() {
        let h =
            parse_hello("# hello sniffer-esp32c6 0.1.0 uptime_ms=123456 bus=9600 8E1 replay=12")
                .expect("hello");
        assert_eq!(
            h,
            Hello {
                firmware: "sniffer-esp32c6".into(),
                version: "0.1.0".into(),
                uptime_ms: 123_456,
                baud: Some(9_600),
                format: Some("8E1".into()),
                replay: Some(12),
            }
        );
    }

    #[test]
    fn hello_tolerates_missing_tail() {
        let h = parse_hello("# hello sniffer-stm32f411 0.2 uptime_ms=7").expect("hello");
        assert_eq!(h.uptime_ms, 7);
        assert_eq!(h.baud, None);
        assert_eq!(h.replay, None);
    }

    #[test]
    fn hello_needs_uptime() {
        assert!(parse_hello("# hello sniffer-esp32c6 0.1.0 bus=9600 8N1").is_none());
    }

    #[test]
    fn frame_line_decodes_body() {
        match parse_line(OK_LINE) {
            DeviceLine::Frame(f) => {
                assert_eq!(f.slave, 0x01);
                assert_eq!(f.func, 0x03);
                assert_eq!(f.body, vec![0x00, 0x00, 0x00, 0x0a]);
                assert_eq!(f.crc, 0xcdc5);
                assert_eq!(f.computed, 0xcdc5);
                assert!(f.crc_ok);
            }
            other => panic!("expected frame, got {other:?}"),
        }
    }

    #[test]
    fn bad_crc_is_flagged() {
        let l = "[1.045] RX slave=0x02 func=0x06 (Write Single Register) data=00 10 12 34 \
                 crc=0x1234 computed=0x5678 BAD_CRC";
        match parse_line(l) {
            DeviceLine::Frame(f) => assert!(!f.crc_ok),
            other => panic!("expected frame, got {other:?}"),
        }
    }

    #[test]
    fn exception_body_is_the_code() {
        let l = "[0.000] RX slave=0x01 func=0x83 (Exception: Read Holding Registers) \
                 exception=0x02 (Illegal Data Address) crc=0xf1c0 computed=0xf1c0 OK";
        match parse_line(l) {
            DeviceLine::Frame(f) => {
                assert_eq!(f.func, 0x83);
                assert_eq!(f.body, vec![0x02]);
            }
            other => panic!("expected frame, got {other:?}"),
        }
    }

    #[test]
    fn frame_without_body() {
        let l = "[0.000] RX slave=0x11 func=0x07 (Read Exception Status) \
                 crc=0x224c computed=0x224c OK";
        match parse_line(l) {
            DeviceLine::Frame(f) => assert!(f.body.is_empty()),
            other => panic!("expected frame, got {other:?}"),
        }
    }

    #[test]
    fn short_frame_bytes() {
        assert_eq!(
            parse_line("[2.500] SHORT (3 bytes): aa bb cc "),
            DeviceLine::Short(vec![0xaa, 0xbb, 0xcc])
        );
    }

    #[test]
    fn dropped_markers() {
        assert_eq!(
            parse_line("[DROPPED 4 frames]"),
            DeviceLine::Dropped {
                count: 4,
                unit: "frames".into()
            }
        );
        assert_eq!(
            parse_line("[DROPPED 9 lines]"),
            DeviceLine::Dropped {
                count: 9,
                unit: "lines".into()
            }
        );
    }

    #[test]
    fn status_variants() {
        assert_eq!(
            parse_line("# bus 19200 8N1"),
            DeviceLine::Bus {
                baud: 19_200,
                format: "8N1".into()
            }
        );
        assert_eq!(
            parse_line("# note [7.002] setpoint 26 -> 27"),
            DeviceLine::Note("setpoint 26 -> 27".into())
        );
        assert_eq!(
            parse_line("# ok note"),
            DeviceLine::Status("ok note".into())
        );
    }

    /// The formatter in `modbus-sniffer-core` is the real producer of these
    /// lines, so parse its own output rather than a hand-written copy.
    #[test]
    fn parses_core_formatter_output() {
        use modbus_sniffer_core as core;
        let raw = core::build_frame(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x0a]);
        let frame = core::Frame {
            timestamp_ms: 12_345,
            data: raw,
            crc_ok: true,
        };
        let mut line = core::Line::new();
        core::format_frame(&frame, &mut line).unwrap();

        assert_eq!(uptime_ms(line.trim_end()), Some(12_345));
        match parse_line(&line) {
            DeviceLine::Frame(f) => {
                assert_eq!(f.slave, 0x01);
                assert_eq!(f.func, 0x03);
                assert_eq!(f.body, vec![0x00, 0x00, 0x00, 0x0a]);
                assert!(f.crc_ok);
            }
            other => panic!("expected frame, got {other:?}"),
        }
    }
}
