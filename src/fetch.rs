//! `fetch-bars`: download 1-minute candles and save them as `Bar` events.

use crate::bars::{detect_bar_gaps, write_events, BAR_MS};
use crate::clock::{format_utc, wall_now_ms};
use crate::event::{sort_for_replay, Bar, Event};
use crate::hyperliquid::fetch_candles;
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// Pause between requests. The info endpoint is weight-limited per IP; a few
/// requests a second is far inside the limit and costs nothing here.
const REQUEST_PAUSE: Duration = Duration::from_millis(300);

/// Fetch `[start_ms, end_ms)` for one coin, paging forward until done.
pub fn fetch_coin(coin: &str, start_ms: i64, end_ms: i64) -> Result<Vec<Bar>> {
    // Keyed by open time so overlapping pages cannot create duplicates.
    let mut by_open: BTreeMap<i64, Bar> = BTreeMap::new();
    let mut cursor = start_ms;
    while cursor < end_ms {
        let page = fetch_candles(coin, "1m", cursor, end_ms)?;
        let Some(last_open) = page.iter().map(|b| b.open_ts).max() else {
            break; // nothing more available
        };
        for bar in page {
            by_open.insert(bar.open_ts, bar);
        }
        if last_open + BAR_MS <= cursor {
            bail!("pagination did not advance past {cursor}");
        }
        cursor = last_open + BAR_MS;
        std::thread::sleep(REQUEST_PAUSE);
    }
    let now = wall_now_ms();
    // Drop the bar still forming: its "close" is not a close yet.
    Ok(by_open.into_values().filter(|b| b.ts <= now && b.open_ts < end_ms).collect())
}

pub fn run(coins: &[String], days: f64, out: &Path) -> Result<()> {
    let end = wall_now_ms().div_euclid(BAR_MS) * BAR_MS;
    let start = end - (days * 86_400_000.0) as i64;
    let mut events: Vec<Event> = Vec::new();
    for coin in coins {
        let bars = fetch_coin(coin, start, end)?;
        match (bars.first(), bars.last()) {
            (Some(f), Some(l)) => println!(
                "{coin}: {} bars, {} to {}",
                bars.len(),
                format_utc(f.open_ts),
                format_utc(l.open_ts)
            ),
            _ => println!("{coin}: no bars returned"),
        }
        events.extend(bars.into_iter().map(Event::Bar));
    }
    sort_for_replay(&mut events);
    for gap in detect_bar_gaps(&events) {
        println!("gap: {} {} ({} to {})", gap.coin, gap.reason, format_utc(gap.from_ts), format_utc(gap.to_ts));
    }
    write_events(out, &events)?;
    println!("wrote {} bars to {}", events.len(), out.display());
    Ok(())
}
