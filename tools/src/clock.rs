//! A one-method clock, so midnight rotation is testable without waiting for
//! midnight.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Local, TimeDelta};

/// Source of local wall-clock time.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Local>;
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Local> {
        Local::now()
    }
}

/// A clock the test drives by hand.
#[derive(Debug, Clone)]
pub struct TestClock(Arc<Mutex<DateTime<Local>>>);

impl TestClock {
    pub fn new(at: DateTime<Local>) -> Self {
        Self(Arc::new(Mutex::new(at)))
    }

    pub fn set(&self, at: DateTime<Local>) {
        *self.0.lock().expect("test clock poisoned") = at;
    }

    pub fn advance(&self, by: TimeDelta) {
        let mut g = self.0.lock().expect("test clock poisoned");
        *g += by;
    }
}

impl Clock for TestClock {
    fn now(&self) -> DateTime<Local> {
        *self.0.lock().expect("test clock poisoned")
    }
}

/// Milliseconds since the Unix epoch, as an `i64` so an offset can be negative.
pub fn epoch_millis(t: DateTime<Local>) -> i64 {
    t.timestamp_millis()
}

/// Turn milliseconds since the Unix epoch back into local time.
pub fn from_epoch_millis(ms: i64) -> Option<DateTime<Local>> {
    DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&Local))
}
