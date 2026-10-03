//! Who asked and who answered.
//!
//! A passive single-wire sniff gives no direction bit, so roles have to be
//! inferred from shape and order: the PDU length rules of each function code,
//! plus "a response follows its request within about a second, on the same slave
//! address and the same function code".

use chrono::TimeDelta;

use super::parse::{Observed, Time};

/// What a PDU looks like it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Request,
    Response,
    /// An exception response (function code with bit 7 set).
    Exception,
    /// Request and response are byte-identical (0x05, 0x06, 0x08, 0x16) or the
    /// lengths collide, so only the surrounding traffic can tell them apart.
    Ambiguous,
    /// Does not match any known shape for this function code.
    Unknown,
}

/// Shape-only classification of one PDU.
pub fn classify(func: u8, body: &[u8]) -> Role {
    if func & 0x80 != 0 {
        return Role::Exception;
    }
    let n = body.len();
    match func {
        // Reads: a request is start(2) + quantity(2); a response is a byte
        // count followed by that many bytes. A 3-byte coil response has the
        // same length as a request, hence Ambiguous.
        0x01..=0x04 => {
            let looks_like_response = n >= 2 && body[0] as usize == n - 1;
            match (n == 4, looks_like_response) {
                (true, true) => Role::Ambiguous,
                (true, false) => Role::Request,
                (false, true) => Role::Response,
                (false, false) => Role::Unknown,
            }
        }
        // Single writes echo the request back verbatim.
        0x05 | 0x06 => {
            if n == 4 {
                Role::Ambiguous
            } else {
                Role::Unknown
            }
        }
        // Multiple writes: request is start(2) + qty(2) + bytecount(1) + data;
        // the response is just start(2) + qty(2).
        0x0f | 0x10 => {
            if n >= 6 && body[4] as usize == n - 5 {
                Role::Request
            } else if n == 4 {
                Role::Response
            } else {
                Role::Unknown
            }
        }
        0x07 => match n {
            0 => Role::Request,
            1 => Role::Response,
            _ => Role::Unknown,
        },
        0x0b | 0x0c | 0x11 => {
            if n == 0 {
                Role::Request
            } else {
                Role::Response
            }
        }
        0x08 => {
            if n == 4 {
                Role::Ambiguous
            } else {
                Role::Unknown
            }
        }
        0x16 => {
            if n == 6 {
                Role::Ambiguous
            } else {
                Role::Unknown
            }
        }
        _ => Role::Unknown,
    }
}

/// One request and, if it came, its answer.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub request: Observed,
    pub response: Option<Observed>,
}

impl Exchange {
    pub fn is_exception(&self) -> bool {
        self.response.as_ref().is_some_and(|r| r.is_exception())
    }

    /// Request to response delay, if there was a response.
    pub fn latency(&self) -> Option<TimeDelta> {
        self.response.as_ref().map(|r| r.time - self.request.time)
    }
}

#[derive(Debug, Default)]
pub struct Paired {
    pub exchanges: Vec<Exchange>,
    /// Responses with no request in front of them.
    pub orphan_responses: Vec<Observed>,
    /// Frames whose shape matches no known function/length combination.
    pub unknown: Vec<Observed>,
}

impl Paired {
    pub fn timeouts(&self) -> impl Iterator<Item = &Exchange> {
        self.exchanges.iter().filter(|e| e.response.is_none())
    }
}

/// Pair requests with responses.
///
/// `max_latency` is how long a request stays open; the Modbus default turnaround
/// on a slow 9600 bus is well under a second.
pub fn pair(frames: &[Observed], max_latency: TimeDelta) -> Paired {
    let mut out = Paired::default();
    // Indices into `out.exchanges` still waiting for a response, oldest first.
    let mut open: Vec<usize> = Vec::new();

    for f in frames {
        expire(&mut open, &out.exchanges, f.time, max_latency);

        let role = classify(f.func, &f.body);
        let matched = match role {
            Role::Response | Role::Exception | Role::Ambiguous => {
                take_open(&mut open, &out.exchanges, f, max_latency)
            }
            _ => None,
        };

        if let Some(idx) = matched {
            out.exchanges[idx].response = Some(f.clone());
            continue;
        }

        match role {
            Role::Request | Role::Ambiguous => out.exchanges.push(Exchange {
                request: f.clone(),
                response: None,
            }),
            Role::Response | Role::Exception => out.orphan_responses.push(f.clone()),
            Role::Unknown => out.unknown.push(f.clone()),
        }
        if matches!(role, Role::Request | Role::Ambiguous) {
            open.push(out.exchanges.len() - 1);
        }
    }

    out
}

fn expire(open: &mut Vec<usize>, exchanges: &[Exchange], now: Time, max_latency: TimeDelta) {
    while let Some(&idx) = open.first() {
        if now - exchanges[idx].request.time > max_latency {
            open.remove(0);
        } else {
            break;
        }
    }
}

/// Newest open request on the same slave and function, within the window.
fn take_open(
    open: &mut Vec<usize>,
    exchanges: &[Exchange],
    resp: &Observed,
    max_latency: TimeDelta,
) -> Option<usize> {
    let want_func = resp.base_func();
    let pos = open.iter().rposition(|&idx| {
        let req = &exchanges[idx].request;
        req.slave == resp.slave
            && req.func == want_func
            && resp.time >= req.time
            && resp.time - req.time <= max_latency
    })?;
    Some(open.remove(pos))
}

#[cfg(test)]
mod tests {
    use super::super::parse::parse_lines;
    use super::*;

    fn frames(lines: &[&str]) -> Vec<Observed> {
        parse_lines(lines.iter().copied()).frames
    }

    #[test]
    fn read_shapes() {
        // 0x03 request: start + quantity.
        assert_eq!(classify(0x03, &[0x00, 0x00, 0x00, 0x0a]), Role::Request);
        // 0x03 response: byte count then that many bytes.
        assert_eq!(classify(0x03, &[0x04, 1, 2, 3, 4]), Role::Response);
        // A 3-byte coil response is indistinguishable from a request by length.
        assert_eq!(classify(0x01, &[0x03, 1, 2, 3]), Role::Ambiguous);
        assert_eq!(classify(0x03, &[0x04, 1, 2]), Role::Unknown);
    }

    #[test]
    fn write_shapes() {
        assert_eq!(classify(0x06, &[0x00, 0x64, 0x00, 0xdc]), Role::Ambiguous);
        assert_eq!(
            classify(0x10, &[0x00, 0x64, 0x00, 0x02, 0x04, 1, 2, 3, 4]),
            Role::Request
        );
        assert_eq!(classify(0x10, &[0x00, 0x64, 0x00, 0x02]), Role::Response);
        assert_eq!(classify(0x83, &[0x02]), Role::Exception);
    }

    #[test]
    fn pairs_a_read_exchange() {
        let f = frames(&[
            "2026-10-03T10:00:00.000+02:00 [1.000] RX slave=0x01 func=0x03 (r) data=00 00 00 02 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.020+02:00 [1.020] RX slave=0x01 func=0x03 (r) data=04 00 01 00 02 crc=0x0 computed=0x0 OK",
        ]);
        let p = pair(&f, TimeDelta::seconds(1));
        assert_eq!(p.exchanges.len(), 1);
        assert!(p.exchanges[0].response.is_some());
        assert_eq!(p.exchanges[0].latency(), Some(TimeDelta::milliseconds(20)));
        assert!(p.orphan_responses.is_empty());
    }

    #[test]
    fn a_late_response_does_not_pair() {
        let f = frames(&[
            "2026-10-03T10:00:00.000+02:00 [1.000] RX slave=0x01 func=0x03 (r) data=00 00 00 02 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:03.000+02:00 [4.000] RX slave=0x01 func=0x03 (r) data=04 00 01 00 02 crc=0x0 computed=0x0 OK",
        ]);
        let p = pair(&f, TimeDelta::seconds(1));
        assert_eq!(p.exchanges.len(), 1);
        assert!(p.exchanges[0].response.is_none());
        assert_eq!(p.orphan_responses.len(), 1);
        assert_eq!(p.timeouts().count(), 1);
    }

    #[test]
    fn a_different_slave_does_not_pair() {
        let f = frames(&[
            "2026-10-03T10:00:00.000+02:00 [1.000] RX slave=0x01 func=0x03 (r) data=00 00 00 02 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.020+02:00 [1.020] RX slave=0x02 func=0x03 (r) data=04 00 01 00 02 crc=0x0 computed=0x0 OK",
        ]);
        let p = pair(&f, TimeDelta::seconds(1));
        assert_eq!(p.timeouts().count(), 1);
        assert_eq!(p.orphan_responses.len(), 1);
    }

    #[test]
    fn echo_writes_resolve_by_order() {
        let f = frames(&[
            "2026-10-03T10:00:00.000+02:00 [1.000] RX slave=0x01 func=0x06 (w) data=00 64 00 dc crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.030+02:00 [1.030] RX slave=0x01 func=0x06 (w) data=00 64 00 dc crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:05.000+02:00 [6.000] RX slave=0x01 func=0x06 (w) data=00 64 00 e6 crc=0x0 computed=0x0 OK",
        ]);
        let p = pair(&f, TimeDelta::seconds(1));
        assert_eq!(p.exchanges.len(), 2);
        assert!(p.exchanges[0].response.is_some());
        // The third frame is a new request with no answer in the log.
        assert!(p.exchanges[1].response.is_none());
    }

    #[test]
    fn exception_pairs_with_its_request() {
        let f = frames(&[
            "2026-10-03T10:00:00.000+02:00 [1.000] RX slave=0x01 func=0x03 (r) data=00 ff 00 02 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.010+02:00 [1.010] RX slave=0x01 func=0x83 (e) exception=0x02 (Illegal Data Address) crc=0x0 computed=0x0 OK",
        ]);
        let p = pair(&f, TimeDelta::seconds(1));
        assert_eq!(p.exchanges.len(), 1);
        assert!(p.exchanges[0].is_exception());
    }

    #[test]
    fn interleaved_slaves_pair_independently() {
        let f = frames(&[
            "2026-10-03T10:00:00.000+02:00 [1.000] RX slave=0x01 func=0x03 (r) data=00 00 00 01 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.005+02:00 [1.005] RX slave=0x02 func=0x03 (r) data=00 10 00 01 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.010+02:00 [1.010] RX slave=0x02 func=0x03 (r) data=02 00 07 crc=0x0 computed=0x0 OK",
            "2026-10-03T10:00:00.020+02:00 [1.020] RX slave=0x01 func=0x03 (r) data=02 00 09 crc=0x0 computed=0x0 OK",
        ]);
        let p = pair(&f, TimeDelta::seconds(1));
        assert_eq!(p.exchanges.len(), 2);
        assert!(p.exchanges.iter().all(|e| e.response.is_some()));
        assert_eq!(p.exchanges[0].request.slave, 0x01);
        assert_eq!(
            p.exchanges[0].response.as_ref().expect("resp").body,
            vec![0x02, 0x00, 0x09]
        );
    }
}
