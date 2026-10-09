//! Kalshi public market data (read-only, no authentication): the hourly
//! "price above strike at close" ladders for BTC (KXBTCD) and ETH (KXETHD).
//!
//! Two uses, one conversion:
//! * live: poll the open markets of a series, take the event closing next,
//!   and build a ladder from each contract's current yes bid/ask;
//! * backfill: list settled markets in a window, and for each hourly event
//!   fetch 1-minute candlesticks for strikes near spot, then build a ladder
//!   for every minute from the bid/ask at that minute's close.
//!
//! Both end in `ladder_event`, so a backfilled snapshot and a live one are
//! built by the same code. Nothing here can trade: Kalshi order endpoints need
//! signed requests and this module has no signing code and no key.

use crate::clock::parse_utc;
use crate::event::PredictionMarket;
use crate::prediction::ladder_snapshot;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

pub const API: &str = "https://api.elections.kalshi.com/trade-api/v2";

/// A quote is used only if its spread is at most this (in dollars, i.e.
/// probability). A 0/1 "quote" carries no information.
const MAX_SPREAD: f64 = 0.20;

/// The series for each coin we support.
pub fn series_for(coin: &str) -> Option<&'static str> {
    match coin {
        "BTC" => Some("KXBTCD"),
        "ETH" => Some("KXETHD"),
        _ => None,
    }
}

fn agent() -> Result<ureq::Agent> {
    Ok(ureq::AgentBuilder::new()
        .tls_connector(std::sync::Arc::new(native_tls::TlsConnector::new()?))
        .timeout(Duration::from_secs(20))
        .build())
}

fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T> {
    agent()?
        .get(url)
        .call()
        .map_err(|e| anyhow!("GET {url} failed: {e}"))?
        .into_json()
        .with_context(|| format!("unexpected JSON from {url}"))
}

#[derive(Debug, Clone, Deserialize)]
pub struct KMarket {
    pub ticker: String,
    pub event_ticker: String,
    #[serde(default)]
    pub floor_strike: Option<f64>,
    #[serde(default)]
    pub strike_type: Option<String>,
    #[serde(default)]
    pub yes_bid_dollars: Option<String>,
    #[serde(default)]
    pub yes_ask_dollars: Option<String>,
    pub open_time: String,
    pub close_time: String,
}

#[derive(Deserialize)]
struct MarketsPage {
    markets: Vec<KMarket>,
    #[serde(default)]
    cursor: Option<String>,
}

/// All markets of a series matching `query` (already URL-safe), following
/// the cursor until the last page.
fn list_markets(series: &str, query: &str) -> Result<Vec<KMarket>> {
    let mut out = Vec::new();
    let mut cursor = String::new();
    loop {
        let mut url = format!("{API}/markets?series_ticker={series}&limit=1000&{query}");
        if !cursor.is_empty() {
            url.push_str(&format!("&cursor={cursor}"));
        }
        let page: MarketsPage = get_json(&url)?;
        out.extend(page.markets);
        match page.cursor {
            Some(c) if !c.is_empty() => cursor = c,
            _ => break,
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(out)
}

/// Mid probability from a yes bid and ask, or `None` if either is missing or
/// the spread is too wide to mean anything.
pub fn mid(bid: Option<&str>, ask: Option<&str>) -> Option<f64> {
    let b: f64 = bid?.parse().ok()?;
    let a: f64 = ask?.parse().ok()?;
    if !(b.is_finite() && a.is_finite()) || a < b || a - b > MAX_SPREAD {
        return None;
    }
    Some((a + b) / 2.0)
}

/// Build a Kalshi `PredictionMarket` from raw (strike, mid) points; see
/// `prediction::ladder_snapshot` for the cleaning and trimming.
pub fn ladder_event(coin: &str, event: &str, ts: i64, close_ts: i64, raw: Vec<(f64, f64)>) -> Option<PredictionMarket> {
    ladder_snapshot("kalshi", coin, event, ts, close_ts, raw)
}

/// Live: the ladder of the open event that closes next.
pub fn fetch_live_ladder(coin: &str, now_ms: i64) -> Result<Option<PredictionMarket>> {
    let series = series_for(coin).ok_or_else(|| anyhow!("no Kalshi series for {coin}"))?;
    let markets = list_markets(series, "status=open")?;
    let next_close = markets
        .iter()
        .filter_map(|m| parse_utc(&m.close_time))
        .filter(|c| *c > now_ms)
        .min();
    let Some(close_ts) = next_close else { return Ok(None) };
    let mut event = String::new();
    let mut raw = Vec::new();
    for m in &markets {
        if parse_utc(&m.close_time) != Some(close_ts) || m.strike_type.as_deref() != Some("greater") {
            continue;
        }
        event = m.event_ticker.clone();
        if let (Some(k), Some(p)) = (m.floor_strike, mid(m.yes_bid_dollars.as_deref(), m.yes_ask_dollars.as_deref())) {
            raw.push((k, p));
        }
    }
    Ok(ladder_event(coin, &event, now_ms, close_ts, raw))
}

#[derive(Deserialize)]
struct Ohlc {
    #[serde(default)]
    close_dollars: Option<String>,
}

#[derive(Deserialize)]
struct Candle {
    end_period_ts: i64,
    #[serde(default)]
    yes_bid: Option<Ohlc>,
    #[serde(default)]
    yes_ask: Option<Ohlc>,
}

#[derive(Deserialize)]
struct CandleSeries {
    market_ticker: String,
    candlesticks: Vec<Candle>,
}

#[derive(Deserialize)]
struct CandleBatch {
    markets: Vec<CandleSeries>,
}

/// Backfill: one `PredictionMarket` per minute per hourly event closing in
/// `[start_ms, end_ms]`. `spot_at(ts)` gives the reference price used to
/// choose which strikes to download (the nearest `strikes_each_side` above
/// and below spot at the event's open).
pub fn backfill(
    coin: &str,
    start_ms: i64,
    end_ms: i64,
    strikes_each_side: usize,
    spot_at: &dyn Fn(i64) -> Option<f64>,
) -> Result<Vec<PredictionMarket>> {
    let series = series_for(coin).ok_or_else(|| anyhow!("no Kalshi series for {coin}"))?;
    let query = format!("status=settled&min_close_ts={}&max_close_ts={}", start_ms / 1000, end_ms / 1000);
    let mut by_event: BTreeMap<String, Vec<KMarket>> = BTreeMap::new();
    for m in list_markets(series, &query)? {
        by_event.entry(m.event_ticker.clone()).or_default().push(m);
    }

    let mut out = Vec::new();
    for (event, markets) in by_event {
        let (Some(open), Some(close)) = (
            markets.first().and_then(|m| parse_utc(&m.open_time)),
            markets.first().and_then(|m| parse_utc(&m.close_time)),
        ) else {
            continue;
        };
        if close - open != 3_600_000 {
            continue; // hourly events only; the daily ones have a different horizon
        }
        let Some(spot) = spot_at(open) else { continue };
        let mut ladder: Vec<&KMarket> = markets
            .iter()
            .filter(|m| m.strike_type.as_deref() == Some("greater") && m.floor_strike.is_some())
            .collect();
        ladder.sort_by(|a, b| a.floor_strike.unwrap_or(0.0).total_cmp(&b.floor_strike.unwrap_or(0.0)));
        let centre = ladder.partition_point(|m| m.floor_strike.unwrap_or(0.0) < spot);
        let lo = centre.saturating_sub(strikes_each_side);
        let hi = (centre + strikes_each_side).min(ladder.len());
        let chosen = &ladder[lo..hi];
        if chosen.len() < 3 {
            continue;
        }

        let tickers: Vec<&str> = chosen.iter().map(|m| m.ticker.as_str()).collect();
        let url = format!(
            "{API}/markets/candlesticks?market_tickers={}&start_ts={}&end_ts={}&period_interval=1",
            tickers.join(","),
            open / 1000,
            close / 1000
        );
        let batch: CandleBatch = get_json(&url)?;
        let strike_of: BTreeMap<&str, f64> =
            chosen.iter().map(|m| (m.ticker.as_str(), m.floor_strike.unwrap_or(f64::NAN))).collect();

        // minute end (s) -> strike -> mid, carrying the last quote forward
        // when a contract has no candle for a minute.
        let mut minutes: BTreeSet<i64> = BTreeSet::new();
        let mut per_strike: Vec<(f64, BTreeMap<i64, f64>)> = Vec::new();
        for series in &batch.markets {
            let Some(&k) = strike_of.get(series.market_ticker.as_str()) else { continue };
            let mut quotes = BTreeMap::new();
            for c in &series.candlesticks {
                let bid = c.yes_bid.as_ref().and_then(|o| o.close_dollars.as_deref());
                let ask = c.yes_ask.as_ref().and_then(|o| o.close_dollars.as_deref());
                if let Some(p) = mid(bid, ask) {
                    quotes.insert(c.end_period_ts, p);
                }
                minutes.insert(c.end_period_ts);
            }
            per_strike.push((k, quotes));
        }
        for &minute_end in &minutes {
            let raw: Vec<(f64, f64)> = per_strike
                .iter()
                .filter_map(|(k, q)| q.range(..=minute_end).next_back().map(|(_, p)| (*k, *p)))
                .collect();
            if let Some(pm) = ladder_event(coin, &event, minute_end * 1000, close, raw) {
                out.push(pm);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    out.sort_by_key(|p| p.ts);
    Ok(out)
}
