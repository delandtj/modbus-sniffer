//! Rendering the protocol map as markdown.

use std::fmt::Write as _;

use chrono::{SecondsFormat, TimeDelta};
use modbus_sniffer_core as mb;

use super::pair::{pair, Paired};
use super::parse::{Capture, Time};
use super::regs::{
    decode, median, spread, writes_between, Decoded, ReadKey, RegKind, RegStats, WriteOp,
};
use super::Options;

/// How many rows a list section prints before it says "and N more".
const MAX_ROWS: usize = 200;

pub fn report(cap: &Capture, opts: &Options) -> String {
    let max_latency = TimeDelta::from_std(opts.max_latency).unwrap_or(TimeDelta::seconds(1));
    let window = TimeDelta::from_std(opts.window).unwrap_or(TimeDelta::seconds(10));
    let gap = TimeDelta::from_std(opts.gap).unwrap_or(TimeDelta::seconds(30));

    let paired = pair(&cap.frames, max_latency);
    let windows: Vec<(Time, Time)> = cap
        .notes
        .iter()
        .map(|n| (n.time - window, n.time + window))
        .collect();
    let decoded = decode(&paired, &windows);

    let mut out = String::new();
    let _ = writeln!(out, "# Modbus RTU protocol map\n");
    section_overview(&mut out, cap, &paired, gap);
    section_roles(&mut out, cap, &paired, &decoded);
    section_polling(&mut out, &decoded);
    section_registers(&mut out, &decoded);
    section_notes(&mut out, cap, &decoded, window);
    section_anomalies(&mut out, cap, &paired, &decoded);
    out
}

// ---------------------------------------------------------------------------
// 1. Overview
// ---------------------------------------------------------------------------

fn section_overview(out: &mut String, cap: &Capture, paired: &Paired, gap: TimeDelta) {
    let _ = writeln!(out, "## 1. Overview\n");

    if cap.files.is_empty() {
        let _ = writeln!(out, "Input: standard input.\n");
    } else {
        let _ = writeln!(out, "Input files:\n");
        for f in &cap.files {
            let _ = writeln!(out, "- `{f}`");
        }
        let _ = writeln!(out);
    }

    match (cap.first, cap.last) {
        (Some(a), Some(b)) => {
            let _ = writeln!(out, "- Time span: {} .. {}", fmt_time(a), fmt_time(b));
            let _ = writeln!(out, "- Duration: {}", fmt_delta(b - a));
        }
        _ => {
            let _ = writeln!(out, "- Time span: the log held no timestamped lines.");
        }
    }
    let _ = writeln!(out, "- Log lines read: {}", cap.lines);
    let _ = writeln!(out, "- Frames with a good CRC: {}", cap.frames.len());
    let _ = writeln!(out, "- Frames with a bad CRC: {}", cap.bad_crc.len());
    let _ = writeln!(out, "- Short frames (under 4 bytes): {}", cap.short.len());
    let dropped: u64 = cap.dropped.iter().map(|(_, n, _)| n).sum();
    let _ = writeln!(
        out,
        "- Frames the device had to drop: {dropped} (in {} markers)",
        cap.dropped.len()
    );
    let _ = writeln!(out, "- Device reboots: {}", cap.reboots.len());
    let _ = writeln!(
        out,
        "- Request/response exchanges: {}",
        paired.exchanges.len()
    );
    let _ = writeln!(out, "- Unparsed lines: {}", cap.unparsed);
    let _ = writeln!(out);

    let _ = writeln!(out, "### Bus settings seen\n");
    let mut shown: Vec<(Time, u32, String)> = Vec::new();
    for (t, baud, fmt) in &cap.bus {
        if shown
            .last()
            .is_some_and(|(_, b, f)| *b == *baud && f == fmt)
        {
            continue;
        }
        shown.push((*t, *baud, fmt.clone()));
    }
    if shown.is_empty() {
        let _ = writeln!(
            out,
            "No `# hello` or `# bus` line in the log, so the bus settings are unknown.\n"
        );
    } else {
        let _ = writeln!(out, "| From | Baud | Format |");
        let _ = writeln!(out, "|---|---|---|");
        for (t, baud, fmt) in shown.iter().take(MAX_ROWS) {
            let _ = writeln!(out, "| {} | {baud} | {fmt} |", fmt_time(*t));
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "### Reboots\n");
    if cap.reboots.is_empty() {
        let _ = writeln!(out, "None.\n");
    } else {
        for t in cap.reboots.iter().take(MAX_ROWS) {
            let _ = writeln!(out, "- {}", fmt_time(*t));
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(
        out,
        "### Silent gaps (no bus traffic for over {})\n",
        fmt_delta(gap)
    );
    let activity = cap.bus_activity();
    let gaps: Vec<(Time, Time)> = activity
        .windows(2)
        .filter(|w| w[1] - w[0] > gap)
        .map(|w| (w[0], w[1]))
        .collect();
    if gaps.is_empty() {
        let _ = writeln!(out, "None.\n");
    } else {
        let _ = writeln!(out, "| From | To | Length |");
        let _ = writeln!(out, "|---|---|---|");
        for (a, b) in gaps.iter().take(MAX_ROWS) {
            let _ = writeln!(
                out,
                "| {} | {} | {} |",
                fmt_time(*a),
                fmt_time(*b),
                fmt_delta(*b - *a)
            );
        }
        if gaps.len() > MAX_ROWS {
            let _ = writeln!(out, "\nand {} more.", gaps.len() - MAX_ROWS);
        }
        let _ = writeln!(out);
    }
}

// ---------------------------------------------------------------------------
// 2. Roles
// ---------------------------------------------------------------------------

fn section_roles(out: &mut String, cap: &Capture, paired: &Paired, decoded: &Decoded) {
    let _ = writeln!(out, "## 2. Roles\n");
    let _ = writeln!(
        out,
        "The sniffer taps one wire and cannot see direction, so these roles come \
         from frame shape and order: a request is followed, within the pairing \
         window, by a frame on the same slave address with the same function code \
         (or that code with bit 7 set)."
    );
    let _ = writeln!(out);

    let paired_count = paired
        .exchanges
        .iter()
        .filter(|e| e.response.is_some())
        .count();
    let _ = writeln!(out, "- Exchanges: {}", paired.exchanges.len());
    let _ = writeln!(out, "- Answered: {paired_count}");
    let _ = writeln!(
        out,
        "- Unanswered requests: {}",
        paired.exchanges.len() - paired_count
    );
    let _ = writeln!(
        out,
        "- Responses with no matching request: {}",
        paired.orphan_responses.len()
    );
    let _ = writeln!(
        out,
        "- Frames of unrecognised shape: {}",
        paired.unknown.len()
    );
    let _ = writeln!(out);

    if decoded.slaves.is_empty() {
        let _ = writeln!(out, "No addressed exchanges were found.\n");
        return;
    }

    let _ = writeln!(out, "### Slave addresses\n");
    let _ = writeln!(
        out,
        "| Slave | Requests | Answered | Exceptions | Unanswered | Functions |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|");
    for (addr, s) in &decoded.slaves {
        let funcs: Vec<String> = s
            .funcs
            .iter()
            .map(|f| format!("0x{f:02x} {}", mb::format_function(*f)))
            .collect();
        let _ = writeln!(
            out,
            "| 0x{addr:02x} ({addr}) | {} | {} | {} | {} | {} |",
            s.requests,
            s.responses,
            s.exceptions,
            s.timeouts,
            funcs.join("; ")
        );
    }
    let _ = writeln!(out);

    let addrs: Vec<String> = decoded
        .slaves
        .keys()
        .map(|a| format!("0x{a:02x}"))
        .collect();
    let _ = writeln!(
        out,
        "One side initiates every exchange; that side is the master. On this bus \
         the master polls {} slave address(es): {}.",
        decoded.slaves.len(),
        addrs.join(", ")
    );
    let _ = writeln!(
        out,
        "\nThe wired controller is the only device on this bus that sends requests, \
         so to replace it your own device has to become the master and reproduce the \
         poll loop in section 3."
    );
    if !cap.bad_crc.is_empty() {
        let _ = writeln!(
            out,
            "\n{} frame(s) with a bad CRC were counted but not decoded, so they do \
             not appear in any role or register table.",
            cap.bad_crc.len()
        );
    }
    let _ = writeln!(out);
}

// ---------------------------------------------------------------------------
// 3. Polling loop
// ---------------------------------------------------------------------------

fn section_polling(out: &mut String, decoded: &Decoded) {
    let _ = writeln!(out, "## 3. Polling loop\n");
    if decoded.reads.is_empty() {
        let _ = writeln!(out, "No read requests were seen.\n");
        return;
    }

    let mut rows: Vec<(&ReadKey, &super::regs::ReadStats)> = decoded.reads.iter().collect();
    rows.sort_by_key(|(_, s)| std::cmp::Reverse(s.times.len()));

    let _ = writeln!(
        out,
        "| Slave | Function | Start | Count | Requests | Period min/median/max | Latency min/median/max | Answered | Exceptions | Unanswered |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|---|");
    for (k, s) in rows.iter().take(MAX_ROWS) {
        let intervals = s.intervals_ms();
        let _ = writeln!(
            out,
            "| 0x{:02x} | 0x{:02x} {} | 0x{:04x} ({}) | {} | {} | {} | {} | {} | {} | {} |",
            k.slave,
            k.func,
            mb::format_function(k.func),
            k.start,
            k.start,
            k.count,
            s.times.len(),
            fmt_spread(&intervals),
            fmt_spread(&s.latency_ms),
            s.responses,
            s.exceptions,
            s.timeouts
        );
    }
    if rows.len() > MAX_ROWS {
        let _ = writeln!(out, "\nand {} more.", rows.len() - MAX_ROWS);
    }

    // The dominant request is the best starting point for a replacement master.
    if let Some((k, s)) = rows.first() {
        let intervals = s.intervals_ms();
        if let Some(m) = median(&intervals) {
            let _ = writeln!(
                out,
                "\nThe busiest request is slave 0x{:02x} function 0x{:02x} from 0x{:04x} for {} \
                 register(s), about every {}.",
                k.slave,
                k.func,
                k.start,
                k.count,
                fmt_ms(m)
            );
        }
    }
    let _ = writeln!(out);
}

// ---------------------------------------------------------------------------
// 4. Register map
// ---------------------------------------------------------------------------

fn section_registers(out: &mut String, decoded: &Decoded) {
    let _ = writeln!(out, "## 4. Register map\n");

    let kinds = [
        RegKind::Holding,
        RegKind::Input,
        RegKind::Coil,
        RegKind::DiscreteInput,
    ];
    let mut any = false;
    for kind in kinds {
        let all: Vec<(u16, &RegStats)> = decoded.map.by_kind(kind).collect();
        if all.is_empty() {
            continue;
        }
        any = true;
        let (moving, constant): (Vec<_>, Vec<_>) = all.iter().partition(|(_, s)| !s.is_constant());
        let (free_running, changing): (Vec<_>, Vec<_>) =
            moving.into_iter().partition(|(_, s)| s.is_free_running());

        let _ = writeln!(out, "### {} ({} addresses)\n", kind.plural(), all.len());

        let _ = writeln!(out, "Changing: {}", changing.len());
        if !changing.is_empty() {
            let _ = writeln!(out);
            reg_table(out, kind, &changing);
        }
        let _ = writeln!(
            out,
            "\nFree-running (moves on over half the polls - a counter, an uptime or \
             a live measurement): {}",
            free_running.len()
        );
        if !free_running.is_empty() {
            let _ = writeln!(out);
            reg_table(out, kind, &free_running);
        }
        let _ = writeln!(out, "\nConstant: {}", constant.len());
        if !constant.is_empty() {
            let _ = writeln!(out);
            reg_table(out, kind, &constant);
        }
        let _ = writeln!(out);
    }
    if !any {
        let _ = writeln!(out, "No register values were decoded.\n");
    }

    let _ = writeln!(out, "### Writes\n");
    if decoded.writes.is_empty() {
        let _ = writeln!(
            out,
            "No write requests (0x05, 0x06, 0x0F, 0x10) were seen. Every setting \
             the controller changes therefore either goes out on a function this \
             capture did not catch, or the capture simply did not cover a change.\n"
        );
    } else {
        let _ = writeln!(
            out,
            "| Time | Slave | Function | Table | Address | Values | Result |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|---|");
        for w in decoded.writes.iter().take(MAX_ROWS) {
            let _ = writeln!(out, "{}", write_row(w));
        }
        if decoded.writes.len() > MAX_ROWS {
            let _ = writeln!(out, "\nand {} more.", decoded.writes.len() - MAX_ROWS);
        }
        let _ = writeln!(out);
    }
}

fn reg_table(out: &mut String, kind: RegKind, rows: &[(u16, &RegStats)]) {
    if kind.is_bit() {
        let _ = writeln!(
            out,
            "| Address | Samples | Changes | Distinct | First | Last | First seen | Last seen |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|---|---|");
        for (addr, s) in rows.iter().take(MAX_ROWS) {
            let _ = writeln!(
                out,
                "| 0x{addr:04x} ({addr}) | {} | {} | {} | {} | {} | {} | {} |",
                s.samples,
                s.transitions,
                s.distinct.len(),
                s.first,
                s.last,
                fmt_time(s.first_time),
                fmt_time(s.last_time)
            );
        }
    } else {
        let _ = writeln!(
            out,
            "| Address | Samples | Changes | Distinct | First | Last | u16 | s16 | /10 | Min | Max |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|---|---|");
        for (addr, s) in rows.iter().take(MAX_ROWS) {
            let _ = writeln!(
                out,
                "| 0x{addr:04x} ({addr}) | {} | {} | {} | 0x{:04x} | 0x{:04x} | {} | {} | {} | 0x{:04x} | 0x{:04x} |",
                s.samples,
                s.transitions,
                s.distinct.len(),
                s.first,
                s.last,
                s.last,
                s.last as i16,
                fmt_tenths(s.last),
                s.min,
                s.max
            );
        }
    }
    if rows.len() > MAX_ROWS {
        let _ = writeln!(out, "\nand {} more.", rows.len() - MAX_ROWS);
    }
}

fn write_row(w: &WriteOp) -> String {
    let values: Vec<String> = w
        .values
        .iter()
        .map(|v| {
            if w.kind.is_bit() {
                v.to_string()
            } else {
                format!("0x{v:04x} ({v}, {}, {})", *v as i16, fmt_tenths(*v))
            }
        })
        .collect();
    let result = match (w.acked, w.exception) {
        (_, Some(code)) => format!("exception 0x{code:02x} ({})", mb::format_exception(code)),
        (true, None) => "acked".to_string(),
        (false, None) => "no response in the log".to_string(),
    };
    format!(
        "| {} | 0x{:02x} | 0x{:02x} {} | {} | 0x{:04x} ({}) | {} | {result} |",
        fmt_time(w.time),
        w.slave,
        w.func,
        mb::format_function(w.func),
        w.kind.label(),
        w.addr,
        w.addr,
        values.join(", ")
    )
}

// ---------------------------------------------------------------------------
// 5. Notes correlation
// ---------------------------------------------------------------------------

fn section_notes(out: &mut String, cap: &Capture, decoded: &Decoded, window: TimeDelta) {
    let _ = writeln!(out, "## 5. Notes correlation\n");
    if cap.notes.is_empty() {
        let _ = writeln!(
            out,
            "No notes in this log. Use `sniffer-capture note \"...\"` while you change \
             a setting on the controller; this section then tells you which registers \
             moved.\n"
        );
        return;
    }
    let _ = writeln!(out, "Window: +-{} around each note.\n", fmt_delta(window));

    for note in &cap.notes {
        let from = note.time - window;
        let to = note.time + window;
        let _ = writeln!(
            out,
            "### {} ({}) {}\n",
            fmt_time(note.time),
            note.source.label(),
            note.text
        );

        let writes = writes_between(&decoded.writes, from, to);
        if writes.is_empty() {
            let _ = writeln!(out, "Writes in window: none.\n");
        } else {
            let _ = writeln!(out, "Writes in window: {}\n", writes.len());
            let _ = writeln!(
                out,
                "| Time | Slave | Function | Table | Address | Values | Result |"
            );
            let _ = writeln!(out, "|---|---|---|---|---|---|---|");
            for w in writes.iter().take(MAX_ROWS) {
                let _ = writeln!(out, "{}", write_row(w));
            }
            let _ = writeln!(out);
        }

        // Free-running registers move on every poll, so listing them here would
        // bury the one register that actually answers to the note.
        let all_changes = decoded.map.changes_between(from, to);
        let (noisy, changes): (Vec<_>, Vec<_>) = all_changes
            .into_iter()
            .partition(|(kind, addr, _)| decoded.map.is_free_running(*kind, *addr));

        if changes.is_empty() {
            let _ = writeln!(out, "Register changes in window: none.\n");
        } else {
            let _ = writeln!(out, "Register changes in window: {}\n", changes.len());
            let _ = writeln!(out, "| Time | Table | Address | Old | New | Delta |");
            let _ = writeln!(out, "|---|---|---|---|---|---|");
            for (kind, addr, c) in changes.iter().take(MAX_ROWS) {
                let delta = i32::from(c.to as i16) - i32::from(c.from as i16);
                let _ = writeln!(
                    out,
                    "| {} | {} | 0x{addr:04x} ({addr}) | 0x{:04x} ({}) | 0x{:04x} ({}) | {delta:+} |",
                    fmt_time(c.time),
                    kind.label(),
                    c.from,
                    c.from as i16,
                    c.to,
                    c.to as i16
                );
            }
            if changes.len() > MAX_ROWS {
                let _ = writeln!(out, "\nand {} more.", changes.len() - MAX_ROWS);
            }
            let _ = writeln!(out);
        }

        if !noisy.is_empty() {
            let mut addrs: Vec<String> = noisy
                .iter()
                .map(|(_, addr, _)| format!("0x{addr:04x}"))
                .collect();
            addrs.sort();
            addrs.dedup();
            let _ = writeln!(
                out,
                "Plus {} change(s) on free-running registers, not listed: {}.\n",
                noisy.len(),
                addrs.join(", ")
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 6. Exceptions and anomalies
// ---------------------------------------------------------------------------

fn section_anomalies(out: &mut String, cap: &Capture, paired: &Paired, decoded: &Decoded) {
    let _ = writeln!(out, "## 6. Exceptions and anomalies\n");

    let _ = writeln!(out, "### Exception responses\n");
    if decoded.exceptions.is_empty() {
        let _ = writeln!(out, "None.\n");
    } else {
        let _ = writeln!(out, "| Time | Slave | Function | Code | Meaning |");
        let _ = writeln!(out, "|---|---|---|---|---|");
        for (t, slave, func, code) in decoded.exceptions.iter().take(MAX_ROWS) {
            let _ = writeln!(
                out,
                "| {} | 0x{slave:02x} | 0x{func:02x} {} | 0x{code:02x} | {} |",
                fmt_time(*t),
                mb::format_function(*func),
                mb::format_exception(*code)
            );
        }
        if decoded.exceptions.len() > MAX_ROWS {
            let _ = writeln!(out, "\nand {} more.", decoded.exceptions.len() - MAX_ROWS);
        }
        let _ = writeln!(out);
    }

    let timeouts: Vec<_> = paired.timeouts().collect();
    let _ = writeln!(out, "### Requests with no response\n");
    if timeouts.is_empty() {
        let _ = writeln!(out, "None.\n");
    } else {
        let _ = writeln!(out, "{} in total.\n", timeouts.len());
        let _ = writeln!(out, "| Time | Slave | Function | Body |");
        let _ = writeln!(out, "|---|---|---|---|");
        for e in timeouts.iter().take(MAX_ROWS) {
            let r = &e.request;
            let _ = writeln!(
                out,
                "| {} | 0x{:02x} | 0x{:02x} {} | {} |",
                fmt_time(r.time),
                r.slave,
                r.func,
                mb::format_function(r.func),
                fmt_hex(&r.body)
            );
        }
        if timeouts.len() > MAX_ROWS {
            let _ = writeln!(out, "\nand {} more.", timeouts.len() - MAX_ROWS);
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "### Unexpected functions and shapes\n");
    if paired.unknown.is_empty() && paired.orphan_responses.is_empty() {
        let _ = writeln!(out, "None.\n");
    } else {
        let mut counts: std::collections::BTreeMap<u8, usize> = Default::default();
        for f in paired.unknown.iter().chain(paired.orphan_responses.iter()) {
            *counts.entry(f.func).or_default() += 1;
        }
        let _ = writeln!(out, "| Function | Frames | Name |");
        let _ = writeln!(out, "|---|---|---|");
        for (func, n) in &counts {
            let _ = writeln!(
                out,
                "| 0x{func:02x} | {n} | {} |",
                mb::format_function(*func & 0x7f)
            );
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "First few, verbatim:\n");
        for f in paired
            .unknown
            .iter()
            .chain(paired.orphan_responses.iter())
            .take(20)
        {
            let _ = writeln!(
                out,
                "- {} slave=0x{:02x} func=0x{:02x} body={}",
                fmt_time(f.time),
                f.slave,
                f.func,
                fmt_hex(&f.body)
            );
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "### Line-level problems\n");
    let _ = writeln!(out, "- Bad CRC frames: {}", cap.bad_crc.len());
    let _ = writeln!(out, "- Short frames: {}", cap.short.len());
    let _ = writeln!(
        out,
        "- Responses whose byte count did not match the payload: {}",
        decoded.malformed
    );
    let _ = writeln!(out, "- Unparsed log lines: {}", cap.unparsed);
    if !cap.dropped.is_empty() {
        let _ = writeln!(out, "- Drop markers:");
        for (t, n, unit) in cap.dropped.iter().take(MAX_ROWS) {
            let _ = writeln!(out, "  - {} lost {n} {unit}", fmt_time(*t));
        }
    }
    let _ = writeln!(out);
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

pub fn fmt_time(t: Time) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, false)
}

pub fn fmt_hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "(empty)".to_string();
    }
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn fmt_tenths(v: u16) -> String {
    format!("{:.1}", f64::from(v as i16) / 10.0)
}

pub fn fmt_ms(ms: i64) -> String {
    if ms.abs() < 1_000 {
        format!("{ms} ms")
    } else {
        format!("{:.3} s", ms as f64 / 1000.0)
    }
}

pub fn fmt_spread(values: &[i64]) -> String {
    match spread(values) {
        Some((min, med, max)) => format!("{} / {} / {}", fmt_ms(min), fmt_ms(med), fmt_ms(max)),
        None => "-".to_string(),
    }
}

pub fn fmt_delta(d: TimeDelta) -> String {
    let total = d.num_milliseconds();
    if total < 0 {
        return format!("-{}", fmt_delta(-d));
    }
    let ms = total % 1000;
    let s = total / 1000 % 60;
    let m = total / 60_000 % 60;
    let h = total / 3_600_000 % 24;
    let days = total / 86_400_000;
    let mut parts: Vec<String> = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if h > 0 || days > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 || h > 0 || days > 0 {
        parts.push(format!("{m}m"));
    }
    if ms > 0 && days == 0 && h == 0 {
        parts.push(format!("{s}.{ms:03}s"));
    } else {
        parts.push(format!("{s}s"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::parse::parse_lines;
    use std::time::Duration;

    const LOG: &[&str] = &[
        "2026-10-03T10:00:00.000+02:00 CAPTURE started, device modbus-sniffer:4000",
        "2026-10-03T10:00:00.050+02:00 # hello sniffer-esp32c6 0.1.0 uptime_ms=1000 bus=9600 8E1 replay=0",
        "2026-10-03T10:00:01.000+02:00 [2.000] RX slave=0x01 func=0x03 (r) data=00 64 00 02 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:01.020+02:00 [2.020] RX slave=0x01 func=0x03 (r) data=04 00 dc 00 01 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:02.000+02:00 [3.000] RX slave=0x01 func=0x03 (r) data=00 64 00 02 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:02.020+02:00 [3.020] RX slave=0x01 func=0x03 (r) data=04 00 dc 00 01 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:03.000+02:00 # note [4.000] setpoint 22.0 -> 23.0",
        "2026-10-03T10:00:03.500+02:00 [4.500] RX slave=0x01 func=0x06 (w) data=00 64 00 e6 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:03.530+02:00 [4.530] RX slave=0x01 func=0x06 (w) data=00 64 00 e6 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:04.000+02:00 [5.000] RX slave=0x01 func=0x03 (r) data=00 64 00 02 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:04.020+02:00 [5.020] RX slave=0x01 func=0x03 (r) data=04 00 e6 00 01 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:05.000+02:00 [6.000] RX slave=0x01 func=0x03 (r) data=00 ff 00 02 crc=0x0 computed=0x0 OK",
        "2026-10-03T10:00:05.010+02:00 [6.010] RX slave=0x01 func=0x83 (e) exception=0x02 (Illegal Data Address) crc=0x0 computed=0x0 OK",
    ];

    fn render(lines: &[&str], window: Duration) -> String {
        let cap = parse_lines(lines.iter().copied());
        report(
            &cap,
            &Options {
                window,
                ..Options::default()
            },
        )
    }

    #[test]
    fn report_has_all_six_sections() {
        let md = render(LOG, Duration::from_secs(10));
        for heading in [
            "## 1. Overview",
            "## 2. Roles",
            "## 3. Polling loop",
            "## 4. Register map",
            "## 5. Notes correlation",
            "## 6. Exceptions and anomalies",
        ] {
            assert!(md.contains(heading), "missing {heading} in:\n{md}");
        }
    }

    #[test]
    fn overview_counts_and_bus_settings() {
        let md = render(LOG, Duration::from_secs(10));
        assert!(md.contains("- Frames with a good CRC: 10"), "{md}");
        assert!(md.contains("| 9600 | 8E1 |"), "{md}");
    }

    #[test]
    fn note_correlation_finds_the_write_and_the_change() {
        let md = render(LOG, Duration::from_secs(10));
        let section = md
            .split("## 5. Notes correlation")
            .nth(1)
            .expect("notes section")
            .split("## 6.")
            .next()
            .expect("end of notes section");
        assert!(section.contains("setpoint 22.0 -> 23.0"), "{section}");
        assert!(section.contains("Writes in window: 1"), "{section}");
        assert!(
            section.contains("Register changes in window: 1"),
            "{section}"
        );
        // 0x00dc -> 0x00e6 is 220 -> 230, i.e. 22.0 -> 23.0 at one decimal.
        assert!(
            section.contains("0x00dc (220) | 0x00e6 (230) | +10"),
            "{section}"
        );
    }

    /// A counter that steps on every poll would otherwise bury the one register
    /// that actually responded to the note.
    #[test]
    fn a_free_running_counter_is_kept_out_of_the_note_table() {
        let mut lines: Vec<String> = Vec::new();
        for i in 0..12u16 {
            let at = format!("2026-10-03T10:00:{i:02}");
            lines.push(format!(
                "{at}.000+02:00 [{i}.000] RX slave=0x01 func=0x03 (r) \
                 data=00 00 00 02 crc=0x0 computed=0x0 OK"
            ));
            let setpoint: u16 = if i < 6 { 220 } else { 230 };
            lines.push(format!(
                "{at}.010+02:00 [{i}.010] RX slave=0x01 func=0x03 (r) data=04 {:02x} {:02x} \
                 {:02x} {:02x} crc=0x0 computed=0x0 OK",
                i >> 8,
                i & 0xff,
                setpoint >> 8,
                setpoint & 0xff
            ));
        }
        lines.push("2026-10-03T10:00:06.500+02:00 # note [6.500] setpoint 22.0 -> 23.0".into());

        let owned: Vec<&str> = lines.iter().map(String::as_str).collect();
        let cap = parse_lines(owned);
        let md = report(
            &cap,
            &Options {
                window: Duration::from_secs(30),
                ..Options::default()
            },
        );

        // Register 0x0000 steps every poll, 0x0001 moved once.
        assert!(
            md.contains("Free-running (moves on over half the polls"),
            "{md}"
        );
        let section = md
            .split("## 5. Notes correlation")
            .nth(1)
            .expect("notes section");
        assert!(
            section.contains("Register changes in window: 1"),
            "only the setpoint should be listed:\n{section}"
        );
        assert!(
            section.contains("0x00dc (220) | 0x00e6 (230) | +10"),
            "{section}"
        );
        assert!(
            section.contains("free-running registers, not listed: 0x0000"),
            "{section}"
        );
    }

    #[test]
    fn a_tight_window_excludes_the_write() {
        let md = render(LOG, Duration::from_millis(100));
        let section = md
            .split("## 5. Notes correlation")
            .nth(1)
            .expect("notes section");
        assert!(section.contains("Writes in window: none."), "{section}");
    }

    #[test]
    fn register_table_shows_scaled_values() {
        let md = render(LOG, Duration::from_secs(10));
        // Holding 0x0064 ends at 0x00e6 = 230 = 23.0 at one decimal.
        assert!(md.contains("| 0x0064 (100) |"), "{md}");
        assert!(md.contains("| 230 | 230 | 23.0 |"), "{md}");
        // 0x0065 never moved.
        assert!(md.contains("Constant: 1"), "{md}");
    }

    #[test]
    fn exception_is_reported() {
        let md = render(LOG, Duration::from_secs(10));
        assert!(md.contains("Illegal Data Address"), "{md}");
    }

    #[test]
    fn empty_input_still_renders() {
        let md = render(&[], Duration::from_secs(10));
        assert!(md.contains("the log held no timestamped lines"), "{md}");
        assert!(md.contains("No read requests were seen."), "{md}");
    }

    #[test]
    fn gaps_are_listed() {
        let md = render(
            &[
                "2026-10-03T10:00:00.000+02:00 [1.000] RX slave=0x01 func=0x03 (r) data=00 00 00 01 crc=0x0 computed=0x0 OK",
                "2026-10-03T10:05:00.000+02:00 [301.000] RX slave=0x01 func=0x03 (r) data=00 00 00 01 crc=0x0 computed=0x0 OK",
            ],
            Duration::from_secs(10),
        );
        assert!(md.contains("| 5m 0s |"), "{md}");
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(fmt_delta(TimeDelta::milliseconds(1_500)), "1.500s");
        assert_eq!(fmt_delta(TimeDelta::seconds(30)), "30s");
        assert_eq!(fmt_delta(TimeDelta::seconds(90)), "1m 30s");
        assert_eq!(fmt_delta(TimeDelta::seconds(3_600)), "1h 0m 0s");
        assert_eq!(fmt_delta(TimeDelta::days(2)), "2d 0h 0m 0s");
    }

    #[test]
    fn value_formatting() {
        assert_eq!(fmt_tenths(220), "22.0");
        assert_eq!(fmt_tenths(0xffff), "-0.1");
        assert_eq!(fmt_ms(20), "20 ms");
        assert_eq!(fmt_ms(1_500), "1.500 s");
        assert_eq!(fmt_hex(&[]), "(empty)");
        assert_eq!(fmt_hex(&[0x01, 0xab]), "01 ab");
    }
}
