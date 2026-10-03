//! `sniffer-capture` - the capture daemon and its control CLI.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use modbus_sniffer_tools::archive::DEFAULT_RAW_KEEP_DAYS;
use modbus_sniffer_tools::capture::{
    self, CaptureConfig, DEFAULT_CONTROL_PATH, DEFAULT_DEVICE, DEFAULT_LOG_DIR,
};
use modbus_sniffer_tools::compact::{self, CompactConfig, Compactor};

#[derive(Parser, Debug)]
#[command(
    name = "sniffer-capture",
    about = "Capture the Modbus RTU sniffer's output to daily log files",
    long_about = "Run the long-lived capture daemon, or talk to a running one over \
                  its unix control socket to drop a note into the log, change the \
                  bus settings, or ask for status."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Connect to the sniffer and log everything it says, forever.
    Run {
        /// `host:port` of the sniffer.
        #[arg(long, default_value = DEFAULT_DEVICE)]
        device: String,
        /// Directory for the `capture-YYYY-MM-DD.log` files.
        #[arg(long, default_value = DEFAULT_LOG_DIR)]
        dir: PathBuf,
        /// Unix control socket to listen on.
        #[arg(long, default_value = DEFAULT_CONTROL_PATH)]
        control: PathBuf,
        /// Do not listen on a control socket at all.
        #[arg(long, conflicts_with = "control")]
        no_control: bool,
        /// Upper bound on the reconnect backoff, in seconds.
        #[arg(long, default_value_t = 30)]
        max_backoff_secs: u64,
        /// Also write a deduplicated `compact-YYYY-MM-DD.log` next to the raw
        /// file, with the same midnight rotation.
        #[arg(long)]
        compact: bool,
        /// Skip the raw log and write only the compact one.
        #[arg(long, requires = "compact")]
        no_raw: bool,
        /// Registers whose jitter does not count as a change, as
        /// `<reg>:<band>` pairs: `--deadband 0x001b:1,0x001e:5,0x0024:15`.
        /// Repeatable. Hex needs `0x`, anything else is decimal.
        #[arg(long, value_name = "SPEC")]
        deadband: Vec<String>,
        /// How often the compact log gets a full snapshot of every polled
        /// block. `0s` disables snapshots.
        #[arg(long, default_value = "300s")]
        snapshot: String,
        /// Silence on one polled block that counts as a gap.
        #[arg(long, default_value = "10s")]
        poll_gap: String,
        /// How long a read request stays open waiting for its response.
        #[arg(long, default_value = "1s")]
        max_latency: String,
        /// Days of gzipped raw log to keep. Raw days before today are gzipped
        /// at startup and at each rotation; compact logs are never deleted.
        #[arg(long, default_value_t = DEFAULT_RAW_KEEP_DAYS)]
        raw_keep_days: u32,
    },
    /// Compact an existing raw log offline (`.gz` input is accepted).
    Compact {
        /// The raw capture log to read, or `-` for standard input.
        input: PathBuf,
        /// Where to write the compact log. Default: standard output.
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Same as `run --deadband`.
        #[arg(long, value_name = "SPEC")]
        deadband: Vec<String>,
        /// Same as `run --snapshot`.
        #[arg(long, default_value = "300s")]
        snapshot: String,
        /// Same as `run --poll-gap`.
        #[arg(long, default_value = "10s")]
        poll_gap: String,
        /// Same as `run --max-latency`.
        #[arg(long, default_value = "1s")]
        max_latency: String,
    },
    /// Mark the log with a human observation.
    Note {
        /// The note text.
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        #[arg(long, default_value = DEFAULT_CONTROL_PATH)]
        control: PathBuf,
    },
    /// Change the sniffer's bus settings.
    Bus {
        /// Baud rate, e.g. 9600.
        baud: u32,
        /// Frame format: 8N1, 8E1, 8O1 or 8N2.
        format: String,
        #[arg(long, default_value = DEFAULT_CONTROL_PATH)]
        control: PathBuf,
    },
    /// Ask the sniffer for its status line.
    Status {
        #[arg(long, default_value = DEFAULT_CONTROL_PATH)]
        control: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sniffer-capture: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn real_main() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            device,
            dir,
            control,
            no_control,
            max_backoff_secs,
            compact,
            no_raw,
            deadband,
            snapshot,
            poll_gap,
            max_latency,
            raw_keep_days,
        } => {
            let compact = if compact {
                Some(compact_config(
                    &deadband,
                    &snapshot,
                    &poll_gap,
                    &max_latency,
                )?)
            } else {
                None
            };
            let cfg = CaptureConfig {
                device,
                dir,
                control: if no_control { None } else { Some(control) },
                max_backoff: Duration::from_secs(max_backoff_secs.max(1)),
                raw: !no_raw,
                compact,
                raw_keep_days,
            };
            // `run` only returns on a fatal setup error, so a clean shutdown is
            // a signal arriving first.
            tokio::select! {
                r = capture::run(cfg) => r?,
                _ = shutdown() => eprintln!("sniffer-capture: shutting down"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Compact {
            input,
            out,
            deadband,
            snapshot,
            poll_gap,
            max_latency,
        } => {
            let cfg = compact_config(&deadband, &snapshot, &poll_gap, &max_latency)?;
            compact_file(&input, out.as_deref(), cfg)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Note { text, control } => {
            send(&control, &format!("note {}", text.join(" "))).await
        }
        Command::Bus {
            baud,
            format,
            control,
        } => send(&control, &format!("bus {baud} {format}")).await,
        Command::Status { control } => send(&control, "status").await,
    }
}

fn parse_duration(name: &str, s: &str) -> Result<Duration> {
    humantime::parse_duration(s).with_context(|| format!("--{name} {s:?} is not a duration"))
}

fn compact_config(
    deadband: &[String],
    snapshot: &str,
    poll_gap: &str,
    max_latency: &str,
) -> Result<CompactConfig> {
    let mut map: BTreeMap<u16, u16> = BTreeMap::new();
    for spec in deadband {
        for (addr, band) in compact::parse_deadbands(spec)? {
            map.insert(addr, band);
        }
    }
    Ok(CompactConfig {
        deadband: map,
        snapshot: parse_duration("snapshot", snapshot)?,
        gap: parse_duration("poll-gap", poll_gap)?,
        max_latency: parse_duration("max-latency", max_latency)?,
    })
}

/// Offline conversion. Reads `input` (`.gz` is unpacked on the fly, `-` is
/// standard input), writes compact lines, and reports the saving on stderr.
fn compact_file(input: &Path, out: Option<&Path>, cfg: CompactConfig) -> Result<()> {
    let reader: Box<dyn BufRead> = if input == Path::new("-") {
        Box::new(BufReader::new(std::io::stdin()))
    } else {
        let f =
            std::fs::File::open(input).with_context(|| format!("reading {}", input.display()))?;
        if input.extension().is_some_and(|e| e == "gz") {
            Box::new(BufReader::new(flate2::read::MultiGzDecoder::new(f)))
        } else {
            Box::new(BufReader::new(f))
        }
    };
    let mut writer: Box<dyn Write> = match out {
        Some(path) => Box::new(BufWriter::new(
            std::fs::File::create(path).with_context(|| format!("writing {}", path.display()))?,
        )),
        None => Box::new(BufWriter::new(std::io::stdout())),
    };

    let mut c = Compactor::new(cfg);
    let mut lines: Vec<String> = Vec::new();
    let mut bytes_in: u64 = 0;
    let mut bytes_out: u64 = 0;
    for line in reader.lines() {
        let line = line.context("reading the input log")?;
        bytes_in += line.len() as u64 + 1;
        lines.clear();
        c.push(&line, &mut lines);
        for l in &lines {
            writeln!(writer, "{l}")?;
            bytes_out += l.len() as u64 + 1;
        }
    }
    lines.clear();
    c.finish(&mut lines);
    for l in &lines {
        writeln!(writer, "{l}")?;
        bytes_out += l.len() as u64 + 1;
    }
    writer.flush()?;

    let s = c.stats();
    eprintln!(
        "sniffer-capture: {} lines / {} bytes in, {} lines / {} bytes out ({:.1}% of the lines, {:.1}% of the bytes)",
        s.lines_in,
        bytes_in,
        s.lines_out,
        bytes_out,
        pct(s.lines_out, s.lines_in),
        pct(bytes_out, bytes_in)
    );
    eprintln!(
        "sniffer-capture: {} polls, {} suppressed, {} unanswered read requests, {} snapshots, {} gaps, {} unparsed lines kept",
        s.polls, s.suppressed, s.unmatched_requests, s.snapshots, s.gaps, s.unparsed
    );
    Ok(())
}

fn pct(a: u64, b: u64) -> f64 {
    if b == 0 {
        return 0.0;
    }
    a as f64 * 100.0 / b as f64
}

async fn send(control: &std::path::Path, command: &str) -> Result<ExitCode> {
    let reply = capture::control_request(control, command).await?;
    println!("{reply}");
    Ok(if reply.starts_with("# err") {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

async fn shutdown() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sniffer-capture: cannot watch SIGTERM: {e}");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
