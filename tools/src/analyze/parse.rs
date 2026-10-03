//! Turning capture-log files back into frames, notes and events.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset};

use super::regs::ReadKey;
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

/// A `COMPACT poll ...` liveness counter from a compacted log: `n` polls of
/// this key since the previous counter line, `suppressed` of them dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactPoll {
    pub time: Time,
    pub key: ReadKey,
    pub n: u64,
    pub suppressed: u64,
}

/// A `COMPACT gap ...` marker: this key stopped being polled for `len_ms` and
/// resumed at `time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactGap {
    pub time: Time,
    pub key: ReadKey,
    pub len_ms: i64,
}

/// Totals per key, reconstructed from the `COMPACT poll` counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactCounters {
    /// Polls the compactor saw, including the ones it dropped.
    pub polls: u64,
    pub suppressed: u64,
    pub first: Option<Time>,
    pub last: Option<Time>,
}

/// Everything the `COMPACT ...` bookkeeping lines told us.
#[derive(Debug, Default)]
pub struct CompactInfo {
    /// True as soon as one `COMPACT` line has been seen.
    pub seen: bool,
    /// `COMPACT begin <settings>` lines, one per compacted file or daemon run.
    pub begins: Vec<(Time, String)>,
    pub snapshots: Vec<Time>,
    pub polls: Vec<CompactPoll>,
    pub gaps: Vec<CompactGap>,
    /// `COMPACT` lines of a kind this build does not know.
    pub unknown: u64,
}

impl CompactInfo {
    /// Sum the per-key counters.
    pub fn counters(&self) -> BTreeMap<ReadKey, CompactCounters> {
        let mut out: BTreeMap<ReadKey, CompactCounters> = BTreeMap::new();
        for p in &self.polls {
            let c = out.entry(p.key).or_default();
            c.polls += p.n;
            c.suppressed += p.suppressed;
            c.first = Some(c.first.map_or(p.time, |f| f.min(p.time)));
            c.last = Some(c.last.map_or(p.time, |l| l.max(p.time)));
        }
        out
    }
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
    /// What the `COMPACT ...` lines of a compacted log said.
    pub compact: CompactInfo,
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
        if let Some(text) = rec.rest.strip_prefix("COMPACT ") {
            self.ingest_compact(t, text);
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

    /// One `COMPACT <kind> ...` line from a compacted log.
    ///
    /// These are the compactor's bookkeeping, not bus traffic: they say how many
    /// polls were folded away, when the state was re-stated in full, and when a
    /// polled block went quiet. Never counted as unparsed.
    fn ingest_compact(&mut self, t: Time, text: &str) {
        self.compact.seen = true;
        let (kind, rest) = text.split_once(' ').unwrap_or((text, ""));
        match kind {
            "begin" => self.compact.begins.push((t, rest.to_string())),
            "snapshot" => self.compact.snapshots.push(t),
            "poll" => {
                let f = fields(rest);
                if let (Some(key), Some(n)) = (compact_key(&f), f.get("n")) {
                    self.compact.polls.push(CompactPoll {
                        time: t,
                        key,
                        n: n.parse().unwrap_or(0),
                        suppressed: f
                            .get("suppressed")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0),
                    });
                } else {
                    self.compact.unknown += 1;
                }
            }
            "gap" => {
                let f = fields(rest);
                if let Some(key) = compact_key(&f) {
                    self.compact.gaps.push(CompactGap {
                        time: t,
                        key,
                        len_ms: f.get("len_ms").and_then(|v| v.parse().ok()).unwrap_or(0),
                    });
                } else {
                    self.compact.unknown += 1;
                }
            }
            _ => self.compact.unknown += 1,
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
        self.compact.begins.sort_by_key(|(t, _)| *t);
        self.compact.snapshots.sort();
        self.compact.polls.sort_by_key(|p| p.time);
        self.compact.gaps.sort_by_key(|g| g.time);
    }

    /// Times of everything that came off the bus, for gap detection.
    ///
    /// In a compacted log most of the traffic is not there any more, so the
    /// compactor's own liveness lines count as activity: each one is proof that
    /// the bus was being polled at that moment.
    pub fn bus_activity(&self) -> Vec<Time> {
        let mut v: Vec<Time> = self
            .frames
            .iter()
            .chain(self.bad_crc.iter())
            .map(|f| f.time)
            .chain(self.short.iter().map(|(t, _)| *t))
            .collect();
        if self.compact.seen {
            v.extend(self.compact.polls.iter().map(|p| p.time));
            v.extend(self.compact.snapshots.iter().copied());
        }
        v.sort();
        v
    }
}

/// Split `k=v k=v ...` into a map. Values never contain spaces in any line we
/// generate, so this is all the parsing a `COMPACT` line needs.
fn fields(rest: &str) -> BTreeMap<&str, &str> {
    rest.split_whitespace()
        .filter_map(|t| t.split_once('='))
        .collect()
}

fn compact_key(f: &BTreeMap<&str, &str>) -> Option<ReadKey> {
    Some(ReadKey {
        slave: u8::try_from(num(f.get("slave")?)?).ok()?,
        func: u8::try_from(num(f.get("func")?)?).ok()?,
        start: num(f.get("start")?)?,
        count: num(f.get("count")?)?,
    })
}

/// `0x1b` or `27`.
fn num(s: &str) -> Option<u16> {
    match s.strip_prefix("0x") {
        Some(hex) => u16::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
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

    const COMPACT_LOG: &[&str] = &[
        "2026-10-03T14:51:12.000+02:00 COMPACT begin deadband=0x001b:1 snapshot=300s gap=10s window=1s",
        "2026-10-03T14:51:12.137+02:00 [454.413] RX slave=0x01 func=0x03 (r) data=00 00 00 02 crc=0x0 computed=0x0 OK",
        "2026-10-03T14:51:12.289+02:00 [454.565] RX slave=0x01 func=0x03 (r) data=04 00 dc 00 05 crc=0x0 computed=0x0 OK",
        "2026-10-03T14:51:12.289+02:00 COMPACT poll slave=0x01 func=0x03 start=0x0000 count=2 n=1 suppressed=0",
        "2026-10-03T14:56:12.289+02:00 COMPACT snapshot keys=1",
        "2026-10-03T14:56:12.289+02:00 COMPACT poll slave=0x01 func=0x03 start=0x0000 count=2 n=300 suppressed=299",
        "2026-10-03T15:01:12.289+02:00 COMPACT gap slave=0x01 func=0x03 start=0x0000 count=2 len_ms=42500",
        "2026-10-03T15:01:12.289+02:00 COMPACT something-from-the-future x=1",
    ];

    #[test]
    fn compact_lines_are_understood_and_never_unparsed() {
        let cap = parse_lines(COMPACT_LOG.iter().copied());
        assert_eq!(cap.unparsed, 0);
        assert!(cap.compact.seen);
        assert_eq!(cap.compact.begins.len(), 1);
        assert!(cap.compact.begins[0].1.contains("deadband=0x001b:1"));
        assert_eq!(cap.compact.snapshots.len(), 1);
        assert_eq!(cap.compact.unknown, 1);

        let key = ReadKey {
            slave: 0x01,
            func: 0x03,
            start: 0x0000,
            count: 2,
        };
        assert_eq!(cap.compact.gaps.len(), 1);
        assert_eq!(cap.compact.gaps[0].key, key);
        assert_eq!(cap.compact.gaps[0].len_ms, 42_500);

        let counters = cap.compact.counters();
        assert_eq!(counters[&key].polls, 301);
        assert_eq!(counters[&key].suppressed, 299);
        // The two written frames are still ordinary frames.
        assert_eq!(cap.frames.len(), 2);
    }

    #[test]
    fn compact_liveness_lines_count_as_bus_activity() {
        let cap = parse_lines(COMPACT_LOG.iter().copied());
        let activity = cap.bus_activity();
        // Two frames plus three COMPACT poll/snapshot stamps.
        assert_eq!(activity.len(), 5);
        assert!(activity.windows(2).all(|w| w[0] <= w[1]), "sorted");
    }

    #[test]
    fn a_raw_log_is_not_flagged_as_compact() {
        let cap = parse_lines(LOG.iter().copied());
        assert!(!cap.compact.seen);
        assert!(cap.compact.counters().is_empty());
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
