//! End-to-end tests for the capture daemon, against the fake device.
//!
//! No hardware and no network beyond loopback: the fake speaks exactly the
//! device protocol and builds its frame lines with `modbus-sniffer-core`'s own
//! formatter.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, FixedOffset};
use modbus_sniffer_tools::capture::{self, CaptureConfig};
use modbus_sniffer_tools::clock::SystemClock;
use modbus_sniffer_tools::device;
use modbus_sniffer_tools::fake::{self, FakeConfig};
use modbus_sniffer_tools::logfile::parse_log_line;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Concatenation of every capture log in `dir`, oldest file first.
fn read_logs(dir: &Path) -> String {
    read_prefixed(dir, "capture-")
}

fn read_prefixed(dir: &Path, prefix: &str) -> String {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".log"))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect::<Vec<_>>()
        .join("")
}

/// Concatenation of every compact log in `dir`.
fn read_compact(dir: &Path) -> String {
    read_prefixed(dir, "compact-")
}

/// Poll until `check` passes, or fail the test after 15 seconds.
async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..750 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn wait_for_log(dir: &Path, needle: &str) {
    wait_for(&format!("{needle:?} in the capture log"), || {
        read_logs(dir).contains(needle)
    })
    .await;
}

/// (wall-clock stamp, device uptime) for every frame line in the log.
fn frame_stamps(body: &str) -> Vec<(DateTime<FixedOffset>, u64)> {
    body.lines()
        .filter_map(parse_log_line)
        .filter(|r| r.rest.contains(" RX "))
        .filter_map(|r| device::uptime_ms(&r.rest).map(|u| (r.time, u)))
        .collect()
}

struct Daemon {
    dir: tempfile::TempDir,
    control: PathBuf,
    handle: JoinHandle<()>,
}

impl Daemon {
    async fn start(device: String) -> Self {
        Self::start_with(device, |_| {}).await
    }

    async fn start_with(device: String, tweak: impl FnOnce(&mut CaptureConfig)) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let control = dir.path().join("ctl.sock");
        let mut cfg = CaptureConfig {
            device,
            dir: dir.path().to_path_buf(),
            control: Some(control.clone()),
            // Keep reconnects quick so the tests do not crawl.
            max_backoff: Duration::from_millis(500),
            ..CaptureConfig::default()
        };
        tweak(&mut cfg);
        let handle = tokio::spawn(async move {
            if let Err(e) = capture::run_with_clock(cfg, SystemClock).await {
                panic!("capture daemon failed: {e:#}");
            }
        });
        let sock = control.clone();
        wait_for("the control socket to appear", || sock.exists()).await;
        Self {
            dir,
            control,
            handle,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn log(&self) -> String {
        read_logs(self.dir.path())
    }

    async fn control(&self, command: &str) -> String {
        capture::control_request(&self.control, command)
            .await
            .expect("control request")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn fast_fake(boot_uptime_ms: u64) -> FakeConfig {
    FakeConfig {
        boot_uptime_ms,
        replay: 2,
        period: Duration::from_millis(40),
        latency: Duration::from_millis(5),
        write_every: 3,
        ..FakeConfig::default()
    }
}

async fn spawn_fake(cfg: FakeConfig) -> (std::net::SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        let _ = fake::serve(listener, cfg).await;
    });
    (addr, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn captures_frames_and_maps_uptime_to_wall_clock() {
    let (addr, fake_task) = spawn_fake(fast_fake(60_000)).await;
    let d = Daemon::start(addr.to_string()).await;

    wait_for_log(d.path(), "CAPTURE connected to").await;
    wait_for("six frame lines", || frame_stamps(&d.log()).len() >= 6).await;
    // The fake writes a register every third poll cycle; the analyzer test
    // depends on those showing up here too.
    wait_for_log(d.path(), "func=0x06").await;

    let body = d.log();

    // Every line is a parseable `<RFC3339> <device line>` record.
    for line in body.lines() {
        let rec = parse_log_line(line).unwrap_or_else(|| panic!("unparseable log line: {line:?}"));
        assert!(!rec.rest.is_empty(), "empty payload in {line:?}");
    }

    // The offset is constant inside one session, so wall-clock deltas have to
    // reproduce device-uptime deltas exactly.
    let stamps = frame_stamps(&body);
    for w in stamps.windows(2) {
        let (t0, u0) = w[0];
        let (t1, u1) = w[1];
        assert_eq!(
            (t1 - t0).num_milliseconds(),
            u1 as i64 - u0 as i64,
            "wall-clock delta does not track uptime delta: {:?} vs {:?}",
            w[0],
            w[1]
        );
    }

    // Replayed lines carry older uptimes, so they must be stamped *before* the
    // hello that arrived after them. Arrival-time stamping would invert this.
    let hello_time = body
        .lines()
        .filter_map(parse_log_line)
        .find(|r| r.rest.starts_with("# hello "))
        .expect("a hello line")
        .time;
    let earliest = stamps.iter().map(|(t, _)| *t).min().expect("a frame");
    assert!(
        earliest < hello_time,
        "replayed frame stamp {earliest} is not before the hello at {hello_time}"
    );

    // A fresh stamp must be near the real wall clock, not at the epoch.
    let newest = stamps.iter().map(|(t, _)| *t).max().expect("a frame");
    let skew = (chrono::Local::now().fixed_offset() - newest)
        .num_seconds()
        .abs();
    assert!(skew < 60, "frame stamped {skew} s away from now");

    // The fake also emits a write exchange, which the analyzer needs.
    assert!(body.contains("func=0x06"), "no write frame in:\n{body}");

    fake_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_a_note_to_the_device() {
    let (addr, fake_task) = spawn_fake(fast_fake(1_000)).await;
    let d = Daemon::start(addr.to_string()).await;
    wait_for_log(d.path(), "CAPTURE connected to").await;

    let reply = d.control("note setpoint 26 -> 27").await;
    assert_eq!(reply, "# ok note", "unexpected reply");

    // The device echo lands in the log with its own uptime stamp.
    wait_for_log(d.path(), "# note [").await;
    let body = d.log();
    assert!(body.contains("setpoint 26 -> 27"), "{body}");
    assert!(
        !body.contains("NOTE setpoint 26 -> 27"),
        "the note should not have been stored locally too:\n{body}"
    );

    fake_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_bus_and_status() {
    let (addr, fake_task) = spawn_fake(fast_fake(1_000)).await;
    let d = Daemon::start(addr.to_string()).await;
    wait_for_log(d.path(), "CAPTURE connected to").await;

    assert_eq!(d.control("bus 19200 8N1").await, "# ok bus");
    let status = d.control("status").await;
    assert!(
        status.starts_with("# status uptime_ms=") && status.contains("bus=19200 8N1"),
        "unexpected status: {status}"
    );

    // The `# bus` line the device announced is in the log, so the analyzer can
    // see the settings change.
    wait_for_log(d.path(), "# bus 19200 8N1").await;

    let bad = d.control("frobnicate").await;
    assert!(bad.starts_with("# err"), "unexpected reply: {bad}");

    fake_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_note_survives_a_disconnected_device() {
    // Nothing listens on port 1, so the connect is refused straight away.
    let d = Daemon::start("127.0.0.1:1".to_string()).await;
    wait_for_log(d.path(), "CAPTURE connect failed").await;

    let reply = d.control("note the device was unplugged").await;
    assert!(reply.starts_with("# err"), "unexpected reply: {reply}");
    assert!(
        reply.contains("capture log"),
        "the reply should say where the note went: {reply}"
    );

    wait_for_log(d.path(), "NOTE the device was unplugged").await;

    // Non-note commands just fail; there is nothing to preserve.
    let status = d.control("status").await;
    assert!(status.starts_with("# err"), "unexpected reply: {status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnects_and_notices_a_reboot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake");
    let addr = listener.local_addr().expect("local_addr");
    let first = tokio::spawn(async move {
        let _ = fake::serve(listener, fast_fake(90_000)).await;
    });

    let d = Daemon::start(addr.to_string()).await;
    wait_for_log(d.path(), "CAPTURE connected to").await;
    wait_for("two frame lines", || frame_stamps(&d.log()).len() >= 2).await;

    // Pull the device away. Aborting `serve` drops both the listener and the
    // live client connection, so the daemon sees a clean EOF.
    first.abort();
    wait_for_log(d.path(), "CAPTURE disconnected").await;

    // Bring it back on the same port, claiming a much lower uptime.
    let listener = TcpListener::bind(addr).await.expect("rebind fake");
    let second = tokio::spawn(async move {
        let _ = fake::serve(listener, fast_fake(500)).await;
    });

    wait_for_log(d.path(), "CAPTURE device rebooted").await;

    let body = d.log();
    assert_eq!(
        body.matches("CAPTURE connected to").count(),
        2,
        "expected two connects in:\n{body}"
    );
    assert!(
        body.contains("CAPTURE uptime offset_ms="),
        "no offset event in:\n{body}"
    );

    // Frames keep coming after the reboot.
    wait_for("frames after the reboot", || {
        let b = d.log();
        let after = b
            .split("CAPTURE device rebooted")
            .nth(1)
            .map(frame_stamps)
            .unwrap_or_default();
        after.len() >= 2
    })
    .await;

    second.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_captured_log_feeds_the_analyzer() {
    let (addr, fake_task) = spawn_fake(fast_fake(1_000)).await;
    let d = Daemon::start(addr.to_string()).await;
    wait_for_log(d.path(), "CAPTURE connected to").await;
    wait_for("ten frame lines", || frame_stamps(&d.log()).len() >= 10).await;
    let _ = d.control("note changed the setpoint").await;
    wait_for_log(d.path(), "# note [").await;
    wait_for("twenty frame lines", || frame_stamps(&d.log()).len() >= 20).await;
    fake_task.abort();

    let body = d.log();
    let md = modbus_sniffer_tools::analyze::analyze_lines(
        body.lines(),
        &modbus_sniffer_tools::analyze::Options::default(),
    );

    assert!(md.contains("## 1. Overview"), "{md}");
    assert!(md.contains("| 9600 | 8E1 |"), "{md}");
    assert!(
        md.contains("0x01 Read Holding Registers") || md.contains("0x03 Read Holding Registers"),
        "{md}"
    );
    assert!(md.contains("changed the setpoint"), "{md}");
    assert!(
        md.contains("Write Single Register"),
        "the fake's writes should show up:\n{md}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_writes_a_second_smaller_log() {
    // The fake's second register is a counter that steps every cycle, so this
    // is also the deadband path: with a wide enough band the whole poll
    // collapses to the pair the first cycle wrote.
    let (addr, fake_task) = spawn_fake(FakeConfig {
        boot_uptime_ms: 1_000,
        replay: 0,
        period: Duration::from_millis(40),
        latency: Duration::from_millis(5),
        write_every: 0,
        ..FakeConfig::default()
    })
    .await;
    let d = Daemon::start_with(addr.to_string(), |cfg| {
        cfg.compact = Some(modbus_sniffer_tools::compact::CompactConfig {
            snapshot: Duration::ZERO,
            gap: Duration::ZERO,
            deadband: [(0x0001, 10_000)].into_iter().collect(),
            ..Default::default()
        });
    })
    .await;

    wait_for_log(d.path(), "CAPTURE connected to").await;
    wait_for("thirty raw frame lines", || {
        frame_stamps(&d.log()).len() >= 30
    })
    .await;
    wait_for("the compact log to exist", || {
        read_compact(d.path()).contains("COMPACT poll")
    })
    .await;
    fake_task.abort();

    let raw = d.log();
    let compact = read_compact(d.path());
    let raw_frames = frame_stamps(&raw).len();
    let compact_frames = frame_stamps(&compact).len();
    assert!(
        compact_frames * 4 < raw_frames,
        "compaction should be a big win: {compact_frames} of {raw_frames} frames"
    );
    assert!(
        compact.contains("COMPACT begin deadband=0x0001:10000"),
        "{compact}"
    );
    // The daemon's own events and the hello survive into the compact log.
    assert!(compact.contains("CAPTURE connected to"), "{compact}");
    assert!(compact.contains("# hello "), "{compact}");
    // Every compact line is still a valid capture-log record.
    for line in compact.lines() {
        parse_log_line(line).unwrap_or_else(|| panic!("unparseable compact line: {line:?}"));
    }
    // And the analyzer reads it without complaining about unknown lines.
    let md = modbus_sniffer_tools::analyze::analyze_lines(
        compact.lines(),
        &modbus_sniffer_tools::analyze::Options::default(),
    );
    assert!(md.contains("- Unparsed lines: 0"), "{md}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_raw_writes_only_the_compact_log() {
    let (addr, fake_task) = spawn_fake(fast_fake(1_000)).await;
    let d = Daemon::start_with(addr.to_string(), |cfg| {
        cfg.raw = false;
        cfg.compact = Some(modbus_sniffer_tools::compact::CompactConfig::default());
    })
    .await;
    wait_for("the compact log to fill", || {
        read_compact(d.path()).contains("CAPTURE connected to")
    })
    .await;
    fake_task.abort();
    assert!(d.log().is_empty(), "no raw log should have been created");
}
