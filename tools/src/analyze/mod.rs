//! Turning capture logs into a protocol map.

pub mod pair;
pub mod parse;
pub mod regs;
pub mod report;

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

/// Knobs for the analysis.
#[derive(Debug, Clone)]
pub struct Options {
    /// Half-width of the note correlation window.
    pub window: Duration,
    /// How long a request stays open while waiting for its response.
    pub max_latency: Duration,
    /// Bus silence longer than this is reported as a gap.
    pub gap: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(10),
            max_latency: Duration::from_secs(1),
            gap: Duration::from_secs(30),
        }
    }
}

/// Read capture logs and render the markdown report.
pub fn analyze_files(paths: &[impl AsRef<Path>], opts: &Options) -> Result<String> {
    let cap = parse::parse_files(paths)?;
    Ok(report::report(&cap, opts))
}

/// Analyze in-memory log lines.
pub fn analyze_lines<'a>(lines: impl IntoIterator<Item = &'a str>, opts: &Options) -> String {
    let cap = parse::parse_lines(lines);
    report::report(&cap, opts)
}
