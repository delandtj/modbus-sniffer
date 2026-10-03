//! Decoding paired exchanges into a register map, a write log and a poll-loop
//! summary.

use std::collections::{BTreeMap, BTreeSet};

use chrono::TimeDelta;

use super::pair::Paired;
use super::parse::Time;

/// The four Modbus data tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RegKind {
    Coil,
    DiscreteInput,
    Holding,
    Input,
}

impl RegKind {
    pub fn label(self) -> &'static str {
        match self {
            RegKind::Coil => "coil",
            RegKind::DiscreteInput => "discrete input",
            RegKind::Holding => "holding register",
            RegKind::Input => "input register",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            RegKind::Coil => "Coils",
            RegKind::DiscreteInput => "Discrete inputs",
            RegKind::Holding => "Holding registers",
            RegKind::Input => "Input registers",
        }
    }

    /// Single-bit tables only ever hold 0 or 1.
    pub fn is_bit(self) -> bool {
        matches!(self, RegKind::Coil | RegKind::DiscreteInput)
    }

    fn for_read(func: u8) -> Option<Self> {
        match func {
            0x01 => Some(RegKind::Coil),
            0x02 => Some(RegKind::DiscreteInput),
            0x03 => Some(RegKind::Holding),
            0x04 => Some(RegKind::Input),
            _ => None,
        }
    }

    fn for_write(func: u8) -> Option<Self> {
        match func {
            0x05 | 0x0f => Some(RegKind::Coil),
            0x06 | 0x10 => Some(RegKind::Holding),
            _ => None,
        }
    }
}

/// One observed value transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub time: Time,
    pub from: u16,
    pub to: u16,
}

/// Everything we learned about one address.
#[derive(Debug, Clone)]
pub struct RegStats {
    pub first: u16,
    pub last: u16,
    pub min: u16,
    pub max: u16,
    pub samples: u64,
    pub distinct: BTreeSet<u16>,
    pub first_time: Time,
    pub last_time: Time,
    /// Every transition seen, counted whether or not it was retained below.
    pub transitions: u64,
    /// Transitions, kept only inside the windows passed to [`decode`] so a
    /// multi-day capture of a free-running counter cannot eat the heap.
    pub changes: Vec<Change>,
}

impl RegStats {
    pub fn is_constant(&self) -> bool {
        self.distinct.len() <= 1
    }

    /// Fraction of polls on which the value moved.
    pub fn change_rate(&self) -> f64 {
        if self.samples < 2 {
            return 0.0;
        }
        self.transitions as f64 / (self.samples - 1) as f64
    }

    /// A counter, uptime or live measurement: it moves on most polls, so
    /// "it changed near your note" carries no information about the note.
    pub fn is_free_running(&self) -> bool {
        self.samples >= 8 && self.change_rate() > 0.5
    }
}

/// Upper bound on retained transitions per address.
const MAX_CHANGES_PER_REG: usize = 4096;

#[derive(Debug, Default)]
pub struct RegisterMap {
    pub regs: BTreeMap<(RegKind, u16), RegStats>,
}

impl RegisterMap {
    fn observe(&mut self, kind: RegKind, addr: u16, value: u16, time: Time, keep_change: bool) {
        match self.regs.get_mut(&(kind, addr)) {
            Some(s) => {
                if s.last != value {
                    s.transitions += 1;
                    if keep_change && s.changes.len() < MAX_CHANGES_PER_REG {
                        s.changes.push(Change {
                            time,
                            from: s.last,
                            to: value,
                        });
                    }
                }
                s.last = value;
                s.min = s.min.min(value);
                s.max = s.max.max(value);
                s.samples += 1;
                s.distinct.insert(value);
                s.last_time = time;
            }
            None => {
                self.regs.insert(
                    (kind, addr),
                    RegStats {
                        first: value,
                        last: value,
                        min: value,
                        max: value,
                        samples: 1,
                        distinct: BTreeSet::from([value]),
                        first_time: time,
                        last_time: time,
                        transitions: 0,
                        changes: Vec::new(),
                    },
                );
            }
        }
    }

    pub fn by_kind(&self, kind: RegKind) -> impl Iterator<Item = (u16, &RegStats)> {
        self.regs
            .iter()
            .filter(move |((k, _), _)| *k == kind)
            .map(|((_, a), s)| (*a, s))
    }

    /// Every transition inside `[from, to]`, across all addresses.
    pub fn changes_between(&self, from: Time, to: Time) -> Vec<(RegKind, u16, &Change)> {
        let mut out: Vec<(RegKind, u16, &Change)> = Vec::new();
        for ((kind, addr), s) in &self.regs {
            for c in &s.changes {
                if c.time >= from && c.time <= to {
                    out.push((*kind, *addr, c));
                }
            }
        }
        out.sort_by_key(|(_, _, c)| c.time);
        out
    }

    pub fn is_free_running(&self, kind: RegKind, addr: u16) -> bool {
        self.regs
            .get(&(kind, addr))
            .is_some_and(RegStats::is_free_running)
    }
}

/// One register write seen on the bus.
#[derive(Debug, Clone)]
pub struct WriteOp {
    pub time: Time,
    pub slave: u8,
    pub func: u8,
    pub kind: RegKind,
    pub addr: u16,
    pub values: Vec<u16>,
    /// Whether a (non-exception) response followed.
    pub acked: bool,
    pub exception: Option<u8>,
}

/// Identity of a repeating read request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReadKey {
    pub slave: u8,
    pub func: u8,
    pub start: u16,
    pub count: u16,
}

#[derive(Debug, Default, Clone)]
pub struct ReadStats {
    pub times: Vec<Time>,
    /// Latencies in milliseconds.
    pub latency_ms: Vec<i64>,
    pub responses: u64,
    pub timeouts: u64,
    pub exceptions: u64,
}

impl ReadStats {
    /// Intervals between consecutive requests, in milliseconds.
    pub fn intervals_ms(&self) -> Vec<i64> {
        self.times
            .windows(2)
            .map(|w| (w[1] - w[0]).num_milliseconds())
            .collect()
    }
}

/// Per-slave activity summary.
#[derive(Debug, Default, Clone)]
pub struct SlaveStats {
    pub requests: u64,
    pub responses: u64,
    pub exceptions: u64,
    pub timeouts: u64,
    pub funcs: BTreeSet<u8>,
}

#[derive(Debug, Default)]
pub struct Decoded {
    pub map: RegisterMap,
    pub writes: Vec<WriteOp>,
    pub reads: BTreeMap<ReadKey, ReadStats>,
    pub slaves: BTreeMap<u8, SlaveStats>,
    /// (time, slave, base function, exception code).
    pub exceptions: Vec<(Time, u8, u8, u8)>,
    /// Responses whose payload did not match the request's byte count.
    pub malformed: u64,
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

/// Decode the exchanges. `windows` bounds which value transitions are retained.
pub fn decode(paired: &Paired, windows: &[(Time, Time)]) -> Decoded {
    let mut out = Decoded::default();
    let keep = |t: Time| windows.iter().any(|(a, b)| t >= *a && t <= *b);

    for ex in &paired.exchanges {
        let req = &ex.request;
        let entry = out.slaves.entry(req.slave).or_default();
        entry.requests += 1;
        entry.funcs.insert(req.func);
        match &ex.response {
            None => entry.timeouts += 1,
            Some(r) if r.is_exception() => entry.exceptions += 1,
            Some(_) => entry.responses += 1,
        }

        if let Some(r) = &ex.response {
            if r.is_exception() {
                if let Some(code) = r.body.first() {
                    out.exceptions.push((r.time, r.slave, r.base_func(), *code));
                }
            }
        }

        if let Some(kind) = RegKind::for_read(req.func) {
            decode_read(&mut out, ex, kind, &keep);
        } else if let Some(kind) = RegKind::for_write(req.func) {
            decode_write(&mut out, ex, kind);
        }
    }

    out.exceptions.sort_by_key(|(t, _, _, _)| *t);
    out.writes.sort_by_key(|w| w.time);
    out
}

fn decode_read(
    out: &mut Decoded,
    ex: &super::pair::Exchange,
    kind: RegKind,
    keep: &impl Fn(Time) -> bool,
) {
    let req = &ex.request;
    let (Some(start), Some(count)) = (be16(&req.body, 0), be16(&req.body, 2)) else {
        return;
    };
    let key = ReadKey {
        slave: req.slave,
        func: req.func,
        start,
        count,
    };
    let stats = out.reads.entry(key).or_default();
    stats.times.push(req.time);
    match &ex.response {
        None => stats.timeouts += 1,
        Some(r) if r.is_exception() => stats.exceptions += 1,
        Some(r) => {
            stats.responses += 1;
            stats
                .latency_ms
                .push((r.time - req.time).num_milliseconds());
        }
    }

    let Some(resp) = &ex.response else { return };
    if resp.is_exception() {
        return;
    }
    let Some((_, data)) = resp.body.split_first() else {
        out.malformed += 1;
        return;
    };
    // The byte count always agrees with the payload length - that is how the
    // frame was classified as a response in the first place. What can still be
    // wrong is the payload not covering the register count the request asked
    // for, which would make every address after the truncation point a guess.
    let expected = if kind.is_bit() {
        (count as usize).div_ceil(8)
    } else {
        count as usize * 2
    };
    if data.len() != expected {
        out.malformed += 1;
        return;
    }

    if kind.is_bit() {
        for i in 0..count {
            let byte = i as usize / 8;
            let bit = i as usize % 8;
            let Some(b) = data.get(byte) else { break };
            let v = u16::from((b >> bit) & 1);
            out.map
                .observe(kind, start.wrapping_add(i), v, resp.time, keep(resp.time));
        }
    } else {
        for i in 0..count {
            let Some(v) = be16(data, i as usize * 2) else {
                break;
            };
            out.map
                .observe(kind, start.wrapping_add(i), v, resp.time, keep(resp.time));
        }
    }
}

fn decode_write(out: &mut Decoded, ex: &super::pair::Exchange, kind: RegKind) {
    let req = &ex.request;
    let Some(addr) = be16(&req.body, 0) else {
        return;
    };

    let values = match req.func {
        // Write Single Coil: 0xFF00 on, 0x0000 off.
        0x05 => match be16(&req.body, 2) {
            Some(0xff00) => vec![1],
            Some(0x0000) => vec![0],
            Some(other) => vec![other],
            None => return,
        },
        0x06 => match be16(&req.body, 2) {
            Some(v) => vec![v],
            None => return,
        },
        0x0f => {
            let Some(qty) = be16(&req.body, 2) else {
                return;
            };
            let data = req.body.get(5..).unwrap_or(&[]);
            (0..qty)
                .map(|i| {
                    let byte = i as usize / 8;
                    let bit = i as usize % 8;
                    u16::from(data.get(byte).map_or(0, |b| (b >> bit) & 1))
                })
                .collect()
        }
        0x10 => {
            let Some(qty) = be16(&req.body, 2) else {
                return;
            };
            let data = req.body.get(5..).unwrap_or(&[]);
            (0..qty)
                .filter_map(|i| be16(data, i as usize * 2))
                .collect()
        }
        _ => return,
    };

    let exception = ex
        .response
        .as_ref()
        .filter(|r| r.is_exception())
        .and_then(|r| r.body.first().copied());

    out.writes.push(WriteOp {
        time: req.time,
        slave: req.slave,
        func: req.func,
        kind,
        addr,
        values,
        acked: ex.response.is_some() && exception.is_none(),
        exception,
    });
}

/// Writes inside `[from, to]`.
pub fn writes_between(writes: &[WriteOp], from: Time, to: Time) -> Vec<&WriteOp> {
    writes
        .iter()
        .filter(|w| w.time >= from && w.time <= to)
        .collect()
}

/// Median of a slice, by value. `None` for an empty slice.
pub fn median(values: &[i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    Some(v[v.len() / 2])
}

/// Convenience: min, median, max in milliseconds.
pub fn spread(values: &[i64]) -> Option<(i64, i64, i64)> {
    let min = *values.iter().min()?;
    let max = *values.iter().max()?;
    Some((min, median(values)?, max))
}

/// Chrono helper: `TimeDelta` from whole milliseconds, saturating.
pub fn delta_ms(ms: i64) -> TimeDelta {
    TimeDelta::try_milliseconds(ms).unwrap_or(TimeDelta::zero())
}

#[cfg(test)]
mod tests {
    use super::super::pair::pair;
    use super::super::parse::parse_lines;
    use super::*;

    fn decoded(lines: &[&str]) -> Decoded {
        let cap = parse_lines(lines.iter().copied());
        let p = pair(&cap.frames, TimeDelta::seconds(1));
        // Keep every change, so the tests see them all.
        let all = [(
            cap.first.expect("first") - TimeDelta::days(1),
            cap.last.expect("last") + TimeDelta::days(1),
        )];
        decode(&p, &all)
    }

    const READ_LOOP: &[&str] = &[
        "2026-10-03T10:00:00.000+02:00 [1.000] RX slave=0x01 func=0x03 (r) data=00 64 00 02 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:00.020+02:00 [1.020] RX slave=0x01 func=0x03 (r) data=04 00 dc 00 01 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:01.000+02:00 [2.000] RX slave=0x01 func=0x03 (r) data=00 64 00 02 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:01.030+02:00 [2.030] RX slave=0x01 func=0x03 (r) data=04 00 e6 00 01 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:02.000+02:00 [3.000] RX slave=0x01 func=0x03 (r) data=00 64 00 02 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:02.010+02:00 [3.010] RX slave=0x01 func=0x03 (r) data=04 00 e6 00 01 crc=0x0 computed=0x0 OK",
    ];

    #[test]
    fn tracks_holding_registers() {
        let d = decoded(READ_LOOP);
        let changing = &d.map.regs[&(RegKind::Holding, 0x0064)];
        assert_eq!(changing.first, 0x00dc);
        assert_eq!(changing.last, 0x00e6);
        assert_eq!(changing.samples, 3);
        assert_eq!(changing.distinct.len(), 2);
        assert_eq!((changing.min, changing.max), (0x00dc, 0x00e6));
        assert!(!changing.is_constant());
        assert_eq!(changing.changes.len(), 1);
        assert_eq!(changing.changes[0].from, 0x00dc);
        assert_eq!(changing.changes[0].to, 0x00e6);

        let constant = &d.map.regs[&(RegKind::Holding, 0x0065)];
        assert!(constant.is_constant());
        assert_eq!(constant.last, 1);
    }

    #[test]
    fn summarises_the_poll_loop() {
        let d = decoded(READ_LOOP);
        let key = ReadKey {
            slave: 0x01,
            func: 0x03,
            start: 0x0064,
            count: 2,
        };
        let s = &d.reads[&key];
        assert_eq!(s.times.len(), 3);
        assert_eq!(s.responses, 3);
        assert_eq!(s.intervals_ms(), vec![1000, 1000]);
        assert_eq!(spread(&s.latency_ms), Some((10, 20, 30)));
    }

    #[test]
    fn decodes_coil_reads_as_bits() {
        let d = decoded(&[
            "2026-10-03T10:00:00.000+02:00 [1.000] RX slave=0x01 func=0x01 (r) data=00 00 00 03 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.010+02:00 [1.010] RX slave=0x01 func=0x01 (r) data=01 05 crc=0x0 computed=0x0 OK",
        ]);
        assert_eq!(d.map.regs[&(RegKind::Coil, 0)].last, 1);
        assert_eq!(d.map.regs[&(RegKind::Coil, 1)].last, 0);
        assert_eq!(d.map.regs[&(RegKind::Coil, 2)].last, 1);
    }

    #[test]
    fn collects_writes_separately() {
        let d = decoded(&[
            "2026-10-03T10:00:00.000+02:00 [1.000] RX slave=0x01 func=0x06 (w) data=00 64 00 e6 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.030+02:00 [1.030] RX slave=0x01 func=0x06 (w) data=00 64 00 e6 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:01.000+02:00 [2.000] RX slave=0x01 func=0x10 (w) data=00 c8 00 02 04 00 01 00 02 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:01.030+02:00 [2.030] RX slave=0x01 func=0x10 (w) data=00 c8 00 02 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:02.000+02:00 [3.000] RX slave=0x01 func=0x05 (w) data=00 03 ff 00 crc=0x0 computed=0x0 OK",
        ]);
        assert_eq!(d.writes.len(), 3);
        assert_eq!(d.writes[0].addr, 0x0064);
        assert_eq!(d.writes[0].values, vec![0x00e6]);
        assert!(d.writes[0].acked);
        assert_eq!(d.writes[1].func, 0x10);
        assert_eq!(d.writes[1].values, vec![1, 2]);
        assert_eq!(d.writes[2].kind, RegKind::Coil);
        assert_eq!(d.writes[2].values, vec![1]);
        assert!(
            !d.writes[2].acked,
            "no response was logged for the coil write"
        );
        // Writes are not folded into the register map.
        assert!(d.map.regs.is_empty());
    }

    #[test]
    fn records_exceptions() {
        let d = decoded(&[
            "2026-10-03T10:00:00.000+02:00 [1.000] RX slave=0x01 func=0x03 (r) data=00 ff 00 02 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.010+02:00 [1.010] RX slave=0x01 func=0x83 (e) exception=0x02 (Illegal Data Address) crc=0x0 computed=0x0 OK",
        ]);
        assert_eq!(d.exceptions.len(), 1);
        assert_eq!(d.exceptions[0].1, 0x01);
        assert_eq!(d.exceptions[0].2, 0x03);
        assert_eq!(d.exceptions[0].3, 0x02);
        assert_eq!(d.slaves[&0x01].exceptions, 1);
    }

    #[test]
    fn flags_a_response_that_is_short_of_the_requested_count() {
        // Two registers asked for, only one came back.
        let d = decoded(&[
            "2026-10-03T10:00:00.000+02:00 [1.000] RX slave=0x01 func=0x03 (r) data=00 00 00 02 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.010+02:00 [1.010] RX slave=0x01 func=0x03 (r) data=02 00 01 crc=0x0 computed=0x0 OK",
        ]);
        assert_eq!(d.malformed, 1);
        assert!(d.map.regs.is_empty());
    }

    #[test]
    fn changes_outside_the_window_are_not_retained() {
        let cap = parse_lines(READ_LOOP.iter().copied());
        let p = pair(&cap.frames, TimeDelta::seconds(1));
        let first = cap.first.expect("first");
        // A window that ends before the value actually changes.
        let d = decode(&p, &[(first, first + TimeDelta::milliseconds(100))]);
        let s = &d.map.regs[&(RegKind::Holding, 0x0064)];
        assert_eq!(s.distinct.len(), 2, "min/max/distinct still see everything");
        assert!(s.changes.is_empty(), "but no transition was retained");
    }

    #[test]
    fn median_and_spread() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[5]), Some(5));
        assert_eq!(median(&[9, 1, 5]), Some(5));
        assert_eq!(spread(&[9, 1, 5]), Some((1, 5, 9)));
    }
}
