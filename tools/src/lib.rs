//! Host-side tools for the Modbus RTU sniffer.
//!
//! Two binaries live on top of this library:
//!
//! - `sniffer-capture` - a daemon that holds one TCP connection to the sniffer,
//!   writes a wall-clock-stamped log file per day, and exposes a unix control
//!   socket so `sniffer-capture note "..."` can mark the log while you poke at
//!   the controller. With `--compact` it also writes a deduplicated second log
//!   (see [`compact`]), and it gzips and expires the raw days (see
//!   [`archive`]). `sniffer-capture compact <log>` does the same conversion
//!   offline.
//! - `sniffer-analyze` - reads those logs back and writes a markdown protocol
//!   map: poll loop, register map, writes, and which registers moved around each
//!   note.
//!
//! `fake-device` is a third binary that speaks the device protocol over TCP, so
//! both of the above can be exercised with no hardware attached.

pub mod analyze;
pub mod archive;
pub mod capture;
pub mod clock;
pub mod compact;
pub mod device;
pub mod fake;
pub mod logfile;
