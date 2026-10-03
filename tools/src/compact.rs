//! Log compaction: the same log, with the repetitive poll traffic folded away.
//!
//! The heat pump controller re-reads the same two register blocks every second,
//! so a day of raw capture is ~346k lines / ~108 MiB of which almost everything
//! is a byte-identical repeat. This module turns a raw capture log into a
//! *compact* log in exactly the same line format - `<RFC3339 stamp> <text>` -
//! so every existing parser keeps working.
//!
//! What survives, always:
//!
//! - `CAPTURE ...` and `NOTE ...` daemon lines, and every `# ` device line
//!   (hello, bus, note, status, ok/err) - they are rare and each one is a fact
//!   about the capture itself.
//! - every frame that is not part of a recognised poll: writes (0x05, 0x06,
//!   0x0f, 0x10) and their acks, exceptions, other function codes, reads of
//!   other ranges, and read requests that never got an answer.
//! - `BAD_CRC` and `SHORT` lines, `[DROPPED n ...]` markers, and anything the
//!   log-line parser could not understand at all.
//!
//! A *recognised poll* is a read request (function 0x03 or 0x04, body exactly
//! `start(2) + count(2)`, CRC OK) followed within the pairing window by a
//! response on the same slave and function whose byte count is `2 * count` and
//! whose payload length matches. Its identity is the key
//! `(slave, func, start, count)`.
//!
//! A recognised poll is dropped when its response equals the last response
//! *written* for that key, where a register listed in the deadband map counts
//! as equal while `|new - last_written| <= deadband` (signed 16-bit). Comparing
//! against the last written value rather than the previous one is what makes
//! slow drift show up: the deviation accumulates until it crosses the deadband
//! and the pair is written. When a pair is written, BOTH the request line and
//! the response line go out, byte for byte as they came in.
//!
//! Three kinds of bookkeeping line keep the suppressed traffic visible. They
//! are all `COMPACT <kind> ...`, stamped like any other line:
//!
//! ```text
//! <ts> COMPACT begin deadband=0x001b:1,0x001e:5,0x0024:15 snapshot=300s gap=10s window=1s
//! <ts> COMPACT snapshot keys=2
//! <ts> COMPACT poll slave=0x01 func=0x03 start=0x0000 count=63 n=300 suppressed=299
//! <ts> COMPACT gap slave=0x01 func=0x03 start=0x0000 count=63 len_ms=42500
//! ```
//!
//! - `begin` is the first output line, and records the settings used.
//! - `snapshot` is written every snapshot interval and is followed by the
//!   latest request+response pair, in full and regardless of change, of every
//!   key that has been polled since the previous snapshot. So a reader can
//!   reconstruct the whole bus state from any point in the file without going
//!   back to the start. Two exceptions keep the file honest rather than merely
//!   regular: a key that has stopped being polled is not resurrected, and a
//!   pair that is already in the output (because it had just changed) is not
//!   repeated - a duplicated frame line would be counted twice by every
//!   reader.
//! - `poll` is the liveness counter: `n` polls of this key have happened since
//!   the previous `poll` line for it, of which `suppressed` were dropped. One
//!   is written whenever a key's pair is written and once per key per snapshot,
//!   so poll counts and periods survive compaction.
//! - `gap` is written when a key that had been polling resumes after more than
//!   the gap threshold of silence. The pair that follows is always written in
//!   full, because the value after a silence is never a safe "unchanged".

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset, TimeDelta};

use crate::analyze::regs::ReadKey;
use crate::device::{self, DeviceLine};
use crate::logfile;

/// Wall-clock time as stored in the log.
pub type Time = DateTime<FixedOffset>;

/// Default interval between full snapshots.
pub const DEFAULT_SNAPSHOT: Duration = Duration::from_secs(300);
/// Default silence per key that counts as a gap.
pub const DEFAULT_GAP: Duration = Duration::from_secs(10);
/// Default pairing window for request -> response.
pub const DEFAULT_MAX_LATENCY: Duration = Duration::from_secs(1);
/// A key must have been polled this often before a silence counts as a gap, so
/// a one-off read cannot produce gap lines.
const MIN_POLLS_FOR_GAP: u64 = 2;

/// Knobs for the compactor.
#[derive(Debug, Clone)]
pub struct CompactConfig {
    /// Register address -> allowed absolute deviation from the last written
    /// value. Addresses not in here must match exactly.
    pub deadband: BTreeMap<u16, u16>,
    /// Interval between full snapshots. Zero disables snapshots entirely.
    pub snapshot: Duration,
    /// A key with no poll for longer than this gets a `COMPACT gap` line when
    /// it resumes. Zero disables gap detection.
    pub gap: Duration,
    /// How long a read request stays open waiting for its response.
    pub max_latency: Duration,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            deadband: BTreeMap::new(),
            snapshot: DEFAULT_SNAPSHOT,
            gap: DEFAULT_GAP,
            max_latency: DEFAULT_MAX_LATENCY,
        }
    }
}

impl CompactConfig {
    /// The `deadband=` field of the `COMPACT begin` line.
    pub fn deadband_str(&self) -> String {
        if self.deadband.is_empty() {
            return "none".to_string();
        }
        self.deadband
            .iter()
            .map(|(addr, band)| format!("0x{addr:04x}:{band}"))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn begin_text(&self) -> String {
        format!(
            "COMPACT begin deadband={} snapshot={} gap={} window={}",
            self.deadband_str(),
            fmt_secs(self.snapshot),
            fmt_secs(self.gap),
            fmt_secs(self.max_latency),
        )
    }
}

fn fmt_secs(d: Duration) -> String {
    let ms = d.as_millis();
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

/// Parse one `--deadband` value: `0x001b:1,0x001e:5,36:15`.
///
/// Addresses and bands are hex with a `0x` prefix, decimal otherwise.
pub fn parse_deadbands(spec: &str) -> Result<Vec<(u16, u16)>> {
    let mut out = Vec::new();
    for item in spec.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (addr, band) = item
            .split_once(':')
            .with_context(|| format!("deadband {item:?} is not <register>:<band>"))?;
        out.push((
            parse_u16(addr).with_context(|| format!("deadband register {addr:?}"))?,
            parse_u16(band).with_context(|| format!("deadband value {band:?}"))?,
        ));
    }
    if out.is_empty() {
        bail!("deadband {spec:?} listed no registers");
    }
    Ok(out)
}

fn parse_u16(s: &str) -> Result<u16> {
    let s = s.trim();
    let v = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u16::from_str_radix(hex, 16)?,
        None => s.parse()?,
    };
    Ok(v)
}

/// What one input line turned out to be, for compaction purposes only.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// A read request that could start a poll.
    PollRequest(ReadKey),
    /// A read response, with its register values already decoded.
    ReadResponse { slave: u8, func: u8, regs: Vec<u16> },
    /// Anything else - always kept.
    Other,
}

fn classify(rest: &str) -> Kind {
    let DeviceLine::Frame(f) = device::parse_line(rest) else {
        return Kind::Other;
    };
    if !f.crc_ok || !matches!(f.func, 0x03 | 0x04) {
        return Kind::Other;
    }
    if f.body.len() == 4 {
        let start = u16::from_be_bytes([f.body[0], f.body[1]]);
        let count = u16::from_be_bytes([f.body[2], f.body[3]]);
        if (1..=125).contains(&count) {
            return Kind::PollRequest(ReadKey {
                slave: f.slave,
                func: f.func,
                start,
                count,
            });
        }
        return Kind::Other;
    }
    // A response is a byte count followed by that many bytes, and a register
    // payload is always an even number of bytes.
    if f.body.len() >= 3 && f.body[0] as usize == f.body.len() - 1 && f.body[0] % 2 == 0 {
        let regs = f.body[1..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes(*c))
            .collect();
        return Kind::ReadResponse {
            slave: f.slave,
            func: f.func,
            regs,
        };
    }
    Kind::Other
}

/// A read request waiting to see whether a response follows.
#[derive(Debug, Clone)]
struct Pending {
    key: ReadKey,
    time: Time,
    text: String,
}

/// Everything remembered about one poll key.
#[derive(Debug, Clone, Default)]
struct KeyState {
    /// The latest pair seen, written or not, verbatim.
    last_req: String,
    last_resp: String,
    /// Register values of the latest response seen.
    last_regs: Vec<u16>,
    /// Register values of the last response actually written out.
    written: Option<Vec<u16>>,
    /// Whether the latest pair above is already in the output. A snapshot does
    /// not repeat a pair it would only duplicate, because two copies of one
    /// frame line would be counted twice by every reader.
    last_emitted: bool,
    last_poll_time: Option<Time>,
    polls_total: u64,
    /// Polls since the last `COMPACT poll` line for this key.
    n: u64,
    /// How many of those `n` were dropped.
    suppressed: u64,
}

/// Counters for the run, for the `compact` subcommand's summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactStats {
    pub lines_in: u64,
    pub lines_out: u64,
    /// Recognised polls (request+response pairs).
    pub polls: u64,
    /// Of those, pairs dropped as unchanged.
    pub suppressed: u64,
    /// Read requests that got no matching response and were kept as-is.
    pub unmatched_requests: u64,
    pub snapshots: u64,
    pub gaps: u64,
    /// Lines the log-line parser could not understand; kept verbatim.
    pub unparsed: u64,
}

/// Streaming raw-log -> compact-log converter. Pure: every timestamp it writes
/// comes from the input lines, so it needs no clock of its own.
#[derive(Debug)]
pub struct Compactor {
    cfg: CompactConfig,
    state: BTreeMap<ReadKey, KeyState>,
    pending: Option<Pending>,
    last_snapshot: Option<Time>,
    /// Cleared on a new output file, so each file starts with `COMPACT begin`.
    announced: bool,
    stats: CompactStats,
}

impl Compactor {
    pub fn new(cfg: CompactConfig) -> Self {
        Self {
            cfg,
            state: BTreeMap::new(),
            pending: None,
            last_snapshot: None,
            announced: false,
            stats: CompactStats::default(),
        }
    }

    pub fn config(&self) -> &CompactConfig {
        &self.cfg
    }

    pub fn stats(&self) -> &CompactStats {
        &self.stats
    }

    /// Ask for a fresh `COMPACT begin` line before the next output line. The
    /// daemon calls this after a midnight rotation so every daily compact file
    /// says what settings produced it.
    pub fn restart_header(&mut self) {
        self.announced = false;
    }

    /// Feed one raw log line; append whatever it produces to `out`.
    pub fn push(&mut self, line: &str, out: &mut Vec<String>) {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            return;
        }
        self.stats.lines_in += 1;

        let Some(rec) = logfile::parse_log_line(line) else {
            // Not our format at all. Dropping it would be destroying evidence.
            self.stats.unparsed += 1;
            self.emit(out, line.to_string());
            return;
        };
        let t = rec.time;
        // The header goes out as soon as there is a timestamp to stamp it with,
        // so it can never sort after the first line of the file.
        if !self.announced {
            self.announced = true;
            let text = self.cfg.begin_text();
            out.push(format!("{} {text}", logfile::format_time(t)));
            self.stats.lines_out += 1;
        }
        let kind = classify(&rec.rest);

        // Resolve the held request first, so output stays in time order.
        let mut paired: Option<(Pending, Vec<u16>)> = None;
        if let Some(p) = self.pending.take() {
            let matches = match &kind {
                Kind::ReadResponse { slave, func, regs } => {
                    *slave == p.key.slave
                        && *func == p.key.func
                        && regs.len() == p.key.count as usize
                        && t >= p.time
                        && t - p.time <= self.max_latency()
                }
                _ => false,
            };
            if matches {
                let Kind::ReadResponse { regs, .. } = &kind else {
                    unreachable!("matches implies a read response")
                };
                paired = Some((p, regs.clone()));
            } else {
                self.stats.unmatched_requests += 1;
                self.emit(out, p.text);
            }
        }

        self.maybe_snapshot(t, out);

        if let Some((p, regs)) = paired {
            self.handle_poll(p, t, line.to_string(), regs, out);
            return;
        }

        match kind {
            Kind::PollRequest(key) => {
                self.pending = Some(Pending {
                    key,
                    time: t,
                    text: line.to_string(),
                })
            }
            _ => self.emit(out, line.to_string()),
        }
    }

    /// Flush the request still in hand and the counters that have not been
    /// reported yet. Call at end of input, so no suppressed poll goes unnamed.
    pub fn finish(&mut self, out: &mut Vec<String>) {
        if let Some(p) = self.pending.take() {
            self.stats.unmatched_requests += 1;
            self.emit(out, p.text);
        }
        let trailing: Vec<(ReadKey, Time, u64, u64)> = self
            .state
            .iter()
            .filter(|(_, s)| s.n > 0)
            .filter_map(|(k, s)| s.last_poll_time.map(|t| (*k, t, s.n, s.suppressed)))
            .collect();
        for (key, t, n, suppressed) in trailing {
            if let Some(s) = self.state.get_mut(&key) {
                s.n = 0;
                s.suppressed = 0;
            }
            self.emit_at(out, t, &poll_text(&key, n, suppressed));
        }
    }

    fn max_latency(&self) -> TimeDelta {
        TimeDelta::from_std(self.cfg.max_latency).unwrap_or(TimeDelta::seconds(1))
    }

    fn emit(&mut self, out: &mut Vec<String>, line: String) {
        out.push(line);
        self.stats.lines_out += 1;
    }

    fn emit_at(&mut self, out: &mut Vec<String>, t: Time, text: &str) {
        let line = format!("{} {text}", logfile::format_time(t));
        self.emit(out, line);
    }

    /// Restate every key that is still being polled, every `snapshot` interval.
    ///
    /// "Still being polled" means it was polled since the previous snapshot. A
    /// key that stopped hours ago (a one-off scan, a bench test) is left in the
    /// past instead of being resurrected into every snapshot for the rest of the
    /// day, which would duplicate its frame lines and inflate every count a
    /// reader takes from them.
    fn maybe_snapshot(&mut self, t: Time, out: &mut Vec<String>) {
        if self.cfg.snapshot.is_zero() {
            return;
        }
        let interval = TimeDelta::from_std(self.cfg.snapshot).unwrap_or(TimeDelta::seconds(300));
        let since = match self.last_snapshot {
            None => {
                self.last_snapshot = Some(t);
                return;
            }
            Some(prev) if t - prev < interval => return,
            Some(prev) => prev,
        };
        self.last_snapshot = Some(t);

        let keys: Vec<ReadKey> = self
            .state
            .iter()
            .filter(|(_, s)| !s.last_resp.is_empty())
            .filter(|(_, s)| s.last_poll_time.is_some_and(|last| last >= since))
            .map(|(k, _)| *k)
            .collect();
        if keys.is_empty() {
            return;
        }
        self.stats.snapshots += 1;
        self.emit_at(out, t, &format!("COMPACT snapshot keys={}", keys.len()));
        for key in keys {
            let (pair, n, suppressed) = {
                let s = self.state.get_mut(&key).expect("key just listed");
                s.written = Some(s.last_regs.clone());
                let pair = (!s.last_emitted).then(|| (s.last_req.clone(), s.last_resp.clone()));
                s.last_emitted = true;
                let taken = (pair, s.n, s.suppressed);
                s.n = 0;
                s.suppressed = 0;
                taken
            };
            if let Some((req, resp)) = pair {
                self.emit(out, req);
                self.emit(out, resp);
            }
            self.emit_at(out, t, &poll_text(&key, n, suppressed));
        }
    }

    fn handle_poll(
        &mut self,
        p: Pending,
        t: Time,
        resp_text: String,
        regs: Vec<u16>,
        out: &mut Vec<String>,
    ) {
        self.stats.polls += 1;
        let gap = TimeDelta::from_std(self.cfg.gap).unwrap_or(TimeDelta::seconds(10));
        let key = p.key;

        let deadband = &self.cfg.deadband;
        let st = self.state.entry(key).or_default();

        let gap_len = match st.last_poll_time {
            Some(prev)
                if !gap.is_zero() && st.polls_total >= MIN_POLLS_FOR_GAP && t - prev > gap =>
            {
                Some(t - prev)
            }
            _ => None,
        };

        st.polls_total += 1;
        st.n += 1;
        st.last_poll_time = Some(t);
        st.last_req = p.text.clone();
        st.last_resp = resp_text.clone();
        st.last_regs = regs.clone();

        let changed = match &st.written {
            None => true,
            Some(written) => differs(written, &regs, key.start, deadband),
        };
        let write = changed || gap_len.is_some();
        st.last_emitted = write;
        if write {
            st.written = Some(regs);
        } else {
            st.suppressed += 1;
        }
        let (n, suppressed) = (st.n, st.suppressed);
        if write {
            st.n = 0;
            st.suppressed = 0;
        }

        if let Some(len) = gap_len {
            self.stats.gaps += 1;
            let text = format!(
                "COMPACT gap slave=0x{:02x} func=0x{:02x} start=0x{:04x} count={} len_ms={}",
                key.slave,
                key.func,
                key.start,
                key.count,
                len.num_milliseconds()
            );
            self.emit_at(out, t, &text);
        }
        if write {
            self.emit(out, p.text);
            self.emit(out, resp_text);
            self.emit_at(out, t, &poll_text(&key, n, suppressed));
        } else {
            self.stats.suppressed += 1;
        }
    }
}

fn poll_text(key: &ReadKey, n: u64, suppressed: u64) -> String {
    format!(
        "COMPACT poll slave=0x{:02x} func=0x{:02x} start=0x{:04x} count={} n={n} suppressed={suppressed}",
        key.slave, key.func, key.start, key.count
    )
}

/// Does `new` differ from `written` by more than the per-register deadband?
///
/// A different length is always a difference. The comparison is signed 16-bit,
/// which is what every value on this bus turns out to be.
fn differs(written: &[u16], new: &[u16], start: u16, deadband: &BTreeMap<u16, u16>) -> bool {
    if written.len() != new.len() {
        return true;
    }
    for (i, (old, now)) in written.iter().zip(new.iter()).enumerate() {
        if old == now {
            continue;
        }
        let addr = start.wrapping_add(i as u16);
        let band = deadband.get(&addr).copied().unwrap_or(0);
        if band == 0 {
            return true;
        }
        let delta = i32::from(*now as i16) - i32::from(*old as i16);
        if delta.unsigned_abs() > u32::from(band) {
            return true;
        }
    }
    false
}

/// Compact a whole log in memory. Convenience for tests and the offline tool.
pub fn compact_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    cfg: CompactConfig,
) -> (Vec<String>, CompactStats) {
    let mut c = Compactor::new(cfg);
    let mut out = Vec::new();
    for line in lines {
        c.push(line, &mut out);
    }
    c.finish(&mut out);
    (out, c.stats().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one request line for `start`/`count`, and one response line with
    /// the given register values. The CRC fields are not checked by the
    /// compactor beyond the trailing `OK`, so they are placeholders.
    fn req(secs: f64, start: u16, count: u16) -> String {
        format!(
            "{} [{secs:.3}] RX slave=0x01 func=0x03 (Read Holding Registers) data={:02x} {:02x} {:02x} {:02x} crc=0x0 computed=0x0 OK",
            stamp(secs),
            start >> 8,
            start & 0xff,
            count >> 8,
            count & 0xff
        )
    }

    fn resp(secs: f64, values: &[u16]) -> String {
        let mut hex = format!("{:02x}", values.len() * 2);
        for v in values {
            hex.push_str(&format!(" {:02x} {:02x}", v >> 8, v & 0xff));
        }
        format!(
            "{} [{secs:.3}] RX slave=0x01 func=0x03 (Read Holding Registers) data={hex} crc=0x0 computed=0x0 OK",
            stamp(secs)
        )
    }

    /// `2026-10-03T10:00:00+02:00` plus `secs`.
    fn stamp(secs: f64) -> String {
        let base = DateTime::parse_from_rfc3339("2026-10-03T10:00:00.000+02:00").expect("base");
        let t = base + TimeDelta::milliseconds((secs * 1000.0).round() as i64);
        logfile::format_time(t)
    }

    fn cfg() -> CompactConfig {
        CompactConfig {
            snapshot: Duration::ZERO,
            gap: Duration::ZERO,
            ..CompactConfig::default()
        }
    }

    fn run(lines: &[String], cfg: CompactConfig) -> (Vec<String>, CompactStats) {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        compact_lines(refs, cfg)
    }

    /// One poll per second, always the same answer.
    fn steady(polls: usize, values: &[u16]) -> Vec<String> {
        let mut v = Vec::new();
        for i in 0..polls {
            let at = i as f64;
            v.push(req(at, 0x0000, values.len() as u16));
            v.push(resp(at + 0.15, values));
        }
        v
    }

    #[test]
    fn identical_polls_collapse_to_one_pair() {
        let (out, stats) = run(&steady(10, &[1, 2, 3]), cfg());
        // begin + request + response + poll counter + the closing counter.
        assert_eq!(out.len(), 5, "{out:#?}");
        assert!(
            out[4].contains(
                "COMPACT poll slave=0x01 func=0x03 start=0x0000 count=3 n=9 suppressed=9"
            ),
            "the tail of suppressed polls is reported at end of input: {out:#?}"
        );
        assert!(out[0].contains("COMPACT begin"), "{out:#?}");
        assert!(out[1].contains("data=00 00 00 03"), "{out:#?}");
        assert!(out[2].contains("data=06 00 01 00 02 00 03"), "{out:#?}");
        assert!(
            out[3].contains(
                "COMPACT poll slave=0x01 func=0x03 start=0x0000 count=3 n=1 suppressed=0"
            ),
            "{out:#?}"
        );
        assert_eq!(stats.polls, 10);
        assert_eq!(stats.suppressed, 9);
    }

    #[test]
    fn a_change_writes_request_and_response_together() {
        let mut lines = steady(3, &[1, 2, 3]);
        lines.push(req(3.0, 0x0000, 3));
        lines.push(resp(3.15, &[1, 2, 4]));
        let (out, _) = run(&lines, cfg());
        let frames: Vec<&String> = out.iter().filter(|l| l.contains(" RX ")).collect();
        assert_eq!(frames.len(), 4, "both pairs, both lines each: {out:#?}");
        assert!(frames[2].contains("data=00 00 00 03"), "{out:#?}");
        assert!(frames[3].contains("00 04"), "{out:#?}");
        // The second counter covers the three polls since the first one.
        let polls: Vec<&String> = out.iter().filter(|l| l.contains("COMPACT poll")).collect();
        assert!(polls[1].contains("n=3 suppressed=2"), "{out:#?}");
    }

    #[test]
    fn a_deadband_swallows_jitter() {
        let mut c = cfg();
        c.deadband.insert(0x0001, 1);
        let mut lines = Vec::new();
        // Register 0x0001 wobbles by +-1 around 100; nothing else moves.
        for (i, v) in [100u16, 101, 99, 100, 101].iter().enumerate() {
            lines.push(req(i as f64, 0x0000, 2));
            lines.push(resp(i as f64 + 0.15, &[7, *v]));
        }
        let (out, stats) = run(&lines, c);
        assert_eq!(stats.suppressed, 4, "{out:#?}");
        assert_eq!(out.iter().filter(|l| l.contains(" RX ")).count(), 2);
    }

    #[test]
    fn drift_past_the_deadband_is_written_even_in_small_steps() {
        let mut c = cfg();
        c.deadband.insert(0x0000, 2);
        let mut lines = Vec::new();
        // 100 -> 101 -> 102 -> 103: each step is inside the deadband, but the
        // third sample is 3 away from the last WRITTEN value of 100.
        for (i, v) in [100u16, 101, 102, 103].iter().enumerate() {
            lines.push(req(i as f64, 0x0000, 1));
            lines.push(resp(i as f64 + 0.15, &[*v]));
        }
        let (out, stats) = run(&lines, c);
        assert_eq!(stats.polls, 4);
        assert_eq!(stats.suppressed, 2, "101 and 102 only: {out:#?}");
        let written: Vec<&String> = out.iter().filter(|l| l.contains("data=02 00")).collect();
        assert_eq!(written.len(), 2, "{out:#?}");
        assert!(written[0].contains("data=02 00 64"), "100: {out:#?}");
        assert!(written[1].contains("data=02 00 67"), "103: {out:#?}");
    }

    #[test]
    fn a_signed_deadband_straddles_zero() {
        let mut c = cfg();
        c.deadband.insert(0x0000, 5);
        let mut lines = Vec::new();
        // -1 (0xffff) -> +2: a delta of 3, inside the band, despite the u16
        // values being 65535 and 2.
        for (i, v) in [0xffffu16, 0x0002].iter().enumerate() {
            lines.push(req(i as f64, 0x0000, 1));
            lines.push(resp(i as f64 + 0.15, &[*v]));
        }
        let (_, stats) = run(&lines, c);
        assert_eq!(stats.suppressed, 1);
    }

    #[test]
    fn snapshots_rewrite_everything_on_cadence() {
        let c = CompactConfig {
            snapshot: Duration::from_secs(5),
            gap: Duration::ZERO,
            ..CompactConfig::default()
        };
        let (out, stats) = run(&steady(12, &[1, 2]), c);
        // Polls run t=0..11 s; the first line sets the baseline, so the
        // snapshots fall at t=5 and t=10.
        assert_eq!(stats.snapshots, 2, "{out:#?}");
        assert_eq!(
            out.iter()
                .filter(|l| l.contains("COMPACT snapshot"))
                .count(),
            2
        );
        // Each snapshot re-writes the pair in full.
        assert_eq!(
            out.iter().filter(|l| l.contains(" RX ")).count(),
            6,
            "{out:#?}"
        );
        // n/suppressed cover the interval since the previous counter line.
        let polls: Vec<&String> = out.iter().filter(|l| l.contains("COMPACT poll")).collect();
        // One for the first pair, one per snapshot, one closing the input.
        assert_eq!(polls.len(), 4, "{out:#?}");
        assert!(polls[1].contains("n=4 suppressed=4"), "{out:#?}");
        assert!(polls[3].contains("n=2 suppressed=2"), "{out:#?}");
        // Every poll is accounted for exactly once across the counters.
        let counted: u64 = polls
            .iter()
            .map(|l| {
                l.split_whitespace()
                    .find_map(|t| t.strip_prefix("n="))
                    .and_then(|v| v.parse::<u64>().ok())
                    .expect("an n= field")
            })
            .sum();
        assert_eq!(counted, stats.polls, "{out:#?}");
    }

    #[test]
    fn a_snapshot_keeps_one_pair_per_key() {
        let c = CompactConfig {
            snapshot: Duration::from_secs(3),
            gap: Duration::ZERO,
            ..CompactConfig::default()
        };
        let mut lines = Vec::new();
        for i in 0..8 {
            let at = i as f64;
            lines.push(req(at, 0x0000, 1));
            lines.push(resp(at + 0.1, &[7]));
            lines.push(req(at + 0.3, 0x0010, 1));
            lines.push(resp(at + 0.4, &[9]));
        }
        let (out, stats) = run(&lines, c);
        assert_eq!(stats.snapshots, 2);
        for snap in out.iter().filter(|l| l.contains("COMPACT snapshot")) {
            assert!(snap.contains("keys=2"), "{snap}");
        }
        // Two keys x (2 frames + 1 counter) per snapshot, plus the two first
        // pairs and their counters.
        assert_eq!(
            out.iter().filter(|l| l.contains(" RX ")).count(),
            2 * 2 + 2 * 4
        );
    }

    #[test]
    fn a_key_that_stopped_polling_is_not_restated_forever() {
        let c = CompactConfig {
            snapshot: Duration::from_secs(5),
            gap: Duration::ZERO,
            ..CompactConfig::default()
        };
        let mut lines = Vec::new();
        // A one-off read at the very start...
        lines.push(req(0.0, 0x0100, 1));
        lines.push(resp(0.1, &[42]));
        // ... then 20 s of a different block being polled.
        for i in 0..20 {
            let at = i as f64;
            lines.push(req(at + 0.3, 0x0000, 1));
            lines.push(resp(at + 0.4, &[7]));
        }
        let (out, _) = run(&lines, c);
        assert_eq!(
            out.iter()
                .filter(|l| l.contains("data=01 00 00 01"))
                .count(),
            1,
            "the abandoned key should appear once, not in every snapshot: {out:#?}"
        );
        let snaps: Vec<&String> = out
            .iter()
            .filter(|l| l.contains("COMPACT snapshot"))
            .collect();
        assert_eq!(snaps.len(), 3, "{out:#?}");
        // Only the first snapshot still counts the one-off read as live.
        assert!(snaps[0].contains("keys=2"), "{out:#?}");
        assert!(snaps[1].contains("keys=1"), "{out:#?}");
        assert!(snaps[2].contains("keys=1"), "{out:#?}");
    }

    #[test]
    fn a_snapshot_does_not_duplicate_a_pair_it_just_wrote() {
        let c = CompactConfig {
            snapshot: Duration::from_secs(3),
            gap: Duration::ZERO,
            ..CompactConfig::default()
        };
        let mut lines = Vec::new();
        // The value changes on every poll, so every pair is written anyway.
        for i in 0..8u16 {
            let at = f64::from(i);
            lines.push(req(at, 0x0000, 1));
            lines.push(resp(at + 0.1, &[i]));
        }
        let (out, _) = run(&lines, c);
        let frames: Vec<&String> = out.iter().filter(|l| l.contains(" RX ")).collect();
        assert_eq!(frames.len(), 16, "8 pairs, no duplicates: {out:#?}");
        let mut sorted = frames.clone();
        sorted.sort();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "no line appears twice: {out:#?}");
    }

    #[test]
    fn a_gap_is_reported_when_polling_resumes() {
        let c = CompactConfig {
            snapshot: Duration::ZERO,
            gap: Duration::from_secs(10),
            ..CompactConfig::default()
        };
        let mut lines = steady(4, &[5]);
        // ... then nothing for 40 s, then the same value again.
        lines.push(req(43.0, 0x0000, 1));
        lines.push(resp(43.15, &[5]));
        let (out, stats) = run(&lines, c);
        assert_eq!(stats.gaps, 1, "{out:#?}");
        let gap = out
            .iter()
            .find(|l| l.contains("COMPACT gap"))
            .expect("a gap line");
        assert!(
            gap.contains("slave=0x01 func=0x03 start=0x0000 count=1 len_ms=40000"),
            "{gap}"
        );
        // The pair after a gap is written even though the value is unchanged.
        assert_eq!(
            out.iter().filter(|l| l.contains(" RX ")).count(),
            4,
            "{out:#?}"
        );
    }

    #[test]
    fn the_first_poll_of_a_key_is_not_a_gap() {
        let c = CompactConfig {
            snapshot: Duration::ZERO,
            gap: Duration::from_secs(1),
            ..CompactConfig::default()
        };
        let mut lines = vec![req(0.0, 0x0000, 1), resp(0.1, &[5])];
        lines.push(req(60.0, 0x0000, 1));
        lines.push(resp(60.1, &[5]));
        let (_, stats) = run(&lines, c);
        // Two polls, one interval: the first poll cannot be preceded by a gap,
        // and a key needs two polls before a silence counts.
        assert_eq!(stats.gaps, 0);
    }

    #[test]
    fn writes_notes_and_damaged_frames_are_always_kept() {
        let keep = [
            "2026-10-03T10:00:00.000+02:00 CAPTURE connected to modbus-sniffer:4000",
            "2026-10-03T10:00:00.100+02:00 # hello sniffer-esp32c6 0.1.0 uptime_ms=1000 bus=9600 8E1 replay=0",
            "2026-10-03T10:00:00.200+02:00 # status uptime_ms=1100 bus=9600 8E1 frames=7",
            "2026-10-03T10:00:00.300+02:00 # note [1.200] setpoint 26 -> 27",
            "2026-10-03T10:00:00.400+02:00 # ok note setpoint 26 -> 27",
            "2026-10-03T10:00:00.500+02:00 NOTE device was offline",
            "2026-10-03T10:00:00.600+02:00 [1.500] RX slave=0x01 func=0x10 (Write Multiple Registers) data=00 3f 00 02 04 00 01 00 02 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.700+02:00 [1.600] RX slave=0x01 func=0x10 (Write Multiple Registers) data=00 3f 00 02 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.800+02:00 [1.700] RX slave=0x01 func=0x06 (Write Single Register) data=00 00 00 e6 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.900+02:00 [1.800] RX slave=0x01 func=0x83 (Exception) exception=0x02 (Illegal Data Address) crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:01.000+02:00 [1.900] RX slave=0x01 func=0x03 (r) data=02 00 2b crc=0x1111 computed=0x2222 BAD_CRC",
            "2026-10-03T10:00:01.100+02:00 [2.000] SHORT (3 bytes): aa bb cc",
            "2026-10-03T10:00:01.200+02:00 [DROPPED 4 lines]",
            "2026-10-03T10:00:01.300+02:00 something the parser has never seen",
            "and a line with no stamp at all",
            // A read of another range, answered: a poll, so it may collapse -
            // but the first one is always written.
            "2026-10-03T10:00:01.400+02:00 [2.100] RX slave=0x02 func=0x04 (r) data=00 05 00 01 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:01.500+02:00 [2.200] RX slave=0x02 func=0x04 (r) data=02 00 07 crc=0x0 computed=0x0 OK",
        ];
        let (out, stats) = compact_lines(keep.iter().copied(), cfg());
        for line in &keep {
            assert!(
                out.iter().any(|o| o == line),
                "compaction lost a line that must be kept:\n{line}\ngot:\n{out:#?}"
            );
        }
        assert_eq!(stats.unparsed, 1, "only the unstamped line: {out:#?}");
    }

    #[test]
    fn an_unanswered_read_request_is_kept() {
        let lines = vec![
            req(0.0, 0x0000, 1),
            // No response; the next line is something else entirely.
            "2026-10-03T10:00:00.500+02:00 [0.500] SHORT (2 bytes): aa bb".to_string(),
            req(5.0, 0x0000, 1),
            resp(5.1, &[3]),
        ];
        let (out, stats) = run(&lines, cfg());
        assert_eq!(stats.unmatched_requests, 1);
        assert_eq!(
            out.iter().filter(|l| l.contains(" RX ")).count(),
            3,
            "{out:#?}"
        );
    }

    #[test]
    fn a_response_outside_the_pairing_window_does_not_pair() {
        let c = CompactConfig {
            max_latency: Duration::from_millis(100),
            ..cfg()
        };
        let lines = vec![req(0.0, 0x0000, 1), resp(0.5, &[3])];
        let (out, stats) = run(&lines, c);
        assert_eq!(stats.polls, 0);
        assert_eq!(stats.unmatched_requests, 1);
        assert_eq!(out.iter().filter(|l| l.contains(" RX ")).count(), 2);
    }

    #[test]
    fn two_keys_are_deduplicated_independently() {
        let mut lines = Vec::new();
        for i in 0..6 {
            let at = i as f64;
            lines.push(req(at, 0x0000, 1));
            lines.push(resp(at + 0.1, &[1]));
            lines.push(req(at + 0.3, 0x0040, 1));
            // The second block changes every poll.
            lines.push(resp(at + 0.4, &[100 + i as u16 * 10]));
        }
        let (out, stats) = run(&lines, cfg());
        assert_eq!(stats.polls, 12);
        assert_eq!(stats.suppressed, 5, "only the first key repeats: {out:#?}");
    }

    #[test]
    fn the_header_returns_after_a_rotation() {
        let mut c = Compactor::new(cfg());
        let mut out = Vec::new();
        for line in steady(2, &[1]) {
            c.push(&line, &mut out);
        }
        c.restart_header();
        for line in steady(1, &[9]) {
            c.push(&line, &mut out);
        }
        assert_eq!(
            out.iter().filter(|l| l.contains("COMPACT begin")).count(),
            2
        );
    }

    #[test]
    fn deadband_spec_parsing() {
        assert_eq!(
            parse_deadbands("0x001b:1,0x001e:5,0x0024:15").expect("parse"),
            vec![(0x001b, 1), (0x001e, 5), (0x0024, 15)]
        );
        assert_eq!(parse_deadbands("36:15").expect("parse"), vec![(36, 15)]);
        assert_eq!(
            parse_deadbands(" 0x10:2 , ").expect("parse"),
            vec![(0x10, 2)]
        );
        assert!(parse_deadbands("0x001b").is_err());
        assert!(parse_deadbands("").is_err());
        assert!(parse_deadbands("zz:1").is_err());
    }

    #[test]
    fn the_begin_line_records_the_settings() {
        let mut c = cfg();
        c.deadband.insert(0x001b, 1);
        c.deadband.insert(0x0024, 15);
        let (out, _) = run(&steady(1, &[1]), c);
        assert!(
            out[0].ends_with(
                "COMPACT begin deadband=0x001b:1,0x0024:15 snapshot=0s gap=0s window=1s"
            ),
            "{:?}",
            out[0]
        );
    }

    #[test]
    fn classification_of_the_real_poll_lines() {
        // The two blocks the heat pump controller actually reads.
        let settings = classify(
            "[454.413] RX slave=0x01 func=0x03 (Read Holding Registers) data=00 00 00 3f crc=0xda05 computed=0xda05 OK",
        );
        assert_eq!(
            settings,
            Kind::PollRequest(ReadKey {
                slave: 1,
                func: 3,
                start: 0,
                count: 0x3f
            })
        );
        let status = classify(
            "[549.625] RX slave=0x01 func=0x10 (Write Multiple Registers) data=00 3f 00 43 crc=0xf4b1 computed=0xf4b1 OK",
        );
        assert_eq!(status, Kind::Other, "a 0x10 ack is never a poll");
        let bad = classify(
            "[1.900] RX slave=0x01 func=0x03 (r) data=00 00 00 3f crc=0x1 computed=0x2 BAD_CRC",
        );
        assert_eq!(bad, Kind::Other, "a bad CRC is never a poll");
    }
}
