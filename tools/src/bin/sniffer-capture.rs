//! `sniffer-capture` - the capture daemon and its control CLI.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use modbus_sniffer_tools::capture::{
    self, CaptureConfig, DEFAULT_CONTROL_PATH, DEFAULT_DEVICE, DEFAULT_LOG_DIR,
};

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
        } => {
            let cfg = CaptureConfig {
                device,
                dir,
                control: if no_control { None } else { Some(control) },
                max_backoff: Duration::from_secs(max_backoff_secs.max(1)),
            };
            // `run` only returns on a fatal setup error, so a clean shutdown is
            // a signal arriving first.
            tokio::select! {
                r = capture::run(cfg) => r?,
                _ = shutdown() => eprintln!("sniffer-capture: shutting down"),
            }
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
