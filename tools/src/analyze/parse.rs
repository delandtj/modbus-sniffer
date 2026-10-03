//! Turning capture-log files back into frames, notes and events.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset};

use crate::device::{self, DeviceLine};
use crate::logfile;

/// Wall-clock time as stored in the log (RFC3339 with its original offset).
pub type Time = DateTime<FixedOffset>;

/// One CRC-checked frame recovered from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub time: Time,
    pub slave: u8,
    pub func: u8,
    pub body: Vec<u8>,
    pub crc_ok: bool,
}

impl Observed {
    /// Function code with the exception bit masked off.
    pub fn base_func(&self) -> u8 {
        self.func & 0x7f
    }

    pub fn is_exception(&self) -> bool {
        self.func & 0x80 != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteSource {
    /// `# note [secs.ms] <text>` echoed by the device.
    Device,
    /// `NOTE <text>` written by the daemon because the device was unreachable.
    Daemon,
}

impl NoteSource {
    pub fn label(self) -> &'static str {
        match self {
            NoteSource::Device => "device",
            NoteSource::Daemon => "daemon",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub time: Time,
    pub text: String,
    pub source: NoteSource,
}

/// Everything one or more capture logs told us.
#[derive(Debug, Default)]
pub struct Capture {
    /// CRC-OK frames, time-ordered.
    pub frames: Vec<Observed>,
    /// Frames whose CRC did not check out; counted, never decoded.
    pub bad_crc: Vec<Observed>,
    pub short: Vec<(Time, Vec<u8>)>,
    /// `[DROPPED n frames|lines]` markers.
    pub dropped: Vec<(Time, u64, String)>,
    pub notes: Vec<Note>,
    /// Bus settings, from `# bus` lines and from each hello.
    pub bus: Vec<(Time, u32, String)>,
    pub reboots: Vec<Time>,
    /// `CAPTURE <text>` daemon events.
    pub events: Vec<(Time, String)>,
    pub first: Option<Time>,
    pub last: Option<Time>,
    pub lines: u64,
    pub unparsed: u64,
    pub files: Vec<String>,
}

impl Capture {
    pub fn ingest(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        self.lines += 1;
        let Some(rec) = logfile::parse_log_line(line) else {
            self.unparsed += 1;
            return;
        };
        let t = rec.time;
        self.first = Some(self.first.map_or(t, |f| f.min(t)));
        self.last = Some(self.last.map_or(t, |l| l.max(t)));

        if let Some(text) = rec.rest.strip_prefix("CAPTURE ") {
            if text.starts_with("device rebooted") {
                self.reboots.push(t);
            }
            self.events.push((t, text.to_string()));
            return;
        }
        if let Some(text) = rec.rest.strip_prefix("NOTE ") {
            self.notes.push(Note {
                time: t,
                text: text.to_string(),
                source: NoteSource::Daemon,
            });
            return;
        }

        match device::parse_line(&rec.rest) {
            DeviceLine::Frame(f) => {
                let o = Observed {
                    time: t,
                    slave: f.slave,
                    func: f.func,
                    body: f.body,
                    crc_ok: f.crc_ok,
                };
                if o.crc_ok {
                    self.frames.push(o);
                } else {
                    self.bad_crc.push(o);
                }
            }
            DeviceLine::Short(bytes) => self.short.push((t, bytes)),
            DeviceLine::Dropped { count, unit } => self.dropped.push((t, count, unit)),
            DeviceLine::Hello(h) => {
                if let (Some(baud), Some(fmt)) = (h.baud, h.format) {
                    self.bus.push((t, baud, fmt));
                }
            }
            DeviceLine::Bus { baud, format } => self.bus.push((t, baud, format)),
            DeviceLine::Note(text) => self.notes.push(Note {
                time: t,
                text,
                source: NoteSource::Device,
            }),
            DeviceLine::Status(_) => {}
            DeviceLine::Unknown(_) => self.unparsed += 1,
        }
    }

    /// Sort everything by time, so several log files can be fed in any order.
    pub fn finish(&mut self) {
        self.frames.sort_by_key(|f| f.time);
        self.bad_crc.sort_by_key(|f| f.time);
        self.short.sort_by_key(|(t, _)| *t);
        self.dropped.sort_by_key(|(t, _, _)| *t);
        self.notes.sort_by_key(|n| n.time);
        self.bus.sort_by_key(|(t, _, _)| *t);
        self.reboots.sort();
        self.events.sort_by_key(|(t, _)| *t);
    }

    /// Times of everything that came off the bus, for gap detection.
    pub fn bus_activity(&self) -> Vec<Time> {
        let mut v: Vec<Time> = self
            .frames
            .iter()
            .chain(self.bad_crc.iter())
            .map(|f| f.time)
            .chain(self.short.iter().map(|(t, _)| *t))
            .collect();
        v.sort();
        v
    }
}

/// Read and parse a set of capture-log files.
pub fn parse_files(paths: &[impl AsRef<Path>]) -> Result<Capture> {
    let mut cap = Capture::default();
    for p in paths {
        let p = p.as_ref();
        let body = std::fs::read_to_string(p)
            .with_context(|| format!("reading capture log {}", p.display()))?;
        cap.files.push(p.display().to_string());
        for line in body.lines() {
            cap.ingest(line);
        }
    }
    cap.finish();
    Ok(cap)
}

/// Parse from in-memory lines (used by the unit tests).
pub fn parse_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Capture {
    let mut cap = Capture::default();
    for line in lines {
        cap.ingest(line);
    }
    cap.finish();
    cap
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &[&str] = &[
        "2026-10-03T14:22:00.000+02:00 CAPTURE started, device modbus-sniffer:4000",
        "2026-10-03T14:22:00.100+02:00 # hello sniffer-esp32c6 0.1.0 uptime_ms=1000 bus=9600 8E1 replay=0",
        "2026-10-03T14:22:01.000+02:00 [1.000] RX slave=0x01 func=0x03 (Read Holding Registers) data=00 00 00 01 crc=0x0000 computed=0x0000 OK",
        "2026-10-03T14:22:01.020+02:00 [1.020] RX slave=0x01 func=0x03 (Read Holding Registers) data=02 00 2a crc=0x0000 computed=0x0000 OK",
        "2026-10-03T14:22:01.500+02:00 [1.500] RX slave=0x01 func=0x03 (Read Holding Registers) data=02 00 2b crc=0x1111 computed=0x2222 BAD_CRC",
        "2026-10-03T14:22:02.000+02:00 [2.000] SHORT (3 bytes): aa bb cc ",
        "2026-10-03T14:22:02.100+02:00 [DROPPED 4 frames]",
        "2026-10-03T14:22:03.000+02:00 # note [3.000] setpoint 26 -> 27",
        "2026-10-03T14:22:04.000+02:00 NOTE device was offline",
        "2026-10-03T14:22:05.000+02:00 CAPTURE device rebooted (uptime_ms 5000 -> 100)",
        "2026-10-03T14:22:06.000+02:00 # bus 19200 8N1",
        "2026-10-03T14:22:07.000+02:00 # ok note",
        "",
    ];

    #[test]
    fn classifies_every_line_kind() {
        let cap = parse_lines(LOG.iter().copied());
        assert_eq!(cap.frames.len(), 2);
        assert_eq!(cap.bad_crc.len(), 1);
        assert_eq!(cap.short.len(), 1);
        assert_eq!(
            cap.dropped,
            vec![(cap.dropped[0].0, 4, "frames".to_string())]
        );
        assert_eq!(cap.notes.len(), 2);
        assert_eq!(cap.notes[0].source, NoteSource::Device);
        assert_eq!(cap.notes[0].text, "setpoint 26 -> 27");
        assert_eq!(cap.notes[1].source, NoteSource::Daemon);
        assert_eq!(cap.reboots.len(), 1);
        // 9600 8E1 from the hello, 19200 8N1 from the `# bus` line.
        assert_eq!(cap.bus.len(), 2);
        assert_eq!(cap.bus[0].1, 9_600);
        assert_eq!(cap.bus[1].1, 19_200);
        assert_eq!(cap.unparsed, 0);
    }

    #[test]
    fn time_span_covers_the_log() {
        let cap = parse_lines(LOG.iter().copied());
        assert_eq!(
            cap.first.expect("first").to_rfc3339(),
            "2026-10-03T14:22:00+02:00"
        );
        assert_eq!(
            cap.last.expect("last").to_rfc3339(),
            "2026-10-03T14:22:07+02:00"
        );
    }

    #[test]
    fn frames_are_sorted_across_files() {
        let cap = parse_lines([
            "2026-10-03T14:22:09.000+02:00 [9.000] RX slave=0x01 func=0x03 (x) data=02 00 01 crc=0x0 computed=0x0 OK",
            "2026-10-03T14:22:08.000+02:00 [8.000] RX slave=0x01 func=0x03 (x) data=00 00 00 01 crc=0x0 computed=0x0 OK",
        ]);
        assert_eq!(cap.frames[0].body, vec![0x00, 0x00, 0x00, 0x01]);
        assert_eq!(cap.frames[1].body, vec![0x02, 0x00, 0x01]);
    }

    #[test]
    fn junk_is_counted_not_fatal() {
        let cap = parse_lines([
            "this is not a capture line",
            "2026-10-03T14:22:08.000+02:00 ?????",
        ]);
        assert_eq!(cap.unparsed, 2);
        assert!(cap.frames.is_empty());
    }
}
