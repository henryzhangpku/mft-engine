//! One-minute bars: building them from trades, finding holes in a bar series,
//! and reading and writing JSONL event files.

use crate::event::{Bar, Event, Gap, Recorded, Trade};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub const BAR_MS: i64 = 60_000;

/// Aggregates trades into 1-minute bars, one in progress per coin.
///
/// A bar is closed by the first trade of a later minute, and is stamped with
/// that trade's time, because that is the moment we actually know the bar is
/// complete. The same builder is used live (paper) and when replaying a
/// recorded trade file, so both see identical bars.
#[derive(Debug, Default)]
pub struct BarBuilder {
    open: BTreeMap<String, Bar>,
}

impl BarBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a trade. Returns the previous bar if this trade closed it.
    /// Trades older than the bar in progress are ignored (late prints).
    pub fn push(&mut self, t: &Trade) -> Option<Bar> {
        let minute = t.ts.div_euclid(BAR_MS) * BAR_MS;
        match self.open.get_mut(&t.coin) {
            Some(bar) if minute == bar.open_ts => {
                bar.high = bar.high.max(t.px);
                bar.low = bar.low.min(t.px);
                bar.close = t.px;
                bar.volume += t.sz;
                bar.trades += 1;
                None
            }
            Some(bar) if minute < bar.open_ts => None,
            _ => {
                let fresh = Bar {
                    coin: t.coin.clone(),
                    ts: t.ts,
                    open_ts: minute,
                    open: t.px,
                    high: t.px,
                    low: t.px,
                    close: t.px,
                    volume: t.sz,
                    trades: 1,
                };
                let mut done = self.open.insert(t.coin.clone(), fresh)?;
                done.ts = t.ts;
                Some(done)
            }
        }
    }
}

/// For a replay: insert a built `Bar` right after each trade that closes one.
pub fn with_built_bars(events: Vec<Event>) -> Vec<Event> {
    let mut builder = BarBuilder::new();
    let mut out = Vec::with_capacity(events.len());
    for ev in events {
        let bar = match &ev {
            Event::Trade(t) => builder.push(t),
            _ => None,
        };
        out.push(ev);
        if let Some(b) = bar {
            out.push(Event::Bar(b));
        }
    }
    out
}

/// Find missing minutes in a bar series (all coins mixed is fine). Returns a
/// `Gap` per hole, stamped just before the first bar after the hole so a
/// replay sees the gap before it sees the bar.
pub fn detect_bar_gaps(events: &[Event]) -> Vec<Gap> {
    let mut last_open: BTreeMap<&str, i64> = BTreeMap::new();
    let mut gaps = Vec::new();
    for ev in events {
        if let Event::Bar(b) = ev {
            if let Some(prev) = last_open.get(b.coin.as_str()) {
                if b.open_ts > prev + BAR_MS {
                    gaps.push(Gap {
                        coin: b.coin.clone(),
                        stream: "bars".into(),
                        ts: b.ts,
                        from_ts: prev + BAR_MS,
                        to_ts: b.open_ts,
                        reason: format!("{} missing 1m bars", (b.open_ts - prev) / BAR_MS - 1),
                    });
                }
            }
            last_open.insert(&b.coin, b.open_ts);
        }
    }
    gaps
}

/// Read a JSONL file of `Event`s. A malformed line is an error, not a skip:
/// silently dropping data is how a backtest drifts away from reality.
pub fn read_events(path: &Path) -> Result<Vec<Event>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut events = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let ev: Event = serde_json::from_str(&line)
            .with_context(|| format!("{}:{}: bad event line", path.display(), i + 1))?;
        events.push(ev);
    }
    Ok(events)
}

/// Read a session log: `Event` lines, each with the arrival time the live
/// engine consumed it at, when the file recorded one. Plain event files read
/// fine too (every arrival is then `None`).
pub fn read_recorded(path: &Path) -> Result<Vec<Recorded>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let rec: Recorded = serde_json::from_str(&line)
            .with_context(|| format!("{}:{}: bad event line", path.display(), i + 1))?;
        out.push(rec);
    }
    Ok(out)
}

pub fn write_events(path: &Path, events: &[Event]) -> Result<()> {
    crate::artifacts::write_jsonl(path, events)
}
