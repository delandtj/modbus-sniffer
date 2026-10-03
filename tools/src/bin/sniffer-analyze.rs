//! `sniffer-analyze` - build a markdown protocol map from capture logs.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use modbus_sniffer_tools::analyze::{self, Options};

#[derive(Parser, Debug)]
#[command(
    name = "sniffer-analyze",
    about = "Build a markdown protocol map from sniffer-capture logs",
    long_about = "Reads one or more capture-YYYY-MM-DD.log files, pairs requests \
                  with responses, reconstructs the register map and correlates \
                  every note with the writes and register changes around it."
)]
struct Cli {
    /// Capture log files, in any order.
    #[arg(required = true, num_args = 1..)]
    logs: Vec<PathBuf>,

    /// Write the report here instead of to standard output.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Half-width of the note correlation window, e.g. 10s, 500ms, 2m.
    #[arg(long, default_value = "10s")]
    window: String,

    /// How long a request stays open while waiting for its response.
    #[arg(long, default_value = "1s")]
    max_latency: String,

    /// Bus silence longer than this is reported as a gap.
    #[arg(long, default_value = "30s")]
    gap: String,
}

fn parse_duration(name: &str, s: &str) -> Result<Duration> {
    humantime::parse_duration(s).with_context(|| format!("--{name} {s:?} is not a duration"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let opts = Options {
        window: parse_duration("window", &cli.window)?,
        max_latency: parse_duration("max-latency", &cli.max_latency)?,
        gap: parse_duration("gap", &cli.gap)?,
    };

    let report = analyze::analyze_files(&cli.logs, &opts)?;
    match &cli.out {
        Some(path) => {
            std::fs::write(path, &report)
                .with_context(|| format!("writing the report to {}", path.display()))?;
            eprintln!("sniffer-analyze: wrote {}", path.display());
        }
        None => print!("{report}"),
    }
    Ok(())
}
