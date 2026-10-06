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

/// `window`: an explicit [start, end) in Unix ms; otherwise the last `days`.
pub fn run(coins: &[String], days: f64, window: Option<(i64, i64)>, out: &Path) -> Result<()> {
    let (start, end) = window.unwrap_or_else(|| {
        let end = wall_now_ms().div_euclid(BAR_MS) * BAR_MS;
        (end - (days * 86_400_000.0) as i64, end)
    });
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

/// Retry a public read a few times with backoff: a long fetch should not die
/// on one rate-limit reply or dropped connection.
fn with_retry<T>(what: &str, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut wait = Duration::from_secs(2);
    for attempt in 1.. {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if attempt < 6 => {
                eprintln!("{what}: attempt {attempt} failed ({e:#}); retrying in {wait:?}");
                std::thread::sleep(wait);
                wait *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// 1-hour candles with open time in `[start_ms, end_ms)`, paged.
pub fn fetch_hourly(coin: &str, start_ms: i64, end_ms: i64) -> Result<Vec<Bar>> {
    const HOUR: i64 = crate::carry::HOUR_MS;
    let mut by_open: BTreeMap<i64, Bar> = BTreeMap::new();
    let mut cursor = start_ms;
    while cursor < end_ms {
        let page = with_retry(coin, || fetch_candles(coin, "1h", cursor, end_ms - 1))?;
        std::thread::sleep(CARRY_PAUSE);
        let Some(last_open) = page.iter().map(|b| b.open_ts).max() else { break };
        for bar in page {
            by_open.insert(bar.open_ts, bar);
        }
        if last_open + HOUR <= cursor {
            bail!("pagination did not advance past {cursor}");
        }
        cursor = last_open + HOUR;
    }
    let now = wall_now_ms();
    Ok(by_open.into_values().filter(|b| b.ts <= now && b.open_ts >= start_ms && b.open_ts < end_ms).collect())
}

/// Settled funding in `[start_ms, end_ms]`, paged 500 records at a time.
pub fn fetch_funding_history(coin: &str, start_ms: i64, end_ms: i64) -> Result<Vec<crate::hyperliquid::FundingRecord>> {
    let mut by_ts = BTreeMap::new();
    let mut cursor = start_ms;
    loop {
        let page = with_retry(coin, || crate::hyperliquid::fetch_funding(coin, cursor, end_ms))?;
        std::thread::sleep(CARRY_PAUSE);
        let Some(last) = page.iter().map(|r| r.ts).max() else { break };
        let n = page.len();
        for r in page {
            by_ts.insert(r.ts, r);
        }
        if n < 500 || last >= end_ms {
            break;
        }
        cursor = last + 1;
    }
    Ok(by_ts.into_values().filter(|r| r.ts >= start_ms && r.ts <= end_ms).collect())
}

/// fundingHistory weighs about 45 per full page against a 1,200-per-minute
/// budget; a pause of 1.5 s keeps every request well inside it.
const CARRY_PAUSE: Duration = Duration::from_millis(1_500);

pub struct CarryFetch {
    pub days: i64,
    pub formation_days: i64,
    pub top: usize,
    pub benchmark: String,
    /// Window end, Unix ms on a UTC midnight (default: the last one).
    pub end: Option<i64>,
    pub bars_out: std::path::PathBuf,
    pub funding_out: std::path::PathBuf,
    pub universe_out: std::path::PathBuf,
}

/// `fetch-carry`: the v4 data set.
///
/// Universe rule (fixed before any return was looked at): every main-dex
/// perp in today's metadata, listed or delisted, ranked by dollar volume
/// (sum of hourly volume x close) over the first `formation_days` of the
/// window; the top `top` are the universe for the whole window. Only data
/// from the formation week chooses it, so a coin that later collapsed or was
/// delisted is still in; a coin listed after the formation week is not.
pub fn run_carry(opts: &CarryFetch) -> Result<()> {
    use crate::carry::{DAY_MS, HOUR_MS};
    let end = opts.end.unwrap_or_else(|| wall_now_ms().div_euclid(DAY_MS) * DAY_MS);
    if end % DAY_MS != 0 {
        bail!("window end must be a UTC midnight");
    }
    let start = end - opts.days * DAY_MS;
    let formation_end = start + opts.formation_days * DAY_MS;
    println!("window {} to {}, formation {} to {}", format_utc(start), format_utc(end), format_utc(start), format_utc(formation_end));

    let candidates = crate::universe::contracts_for_dex("")?;
    println!("{} main-dex candidates ({} delisted today)", candidates.len(), candidates.iter().filter(|c| c.delisted).count());
    let mut ranked: Vec<(String, bool, Option<f64>)> = Vec::new();
    for c in &candidates {
        // Bars stamped (start, formation_end]: candles opening in [start, formation_end).
        let bars = fetch_hourly(&c.coin, start, formation_end)?;
        let full_week = bars.len() as i64 >= opts.formation_days * 24 * 9 / 10;
        let vol = full_week.then(|| bars.iter().map(|b| b.volume * b.close).sum::<f64>());
        println!("  {:<12} {:>4} formation bars{}", c.coin, bars.len(), vol.map_or(String::new(), |v| format!(", ${:.1}M", v / 1e6)));
        ranked.push((c.coin.clone(), c.delisted, vol));
    }
    let mut by_vol: Vec<&(String, bool, Option<f64>)> = ranked.iter().filter(|r| r.2.is_some()).collect();
    by_vol.sort_by(|a, b| b.2.unwrap().total_cmp(&a.2.unwrap()).then_with(|| a.0.cmp(&b.0)));
    let universe: Vec<String> = by_vol.iter().take(opts.top).map(|r| r.0.clone()).collect();
    let mut coins = universe.clone();
    if !coins.contains(&opts.benchmark) {
        coins.push(opts.benchmark.clone());
    }
    println!("universe ({}): {}", universe.len(), universe.join(", "));

    let mut events = Vec::new();
    let mut funding = Vec::new();
    for coin in &coins {
        // One extra hour before `start` so the bar stamped `start` exists.
        let bars = fetch_hourly(coin, start - HOUR_MS, end)?;
        let rates = fetch_funding_history(coin, start - HOUR_MS / 2, end + HOUR_MS / 2)?;
        println!("  {coin:<12} {} bars, {} funding records", bars.len(), rates.len());
        events.extend(bars.into_iter().map(Event::Bar));
        funding.extend(rates);
    }
    sort_for_replay(&mut events);
    write_events(&opts.bars_out, &events)?;
    crate::artifacts::write_jsonl(&opts.funding_out, &funding)?;
    let doc = serde_json::json!({
        "fetched_at": format_utc(wall_now_ms()),
        "source": "https://api.hyperliquid.xyz/info metaAndAssetCtxs, candleSnapshot (1h), fundingHistory",
        "rule": format!(
            "Every main-dex perp in the metadata at fetch time (listed or delisted), ranked by dollar volume (sum of hourly volume x close) over the formation window; the top {} with at least 90% of formation hours present form the universe for the whole window. Builder (HIP-3) dexes excluded.",
            opts.top
        ),
        "window_start": format_utc(start),
        "window_end": format_utc(end),
        "formation_end": format_utc(formation_end),
        "benchmark": opts.benchmark,
        "universe": universe,
        "candidates": ranked.iter().map(|(c, d, v)| serde_json::json!({"coin": c, "delisted_at_fetch": d, "formation_dollar_volume": v})).collect::<Vec<_>>(),
    });
    crate::artifacts::write_json(&opts.universe_out, &doc)?;
    println!("wrote {} bars to {}, {} funding records to {}, universe to {}", events.len(), opts.bars_out.display(), funding.len(), opts.funding_out.display(), opts.universe_out.display());
    Ok(())
}
