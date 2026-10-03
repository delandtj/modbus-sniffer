//! The capture daemon: one TCP connection to the sniffer, one log file per day,
//! one unix control socket for the CLI.

use std::collections::VecDeque;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::clock::{epoch_millis, from_epoch_millis, Clock, SystemClock};
use crate::device;
use crate::logfile::LogWriter;

/// Default path of the control socket.
pub const DEFAULT_CONTROL_PATH: &str = "/run/modbus-sniffer/ctl.sock";
/// Default log directory.
pub const DEFAULT_LOG_DIR: &str = "/var/lib/modbus-sniffer";
/// Default device address.
pub const DEFAULT_DEVICE: &str = "modbus-sniffer:4000";

/// Commands the control socket forwards to the device.
pub const CONTROL_VERBS: [&str; 3] = ["note", "bus", "status"];

/// How long a single TCP connect attempt may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the device gets to answer a forwarded command.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a control client waits in total (queueing + device reply).
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
/// First reconnect delay; doubles up to `max_backoff`.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Commands allowed in flight at once.
const MAX_PENDING: usize = 8;

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// `host:port` of the sniffer.
    pub device: String,
    /// Directory the daily log files go in.
    pub dir: PathBuf,
    /// Control socket to listen on, or `None` for no control socket.
    pub control: Option<PathBuf>,
    /// Upper bound on the reconnect backoff.
    pub max_backoff: Duration,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            device: DEFAULT_DEVICE.to_string(),
            dir: PathBuf::from(DEFAULT_LOG_DIR),
            control: Some(PathBuf::from(DEFAULT_CONTROL_PATH)),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// A command on its way to the device, with somewhere to put the reply.
struct DeviceRequest {
    command: String,
    reply: oneshot::Sender<Result<String, String>>,
}

/// Clone-able handle on the log writer task.
#[derive(Clone)]
pub struct LogSink {
    tx: mpsc::UnboundedSender<(DateTime<Local>, String)>,
}

impl LogSink {
    /// Queue one record. Dropping it on the floor when the writer is gone is
    /// correct: the process is shutting down.
    pub fn send(&self, stamp: DateTime<Local>, text: &str) {
        let _ = self.tx.send((stamp, text.to_string()));
    }
}

/// What we know about the device between reconnects.
#[derive(Debug, Default)]
struct DeviceState {
    /// `wall_clock_ms - uptime_ms`, learned from the current session's hello.
    offset_ms: Option<i64>,
    /// Highest uptime seen so far, for reboot detection.
    last_uptime_ms: Option<u64>,
}

/// Run the daemon with the real clock. Never returns on its own.
pub async fn run(cfg: CaptureConfig) -> Result<()> {
    run_with_clock(cfg, SystemClock).await
}

/// Run the daemon against an injected clock (used by the tests).
pub async fn run_with_clock<C: Clock + Clone>(cfg: CaptureConfig, clock: C) -> Result<()> {
    std::fs::create_dir_all(&cfg.dir)
        .with_context(|| format!("creating log directory {}", cfg.dir.display()))?;

    // The writer uses blocking std::fs on a tokio task. At Modbus RTU rates the
    // line rate is a few hundred per second at most, so a flush per line costs
    // far less than the complexity of a spawn_blocking round trip per frame.
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<(DateTime<Local>, String)>();
    let sink = LogSink { tx: log_tx };
    let mut writer = LogWriter::new(cfg.dir.clone(), clock.clone());
    tokio::spawn(async move {
        while let Some((stamp, text)) = log_rx.recv().await {
            if let Err(e) = writer.write(stamp, &text) {
                eprintln!("capture: log write failed: {e}");
            }
        }
    });

    let (req_tx, mut req_rx) = mpsc::channel::<DeviceRequest>(64);
    if let Some(path) = &cfg.control {
        let listener = bind_control(path)
            .with_context(|| format!("binding control socket {}", path.display()))?;
        sink.send(
            clock.now(),
            &format!("CAPTURE control socket at {}", path.display()),
        );
        tokio::spawn(control_server(
            listener,
            req_tx.clone(),
            sink.clone(),
            clock.clone(),
        ));
    }
    // Hold a sender so `req_rx.recv()` never resolves to `None`.
    let _keepalive = req_tx;

    sink.send(
        clock.now(),
        &format!("CAPTURE started, device {}", cfg.device),
    );

    let mut state = DeviceState::default();
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&cfg.device)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                sink.send(clock.now(), &format!("CAPTURE connected to {}", cfg.device));
                backoff = INITIAL_BACKOFF;
                state.offset_ms = None;
                match session(stream, &mut req_rx, &sink, &clock, &mut state).await {
                    Ok(()) => sink.send(
                        clock.now(),
                        "CAPTURE disconnected (device closed the connection)",
                    ),
                    Err(e) => sink.send(clock.now(), &format!("CAPTURE disconnected: {e}")),
                }
            }
            Ok(Err(e)) => sink.send(clock.now(), &format!("CAPTURE connect failed: {e}")),
            Err(_) => sink.send(
                clock.now(),
                &format!("CAPTURE connect to {} timed out", cfg.device),
            ),
        }

        drain_while_disconnected(&mut req_rx, &sink, &clock, backoff).await;
        backoff = (backoff * 2).min(cfg.max_backoff);
    }
}

/// Read one connection to exhaustion, forwarding commands while it lasts.
async fn session<C: Clock>(
    stream: TcpStream,
    req_rx: &mut mpsc::Receiver<DeviceRequest>,
    sink: &LogSink,
    clock: &C,
    state: &mut DeviceState,
) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let mut pending: VecDeque<(oneshot::Sender<Result<String, String>>, Instant)> = VecDeque::new();

    loop {
        let next_deadline = pending.front().map(|(_, d)| *d);
        tokio::select! {
            line = lines.next_line() => {
                match line.context("reading from the device")? {
                    None => return Ok(()),
                    Some(l) => handle_device_line(&l, sink, clock, state, &mut pending),
                }
            }
            req = req_rx.recv(), if pending.len() < MAX_PENDING => {
                // `_keepalive` in `run` keeps this from ever being `None`.
                let Some(req) = req else { return Ok(()) };
                let mut out = req.command.clone();
                out.push('\n');
                if let Err(e) = wr.write_all(out.as_bytes()).await {
                    let _ = req.reply.send(Err(format!("write to device failed: {e}")));
                    return Err(e).context("writing a command to the device");
                }
                let _ = wr.flush().await;
                pending.push_back((req.reply, Instant::now() + REPLY_TIMEOUT));
            }
            () = wait_until(next_deadline) => {
                if let Some((reply, _)) = pending.pop_front() {
                    let _ = reply.send(Err(
                        "the device did not reply in time".to_string(),
                    ));
                }
            }
        }
    }
}

/// Sleep until `deadline`, or forever when there is nothing to wait for.
async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

fn handle_device_line<C: Clock>(
    raw: &str,
    sink: &LogSink,
    clock: &C,
    state: &mut DeviceState,
    pending: &mut VecDeque<(oneshot::Sender<Result<String, String>>, Instant)>,
) {
    let line = raw.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return;
    }
    let arrival = clock.now();

    if let Some(hello) = device::parse_hello(line) {
        if state
            .last_uptime_ms
            .is_some_and(|prev| hello.uptime_ms < prev)
        {
            sink.send(
                arrival,
                &format!(
                    "CAPTURE device rebooted (uptime_ms {} -> {})",
                    state.last_uptime_ms.unwrap_or(0),
                    hello.uptime_ms
                ),
            );
        }
        let offset = epoch_millis(arrival) - hello.uptime_ms as i64;
        state.offset_ms = Some(offset);
        state.last_uptime_ms = Some(hello.uptime_ms);
        sink.send(arrival, line);
        sink.send(
            arrival,
            &format!(
                "CAPTURE uptime offset_ms={offset} firmware={} version={}",
                hello.firmware, hello.version
            ),
        );
        return;
    }

    let uptime = device::uptime_ms(line);
    let stamp = match (uptime, state.offset_ms) {
        (Some(u), Some(off)) => from_epoch_millis(off + u as i64).unwrap_or(arrival),
        _ => arrival,
    };
    if let Some(u) = uptime {
        state.last_uptime_ms = Some(state.last_uptime_ms.map_or(u, |prev| prev.max(u)));
    }
    sink.send(stamp, line);

    if is_command_reply(line) {
        if let Some((reply, _)) = pending.pop_front() {
            let _ = reply.send(Ok(line.to_string()));
        }
    }
}

fn is_command_reply(line: &str) -> bool {
    line.starts_with("# ok") || line.starts_with("# err") || line.starts_with("# status")
}

/// While there is no device, answer control requests immediately instead of
/// leaving the CLI hanging for the whole backoff.
async fn drain_while_disconnected<C: Clock>(
    req_rx: &mut mpsc::Receiver<DeviceRequest>,
    sink: &LogSink,
    clock: &C,
    backoff: Duration,
) {
    let _ = (sink, clock);
    let deadline = Instant::now() + backoff;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return,
            req = req_rx.recv() => match req {
                Some(req) => {
                    let _ = req.reply.send(Err("the device is not connected".to_string()));
                }
                None => {
                    tokio::time::sleep_until(deadline).await;
                    return;
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Control socket
// ---------------------------------------------------------------------------

/// Bind the control socket, clearing a stale one left by a crashed daemon.
fn bind_control(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_socket() {
            std::fs::remove_file(path)?;
        } else {
            anyhow::bail!("{} exists and is not a socket", path.display());
        }
    }
    let listener = UnixListener::bind(path)?;
    // 0o660 so a dedicated group can run the CLI without being root.
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o660);
    std::fs::set_permissions(path, perms)?;
    Ok(listener)
}

async fn control_server<C: Clock + Clone>(
    listener: UnixListener,
    req_tx: mpsc::Sender<DeviceRequest>,
    sink: LogSink,
    clock: C,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle_control_client(
                    stream,
                    req_tx.clone(),
                    sink.clone(),
                    clock.clone(),
                ));
            }
            Err(e) => {
                eprintln!("capture: control accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle_control_client<C: Clock>(
    stream: UnixStream,
    req_tx: mpsc::Sender<DeviceRequest>,
    sink: LogSink,
    clock: C,
) {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let reply = dispatch(&line, &req_tx, &sink, &clock).await;
        if wr.write_all(reply.as_bytes()).await.is_err() || wr.write_all(b"\n").await.is_err() {
            return;
        }
        let _ = wr.flush().await;
    }
}

/// Forward one control command and render the single-line answer.
async fn dispatch<C: Clock>(
    line: &str,
    req_tx: &mpsc::Sender<DeviceRequest>,
    sink: &LogSink,
    clock: &C,
) -> String {
    let verb = line.split_whitespace().next().unwrap_or("");
    if !CONTROL_VERBS.contains(&verb) {
        return format!(
            "# err unsupported command {verb:?}; expected one of {}",
            CONTROL_VERBS.join(", ")
        );
    }

    let (tx, rx) = oneshot::channel();
    let queued = req_tx
        .send(DeviceRequest {
            command: line.to_string(),
            reply: tx,
        })
        .await
        .is_ok();

    let result = if queued {
        match tokio::time::timeout(CONTROL_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("the capture daemon dropped the request".to_string()),
            Err(_) => Err("timed out waiting for the device".to_string()),
        }
    } else {
        Err("the capture daemon is shutting down".to_string())
    };

    match result {
        Ok(reply) => reply,
        Err(err) => {
            if verb == "note" {
                // A note is a human observation that cannot be recreated later,
                // so it goes into the log even when the device is unreachable.
                let text = line.strip_prefix("note").unwrap_or("").trim();
                sink.send(clock.now(), &format!("NOTE {text}"));
                format!("# err {err} - note written to the capture log instead")
            } else {
                format!("# err {err}")
            }
        }
    }
}

/// Send one command to a running daemon's control socket and read the answer.
pub async fn control_request(path: &Path, command: &str) -> Result<String> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to the control socket {}", path.display()))?;
    let (rd, mut wr) = stream.into_split();
    wr.write_all(command.as_bytes()).await?;
    wr.write_all(b"\n").await?;
    wr.flush().await?;
    let mut lines = BufReader::new(rd).lines();
    match tokio::time::timeout(CONTROL_TIMEOUT + Duration::from_secs(2), lines.next_line()).await {
        Ok(Ok(Some(line))) => Ok(line),
        Ok(Ok(None)) => {
            anyhow::bail!("the capture daemon closed the control socket without a reply")
        }
        Ok(Err(e)) => Err(e).context("reading the control reply"),
        Err(_) => anyhow::bail!("the capture daemon did not answer in time"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_replies_are_recognised() {
        assert!(is_command_reply("# ok note"));
        assert!(is_command_reply("# err bad baud"));
        assert!(is_command_reply("# status uptime_ms=1 bus=9600 8E1"));
        assert!(!is_command_reply("# note [1.000] hello"));
        assert!(!is_command_reply("# hello sniffer 1 uptime_ms=1"));
        assert!(!is_command_reply("[1.000] RX slave=0x01 func=0x03"));
    }
}
