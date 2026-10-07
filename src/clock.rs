//! Where "now" comes from.
//!
//! The engine never calls the system clock directly. It asks a `Clock`. In a
//! replay the clock is the timestamp of the event being processed, so a
//! backtest of last Tuesday believes it is last Tuesday and the stale-data and
//! daily-loss rules behave exactly as they would have live. In paper mode the
//! clock is the arrival stamp of the event being processed: the wall-clock
//! time it was received, which is recorded with it. So the live clock is
//! itself an input in the session log, and a replay keyed by arrival runs on
//! exactly the clock the live engine had. Swapping the clock is one of the
//! only two things that differ between the modes (the other is the event
//! source).

use crate::event::Event;
use std::time::{SystemTime, UNIX_EPOCH};

pub trait Clock {
    /// Current time in Unix milliseconds.
    fn now_ms(&self) -> i64;

    /// Called by the event loop before each event is handed to the engine.
    /// A replay clock advances here; a wall clock ignores it.
    fn observe(&mut self, _event: &Event) {}

    /// The same, with the event's arrival stamp when one is known. Only a
    /// clock keyed by arrival uses it.
    fn observe_at(&mut self, event: &Event, _arrival_ts: Option<i64>) {
        self.observe(event)
    }
}

/// Which timestamp a replay orders events by and runs its clock on.
///
/// * `Exchange`: the event's own timestamp (exchange time for market data,
///   close time for bars). Right for backfilled history, where there was no
///   live receiver. Every experiment and backfill on the ledger ran on it.
/// * `Arrival`: when the live engine received the event. Right for a session
///   recorded live: it consumes what the live engine saw, in the order it saw
///   it, on the clock it had.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum TimeKey {
    #[default]
    Exchange,
    Arrival,
}

impl TimeKey {
    pub fn name(self) -> &'static str {
        match self {
            TimeKey::Exchange => "exchange",
            TimeKey::Arrival => "arrival",
        }
    }
}

/// Event time. Never moves backwards, so an out-of-order event cannot make
/// the engine think time has rewound.
#[derive(Debug, Default, Clone)]
pub struct ReplayClock {
    now: i64,
    key: TimeKey,
}

impl ReplayClock {
    /// Keyed by exchange time, as every replay was before arrival keying.
    pub fn new(start_ms: i64) -> Self {
        Self::keyed(start_ms, TimeKey::Exchange)
    }

    pub fn keyed(start_ms: i64, key: TimeKey) -> Self {
        Self { now: start_ms, key }
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
        self.observe_at(event, None);
    }

    fn observe_at(&mut self, event: &Event, arrival_ts: Option<i64>) {
        let t = match self.key {
            TimeKey::Exchange => event.ts(),
            TimeKey::Arrival => arrival_ts.unwrap_or_else(|| event.own_arrival_ts()),
        };
        self.advance_to(t);
    }
}

/// Wall time. Live paper runs on arrival stamps instead (see the module
/// note), so that its clock is recorded; this remains for callers that want
/// the bare wall clock.
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

/// Parse an RFC 3339 UTC timestamp such as `2026-10-06T04:00:00Z` or
/// `2026-10-05T09:00:33.381895Z` into Unix milliseconds. Only the `Z` form is
/// accepted, which is what Kalshi returns; anything else is an error rather
/// than a guess about the offset.
pub fn parse_utc(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, mo, da) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':').map(|p| p.parse::<i64>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    // Milliseconds from the first three fractional digits, if any.
    let ms = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(3).collect();
        format!("{digits:0<3}").parse::<i64>().ok()?
    };
    // Howard Hinnant's days_from_civil, the inverse of format_utc's step.
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + da - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + se) * 1000) + ms)
}
