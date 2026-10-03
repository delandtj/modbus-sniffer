//! A fake sniffer device, so the daemon and the analyzer can be exercised with
//! no hardware on the bench.
//!
//! It speaks exactly the protocol the firmware speaks, and it builds its frame
//! lines with `modbus-sniffer-core`'s own `build_frame` + `format_frame`, so the
//! bytes on the wire cannot drift from what the firmware would emit.

use std::time::Duration;

use modbus_sniffer_core as core;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct FakeConfig {
    pub firmware: String,
    pub version: String,
    pub baud: u32,
    pub format: String,
    /// Uptime in milliseconds the fake claims at the moment `serve` is called.
    /// Set it lower than a previous run's to simulate a reboot.
    pub boot_uptime_ms: u64,
    /// Lines to replay from the "while nobody was connected" buffer on connect.
    pub replay: usize,
    /// Interval between poll cycles.
    pub period: Duration,
    /// Response latency within a poll cycle.
    pub latency: Duration,
    /// Corrupt the CRC of every Nth response (0 disables).
    pub bad_crc_every: usize,
    /// Emit a `Write Single Register` exchange every Nth cycle (0 disables).
    pub write_every: usize,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            firmware: "sniffer-esp32c6".to_string(),
            version: "0.1.0-fake".to_string(),
            baud: 9_600,
            format: "8E1".to_string(),
            boot_uptime_ms: 1_000,
            replay: 2,
            period: Duration::from_millis(200),
            latency: Duration::from_millis(15),
            bad_crc_every: 0,
            write_every: 0,
        }
    }
}

/// Format one frame line exactly as the firmware would.
///
/// `pdu` is address + function + body; the CRC is appended. With `corrupt` the
/// last CRC byte is flipped, which is how a BAD_CRC line is produced.
pub fn frame_line(pdu: &[u8], uptime_ms: u64, corrupt: bool) -> String {
    let mut raw = core::build_frame(pdu);
    if corrupt {
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
    }
    let crc_ok = !corrupt;
    let frame = core::Frame {
        timestamp_ms: uptime_ms,
        data: raw,
        crc_ok,
    };
    let mut line = core::Line::new();
    core::format_frame(&frame, &mut line).expect("a 8-byte frame fits in Line");
    line.as_str().to_string()
}

/// Aborts the task it holds when dropped, so the client connection really goes
/// away both when a new client arrives and when `serve` itself is cancelled.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Serve forever. A new connection replaces the old one, like the firmware.
pub async fn serve(listener: TcpListener, cfg: FakeConfig) -> std::io::Result<()> {
    let started = Instant::now();
    let mut current: Option<AbortOnDrop> = None;
    loop {
        let (stream, _) = listener.accept().await?;
        let cfg = cfg.clone();
        let guard = AbortOnDrop(tokio::spawn(async move {
            let _ = client(stream, cfg, started).await;
        }));
        // Dropping the previous guard is what hangs up on the old client.
        drop(current.replace(guard));
    }
}

async fn client(stream: TcpStream, cfg: FakeConfig, started: Instant) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let mut baud = cfg.baud;
    let mut format = cfg.format.clone();
    let uptime = |started: Instant| cfg.boot_uptime_ms + started.elapsed().as_millis() as u64;

    let now = uptime(started);
    let hello = format!(
        "# hello {} {} uptime_ms={now} bus={baud} {format} replay={}\r\n",
        cfg.firmware, cfg.version, cfg.replay
    );
    wr.write_all(hello.as_bytes()).await?;

    // Replayed lines carry their original (older) uptime stamps.
    let step = cfg.period.as_millis() as u64;
    for i in 0..cfg.replay {
        let back = step * (cfg.replay - i) as u64;
        let stamp = now.saturating_sub(back);
        let line = frame_line(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x04], stamp, false);
        wr.write_all(line.as_bytes()).await?;
    }
    wr.flush().await?;

    let mut cycle: usize = 0;
    let mut counter: u16 = 0;
    let mut setpoint: u16 = 220;
    let mut ticker = tokio::time::interval(cfg.period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            incoming = lines.next_line() => {
                let Some(cmd) = incoming? else { return Ok(()) };
                let cmd = cmd.trim();
                if cmd.is_empty() {
                    continue;
                }
                let reply = handle_command(cmd, uptime(started), &mut baud, &mut format);
                wr.write_all(reply.as_bytes()).await?;
                wr.flush().await?;
            }
            _ = ticker.tick() => {
                cycle += 1;
                counter = counter.wrapping_add(1);

                let req = [0x01, 0x03, 0x00, 0x00, 0x00, 0x04];
                wr.write_all(frame_line(&req, uptime(started), false).as_bytes()).await?;
                wr.flush().await?;
                tokio::time::sleep(cfg.latency).await;

                let corrupt = cfg.bad_crc_every != 0 && cycle.is_multiple_of(cfg.bad_crc_every);
                let mut resp = vec![0x01, 0x03, 0x08];
                for v in [setpoint, counter, 0x0001, 0x0000] {
                    resp.extend_from_slice(&v.to_be_bytes());
                }
                wr.write_all(frame_line(&resp, uptime(started), corrupt).as_bytes()).await?;
                wr.flush().await?;

                if cfg.write_every != 0 && cycle.is_multiple_of(cfg.write_every) {
                    setpoint = setpoint.wrapping_add(10);
                    let mut w = vec![0x01, 0x06, 0x00, 0x64];
                    w.extend_from_slice(&setpoint.to_be_bytes());
                    wr.write_all(frame_line(&w, uptime(started), false).as_bytes()).await?;
                    wr.flush().await?;
                    tokio::time::sleep(cfg.latency).await;
                    wr.write_all(frame_line(&w, uptime(started), false).as_bytes()).await?;
                    wr.flush().await?;
                }
            }
        }
    }
}

fn handle_command(cmd: &str, uptime_ms: u64, baud: &mut u32, format: &mut String) -> String {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    match tokens.first().copied() {
        Some("status") => {
            format!("# status uptime_ms={uptime_ms} bus={baud} {format} clients=1\r\n")
        }
        Some("note") => {
            let text = cmd.strip_prefix("note").unwrap_or("").trim();
            let secs = uptime_ms / 1000;
            let ms = uptime_ms % 1000;
            format!("# note [{secs}.{ms:03}] {text}\r\n# ok note\r\n")
        }
        Some("bus") => match (
            tokens.get(1).and_then(|t| t.parse::<u32>().ok()),
            tokens.get(2).copied(),
        ) {
            (Some(b), Some(f)) if crate::device::is_frame_format(f) => {
                *baud = b;
                *format = f.to_string();
                format!("# bus {b} {f}\r\n# ok bus\r\n")
            }
            _ => "# err bus <baud> <8N1|8E1|8O1|8N2>\r\n".to_string(),
        },
        _ => format!("# err unknown command {cmd:?}\r\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{parse_line, DeviceLine};

    #[test]
    fn frame_line_is_parseable_and_ok() {
        let line = frame_line(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x0a], 12_345, false);
        assert!(line.ends_with("\r\n"));
        match parse_line(&line) {
            DeviceLine::Frame(f) => {
                assert!(f.crc_ok);
                assert_eq!(f.body, vec![0x00, 0x00, 0x00, 0x0a]);
            }
            other => panic!("expected frame, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_frame_line_is_bad_crc() {
        let line = frame_line(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x0a], 1, true);
        match parse_line(&line) {
            DeviceLine::Frame(f) => assert!(!f.crc_ok),
            other => panic!("expected frame, got {other:?}"),
        }
    }

    #[test]
    fn commands_are_answered() {
        let mut baud = 9_600;
        let mut format = "8E1".to_string();
        assert!(handle_command("status", 5_000, &mut baud, &mut format)
            .starts_with("# status uptime_ms=5000 bus=9600 8E1"));
        assert_eq!(
            handle_command("note hello there", 7_002, &mut baud, &mut format),
            "# note [7.002] hello there\r\n# ok note\r\n"
        );
        assert_eq!(
            handle_command("bus 19200 8N1", 1, &mut baud, &mut format),
            "# bus 19200 8N1\r\n# ok bus\r\n"
        );
        assert_eq!(baud, 19_200);
        assert_eq!(format, "8N1");
        assert!(handle_command("bus 19200 nope", 1, &mut baud, &mut format).starts_with("# err"));
        assert!(handle_command("frobnicate", 1, &mut baud, &mut format).starts_with("# err"));
    }
}
