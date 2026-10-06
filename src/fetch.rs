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

/// `fetch-kalshi`: backfill Kalshi hourly ladders over the window covered by
/// a bar file, one `PredictionMarket` event per minute. The bars supply the
/// spot price used to pick which strikes to download; they are not mixed
/// into the output.
pub fn run_kalshi(coins: &[String], bars_path: &Path, strikes_each_side: usize, out: &Path) -> Result<()> {
    let bars = crate::bars::read_events(bars_path)?;
    let mut events: Vec<Event> = Vec::new();
    for coin in coins {
        let opens: BTreeMap<i64, f64> = bars
            .iter()
            .filter_map(|e| match e {
                Event::Bar(b) if &b.coin == coin => Some((b.open_ts, b.open)),
                _ => None,
            })
            .collect();
        let (Some(&start), Some(&end)) = (opens.keys().next(), opens.keys().next_back()) else {
            println!("{coin}: no bars, skipped");
            continue;
        };
        let spot_at = |t: i64| opens.range(..=t).next_back().map(|(_, px)| *px);
        let ladders = crate::kalshi::backfill(coin, start, end + BAR_MS, strikes_each_side, &spot_at)?;
        let hours: std::collections::BTreeSet<&str> = ladders.iter().map(|p| p.event.as_str()).collect();
        println!("{coin}: {} minute snapshots from {} hourly events", ladders.len(), hours.len());
        events.extend(ladders.into_iter().map(Event::PredictionMarket));
    }
    sort_for_replay(&mut events);
    write_events(out, &events)?;
    println!("wrote {} prediction-market events to {}", events.len(), out.display());
    Ok(())
}
