//! Housekeeping for the raw daily logs: gzip the days that are finished, then
//! drop the archives that have aged out.
//!
//! A rotated raw day gzips about 35x (108 MiB -> ~3 MiB), which is what makes
//! keeping two weeks of raw frames affordable next to the compact logs. Only
//! `capture-*.log` / `capture-*.log.gz` are ever touched: the compact logs are
//! the long-term record and are never compressed or deleted here.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::NaiveDate;
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::logfile::RAW_PREFIX;

/// Default number of days of gzipped raw log to keep.
pub const DEFAULT_RAW_KEEP_DAYS: u32 = 14;

/// What housekeeping decided to do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Raw `.log` files to compress (file names, not paths).
    pub gzip: Vec<String>,
    /// `.log.gz` files to delete, including ones this plan is about to create.
    pub delete: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.gzip.is_empty() && self.delete.is_empty()
    }
}

/// The day a raw log file name refers to, and whether it is already gzipped.
///
/// Returns `None` for anything that is not a raw capture log - in particular
/// for every `compact-*` file.
pub fn raw_day(name: &str) -> Option<(NaiveDate, bool)> {
    let rest = name.strip_prefix(RAW_PREFIX)?.strip_prefix('-')?;
    let (day, gz) = match rest.strip_suffix(".log.gz") {
        Some(day) => (day, true),
        None => (rest.strip_suffix(".log")?, false),
    };
    let day = NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    Some((day, gz))
}

/// Decide what to compress and what to delete.
///
/// - every raw `.log` for a day before `today` is compressed (today's file is
///   still being written, so it is left alone);
/// - every `.log.gz` for a day older than `keep_days` before `today` is
///   deleted, including one this plan has just decided to create.
pub fn plan(names: &[String], today: NaiveDate, keep_days: u32) -> Plan {
    let cutoff = today - chrono::Duration::days(i64::from(keep_days));
    let mut out = Plan::default();
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort();
    for name in sorted {
        let Some((day, gz)) = raw_day(name) else {
            continue;
        };
        if gz {
            if day < cutoff {
                out.delete.push(name.clone());
            }
            continue;
        }
        if day >= today {
            continue;
        }
        out.gzip.push(name.clone());
        if day < cutoff {
            // Older than the retention window: compress it so there is a
            // single code path, then drop the archive straight away.
            out.delete.push(format!("{name}.gz"));
        }
    }
    out.delete.sort();
    out.delete.dedup();
    out
}

/// List the directory, run [`plan`] and carry it out. Blocking; call it from
/// `spawn_blocking` so a 100 MB gzip cannot stall the capture loop.
pub fn run(dir: &Path, today: NaiveDate, keep_days: u32) -> Result<Plan> {
    let names = list(dir)?;
    let plan = plan(&names, today, keep_days);
    for name in &plan.gzip {
        gzip_file(&dir.join(name)).with_context(|| format!("compressing {name}"))?;
    }
    for name in &plan.delete {
        let path = dir.join(name);
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("deleting {name}"))?;
        }
    }
    Ok(plan)
}

fn list(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    for entry in rd {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        if let Some(name) = entry.file_name().to_str() {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

/// `foo.log` -> `foo.log.gz`, then remove `foo.log`.
///
/// The archive is written to a `.part` file and renamed, so an interrupted run
/// never leaves a truncated `.gz` that looks finished, and the input is only
/// removed once the archive is on disk.
fn gzip_file(path: &Path) -> Result<PathBuf> {
    let gz = PathBuf::from(format!("{}.gz", path.display()));
    let part = PathBuf::from(format!("{}.part", gz.display()));
    {
        let mut src = BufReader::new(File::open(path)?);
        let mut enc = GzEncoder::new(BufWriter::new(File::create(&part)?), Compression::default());
        std::io::copy(&mut src, &mut enc)?;
        enc.finish()?
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()?;
    }
    std::fs::rename(&part, &gz)?;
    std::fs::remove_file(path)?;
    Ok(gz)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("a date")
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn recognises_only_raw_capture_names() {
        assert_eq!(
            raw_day("capture-2026-10-03.log"),
            Some((day("2026-10-03"), false))
        );
        assert_eq!(
            raw_day("capture-2026-10-03.log.gz"),
            Some((day("2026-10-03"), true))
        );
        assert_eq!(raw_day("compact-2026-10-03.log"), None);
        assert_eq!(raw_day("compact-2026-10-03.log.gz"), None);
        assert_eq!(raw_day("capture-2026-10-03.log.part"), None);
        assert_eq!(raw_day("capture-oops.log"), None);
        assert_eq!(raw_day("report.md"), None);
    }

    #[test]
    fn todays_file_is_left_alone() {
        let p = plan(
            &names(&["capture-2026-10-03.log", "capture-2026-10-02.log"]),
            day("2026-10-03"),
            14,
        );
        assert_eq!(p.gzip, names(&["capture-2026-10-02.log"]));
        assert!(p.delete.is_empty());
    }

    #[test]
    fn archives_older_than_the_window_are_deleted() {
        let p = plan(
            &names(&[
                "capture-2026-09-01.log.gz",
                "capture-2026-09-20.log.gz",
                "capture-2026-10-03.log",
            ]),
            day("2026-10-03"),
            14,
        );
        assert!(p.gzip.is_empty());
        assert_eq!(p.delete, names(&["capture-2026-09-01.log.gz"]));
    }

    #[test]
    fn compact_logs_are_never_touched() {
        let p = plan(
            &names(&[
                "compact-2026-01-01.log",
                "compact-2026-01-01.log.gz",
                "compact-2026-10-02.log",
            ]),
            day("2026-10-03"),
            1,
        );
        assert!(p.is_empty(), "{p:?}");
    }

    #[test]
    fn a_very_old_raw_log_is_compressed_then_dropped() {
        let p = plan(&names(&["capture-2026-01-01.log"]), day("2026-10-03"), 14);
        assert_eq!(p.gzip, names(&["capture-2026-01-01.log"]));
        assert_eq!(p.delete, names(&["capture-2026-01-01.log.gz"]));
    }

    #[test]
    fn keeping_zero_days_still_keeps_today() {
        let p = plan(
            &names(&["capture-2026-10-03.log", "capture-2026-10-02.log.gz"]),
            day("2026-10-03"),
            0,
        );
        assert!(p.gzip.is_empty());
        assert_eq!(p.delete, names(&["capture-2026-10-02.log.gz"]));
    }

    #[test]
    fn run_compresses_and_prunes_a_real_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "2026-10-02T10:00:00.000+02:00 [1.000] RX slave=0x01\n".repeat(500);
        for name in [
            "capture-2026-10-02.log",
            "capture-2026-10-03.log",
            "compact-2026-10-02.log",
        ] {
            std::fs::write(dir.path().join(name), &body).expect("write");
        }
        std::fs::write(dir.path().join("capture-2026-09-01.log.gz"), b"old").expect("write");

        let p = run(dir.path(), day("2026-10-03"), 14).expect("run");
        assert_eq!(p.gzip, names(&["capture-2026-10-02.log"]));
        assert_eq!(p.delete, names(&["capture-2026-09-01.log.gz"]));

        assert!(!dir.path().join("capture-2026-10-02.log").exists());
        assert!(!dir.path().join("capture-2026-09-01.log.gz").exists());
        assert!(dir.path().join("capture-2026-10-03.log").exists());
        assert!(
            dir.path().join("compact-2026-10-02.log").exists(),
            "the compact log must survive"
        );

        // The archive is a real gzip stream and round-trips.
        let gz = dir.path().join("capture-2026-10-02.log.gz");
        let raw = std::fs::read(&gz).expect("read gz");
        assert_eq!(&raw[..2], &[0x1f, 0x8b], "gzip magic");
        assert!(raw.len() < body.len() / 4, "it should compress well");
        let mut back = String::new();
        std::io::Read::read_to_string(
            &mut flate2::read::GzDecoder::new(File::open(&gz).expect("open gz")),
            &mut back,
        )
        .expect("decompress");
        assert_eq!(back, body);
        assert!(!dir.path().join("capture-2026-10-02.log.gz.part").exists());
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = run(&dir.path().join("nope"), day("2026-10-03"), 14).expect("run");
        assert!(p.is_empty());
    }
}
