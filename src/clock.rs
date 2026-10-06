//! Where "now" comes from.
//!
//! The engine never calls the system clock directly. It asks a `Clock`. In a
//! replay the clock is the timestamp of the event being processed, so a
//! backtest of last Tuesday believes it is last Tuesday and the stale-data and
//! daily-loss rules behave exactly as they would have live. In paper mode the
//! clock is the wall clock. Swapping the clock is one of the only two things
//! that differ between the modes (the other is the event source).

use crate::event::Event;
use std::time::{SystemTime, UNIX_EPOCH};

pub trait Clock {
    /// Current time in Unix milliseconds.
    fn now_ms(&self) -> i64;

    /// Called by the event loop before each event is handed to the engine.
    /// A replay clock advances here; a wall clock ignores it.
    fn observe(&mut self, _event: &Event) {}
}

/// Event time. Never moves backwards, so an out-of-order event cannot make
/// the engine think time has rewound.
#[derive(Debug, Default, Clone)]
pub struct ReplayClock {
    now: i64,
}

impl ReplayClock {
    pub fn new(start_ms: i64) -> Self {
        Self { now: start_ms }
    }

    /// Move the clock forward explicitly. Used by tests to simulate silence
    /// (time passing with no data), which is how the stale-data kill is tested.
    pub fn advance_to(&mut self, ts: i64) {
        self.now = self.now.max(ts);
    }
}

impl Clock for ReplayClock {
    fn now_ms(&self) -> i64 {
        self.now
    }

    fn observe(&mut self, event: &Event) {
        self.advance_to(event.ts());
    }
}

/// Wall time, for live paper trading.
#[derive(Debug, Default, Clone, Copy)]
pub struct WallClock;

impl Clock for WallClock {
    fn now_ms(&self) -> i64 {
        wall_now_ms()
    }
}

/// Wall-clock milliseconds. A host clock set before 1970 is broken beyond
/// anything the engine can reason about, so we stop rather than guess.
pub fn wall_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is set before 1970")
        .as_millis() as i64
}

/// Format Unix milliseconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC), for log lines.
///
/// Written out rather than pulling in a date crate. The day-to-date step is
/// Howard Hinnant's `civil_from_days` algorithm (proleptic Gregorian).
pub fn format_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (h, m, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}
