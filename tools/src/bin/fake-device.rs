//! `fake-device` - stand in for the sniffer so the host tools can be exercised
//! with no hardware attached.

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use modbus_sniffer_tools::fake::{self, FakeConfig};
use tokio::net::TcpListener;

#[derive(Parser, Debug)]
#[command(
    name = "fake-device",
    about = "Pretend to be a Modbus RTU sniffer on a TCP port",
    long_about = "Speaks the same line protocol as the firmware: a hello, a replay \
                  burst, then a poll loop of frames built with modbus-sniffer-core's \
                  own formatter. Answers note/bus/status commands."
)]
struct Cli {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:4000")]
    listen: String,
    /// Uptime in milliseconds to claim at startup. Restart with a lower value to
    /// make the capture daemon see a reboot.
    #[arg(long, default_value_t = 1_000)]
    uptime_ms: u64,
    /// Lines to replay on connect.
    #[arg(long, default_value_t = 2)]
    replay: usize,
    /// Poll period, e.g. 200ms.
    #[arg(long, default_value = "200ms")]
    period: String,
    /// Corrupt the CRC of every Nth response (0 disables).
    #[arg(long, default_value_t = 0)]
    bad_crc_every: usize,
    /// Emit a Write Single Register exchange every Nth cycle (0 disables).
    #[arg(long, default_value_t = 0)]
    write_every: usize,
    /// Bus baud rate to report.
    #[arg(long, default_value_t = 9_600)]
    baud: u32,
    /// Bus frame format to report.
    #[arg(long, default_value = "8E1")]
    format: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let period = humantime::parse_duration(&cli.period)
        .with_context(|| format!("--period {:?} is not a duration", cli.period))?;

    let cfg = FakeConfig {
        baud: cli.baud,
        format: cli.format,
        boot_uptime_ms: cli.uptime_ms,
        replay: cli.replay,
        period: period.max(Duration::from_millis(1)),
        bad_crc_every: cli.bad_crc_every,
        write_every: cli.write_every,
        ..FakeConfig::default()
    };

    let listener = TcpListener::bind(&cli.listen)
        .await
        .with_context(|| format!("binding {}", cli.listen))?;
    eprintln!("fake-device: listening on {}", cli.listen);
    fake::serve(listener, cfg).await.context("serving")?;
    Ok(())
}
