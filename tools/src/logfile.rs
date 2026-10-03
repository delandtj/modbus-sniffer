//! The on-disk capture log: one file per local day, one line per device line.
//!
//! Format: `<RFC3339 local time, milliseconds> <original line>`, e.g.
//!
//! ```text
//! 2026-10-03T14:22:01.123+02:00 [12.345] RX slave=0x01 func=0x03 ...
//! 2026-10-03T14:22:05.000+02:00 CAPTURE connected to modbus-sniffer:4000
//! 2026-10-03T14:22:09.500+02:00 NOTE setpoint 26 -> 27
//! ```
//!
//! The wall-clock stamp of a frame line is its device uptime mapped through the
//! offset learned from the hello, so it survives reconnects and reboots. The
//! *file* a line lands in is chosen by arrival time, which can differ by at most
//! the replay buffer depth right after a reconnect; that is deliberate, because
//! it keeps the writer from having to hold several files open.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, Local, NaiveDate, SecondsFormat};

use crate::clock::Clock;

/// File-name prefix of the raw daily logs.
pub const RAW_PREFIX: &str = "capture";
/// File-name prefix of the compacted daily logs.
pub const COMPACT_PREFIX: &str = "compact";

/// Stamp format used for every line in the log.
pub fn format_stamp(t: DateTime<Local>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, false)
}

/// The same stamp format for a time read back out of a log.
pub fn format_time(t: DateTime<FixedOffset>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, false)
}

/// File name for a given local day.
pub fn file_name(day: NaiveDate) -> String {
    file_name_with_prefix(RAW_PREFIX, day)
}

/// File name for a given local day and file-name prefix.
pub fn file_name_with_prefix(prefix: &str, day: NaiveDate) -> String {
    format!("{prefix}-{}.log", day.format("%Y-%m-%d"))
}

/// Appends records to the current day's file, rotating at local midnight.
pub struct LogWriter<C: Clock> {
    dir: PathBuf,
    clock: C,
    prefix: String,
    open: Option<(NaiveDate, BufWriter<File>)>,
}

impl<C: Clock> LogWriter<C> {
    pub fn new(dir: impl Into<PathBuf>, clock: C) -> Self {
        Self {
            dir: dir.into(),
            clock,
            prefix: RAW_PREFIX.to_string(),
            open: None,
        }
    }

    /// Write `<prefix>-YYYY-MM-DD.log` instead of `capture-YYYY-MM-DD.log`.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write one record: `<stamp> <text>`. `text` must not contain a newline;
    /// any trailing CR/LF from the device is stripped.
    pub fn write(&mut self, stamp: DateTime<Local>, text: &str) -> std::io::Result<()> {
        let text = text.trim_end_matches(['\r', '\n']);
        self.write_line(&format!("{} {}", format_stamp(stamp), text))
    }

    /// Write one already-stamped line verbatim. The file it lands in is still
    /// chosen by the clock, so rotation is identical to [`Self::write`].
    pub fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let today = self.clock.now().date_naive();
        let rotate = match &self.open {
            Some((day, _)) => *day != today,
            None => true,
        };
        if rotate {
            if let Some((_, mut w)) = self.open.take() {
                let _ = w.flush();
            }
            std::fs::create_dir_all(&self.dir)?;
            let path = self.dir.join(file_name_with_prefix(&self.prefix, today));
            let f = OpenOptions::new().create(true).append(true).open(path)?;
            self.open = Some((today, BufWriter::new(f)));
        }

        let (_, w) = self.open.as_mut().expect("just opened");
        writeln!(w, "{}", line.trim_end_matches(['\r', '\n']))?;
        // Flush per line: a multi-day capture that loses its tail to a crash is
        // worse than the cost of one write syscall per frame at 9600 baud.
        w.flush()
    }
}

/// One parsed capture-log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub time: DateTime<FixedOffset>,
    /// Everything after the stamp: a device line, `CAPTURE <text>` or
    /// `NOTE <text>`.
    pub rest: String,
}

/// Parse `<RFC3339> <rest>`. Returns `None` for blank or malformed lines.
pub fn parse_log_line(line: &str) -> Option<LogRecord> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return None;
    }
    let (stamp, rest) = line.split_once(' ')?;
    let time = DateTime::parse_from_rfc3339(stamp).ok()?;
    Some(LogRecord {
        time,
        rest: rest.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use chrono::TimeZone;

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(y, m, d, h, min, s)
            .single()
            .expect("unambiguous local time")
    }

    #[test]
    fn stamp_has_milliseconds_and_offset() {
        let t = local(2026, 10, 3, 14, 22, 1) + chrono::TimeDelta::milliseconds(123);
        let s = format_stamp(t);
        assert!(s.starts_with("2026-10-03T14:22:01.123"), "{s}");
        // RFC3339 numeric offset, not `Z`-with-no-offset-info.
        assert!(s.contains('+') || s.contains('-'), "{s}");
    }

    #[test]
    fn rotates_at_local_midnight() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = TestClock::new(local(2026, 10, 3, 23, 59, 59));
        let mut w = LogWriter::new(dir.path(), clock.clone());

        w.write(clock.now(), "[1.000] RX a").expect("write");
        clock.set(local(2026, 10, 4, 0, 0, 1));
        w.write(clock.now(), "[2.000] RX b").expect("write");
        w.write(clock.now(), "[3.000] RX c").expect("write");

        let day1 = dir.path().join("capture-2026-10-03.log");
        let day2 = dir.path().join("capture-2026-10-04.log");
        assert_eq!(
            std::fs::read_to_string(&day1)
                .expect("day 1")
                .lines()
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(&day2)
                .expect("day 2")
                .lines()
                .count(),
            2
        );
    }

    #[test]
    fn appends_to_an_existing_day_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = TestClock::new(local(2026, 10, 3, 10, 0, 0));
        {
            let mut w = LogWriter::new(dir.path(), clock.clone());
            w.write(clock.now(), "first").expect("write");
        }
        {
            let mut w = LogWriter::new(dir.path(), clock.clone());
            w.write(clock.now(), "second").expect("write");
        }
        let body =
            std::fs::read_to_string(dir.path().join("capture-2026-10-03.log")).expect("read");
        assert_eq!(body.lines().count(), 2);
        assert!(body.contains("first"));
        assert!(body.contains("second"));
    }

    #[test]
    fn round_trips_a_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock = TestClock::new(local(2026, 10, 3, 14, 22, 1));
        let mut w = LogWriter::new(dir.path(), clock.clone());
        w.write(clock.now(), "[12.345] RX slave=0x01\r\n")
            .expect("write");

        let body =
            std::fs::read_to_string(dir.path().join("capture-2026-10-03.log")).expect("read");
        let rec = parse_log_line(body.lines().next().expect("a line")).expect("parse");
        assert_eq!(rec.rest, "[12.345] RX slave=0x01");
        assert_eq!(rec.time.timestamp(), clock.now().timestamp());
    }

    #[test]
    fn rejects_junk_lines() {
        assert!(parse_log_line("").is_none());
        assert!(parse_log_line("not-a-timestamp hello").is_none());
        assert!(parse_log_line("2026-10-03T14:22:01.123+02:00").is_none());
    }
}
