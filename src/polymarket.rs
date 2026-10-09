//! Polymarket public market data (read-only, no authentication): the daily
//! "Bitcoin above ___ on <date>?" and "Ethereum above ___ on <date>?" ladders.
//!
//! Each daily event is a set of binary markets, "will the Binance BTC/USDT
//! 1-minute candle at 12:00 ET close above K", one per strike (11 strikes,
//! $2,000 apart for BTC and $100 for ETH). The Yes price is the market's
//! P(close > K), so an event is the same kind of ladder as a Kalshi event,
//! with a different horizon: noon ET on the event's date, not the top of the
//! next hour.
//!
//! Two public endpoints, both GET, no key:
//! * the Gamma API (`/events?slug=...`) for the event's markets: question,
//!   strike, resolution time, the Yes and No token ids;
//! * the CLOB's `/prices-history` for the Yes token's price history at
//!   1-minute fidelity.
//!
//! Every raw response is cached under `data/polymarket_raw/` and read from
//! there on a rerun, so the recorded ladders can be rebuilt offline and
//! audited against what the endpoints returned. Nothing here can trade:
//! Polymarket orders are signed with a wallet key, and this module has no
//! signing code, no wallet and no key, and calls no order endpoint.

use crate::clock::{format_utc, parse_utc};
use crate::event::PredictionMarket;
use crate::prediction::ladder_snapshot;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const GAMMA_API: &str = "https://gamma-api.polymarket.com";
pub const CLOB_API: &str = "https://clob.polymarket.com";
pub const VENUE: &str = "polymarket";

/// A strike's last price is carried forward at most this long. Older than
/// that, the strike is dropped from the minute's ladder.
pub const MAX_QUOTE_AGE_MS: i64 = 300_000;

/// Pause between live requests: well inside the public rate limits.
const REQUEST_PAUSE: Duration = Duration::from_millis(250);

const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;

/// The slug prefix of the daily "above" event for each coin we support.
pub fn slug_coin(coin: &str) -> Option<&'static str> {
    match coin {
        "BTC" => Some("bitcoin"),
        "ETH" => Some("ethereum"),
        _ => None,
    }
}

/// The daily event's slug for a UTC day number, e.g.
/// `bitcoin-above-on-october-3-2026`.
pub fn event_slug(coin: &str, day: i64) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november",
        "december",
    ];
    let date = format_utc(day * DAY_MS); // "YYYY-MM-DDT..."
    let year: i32 = date.get(0..4)?.parse().ok()?;
    let month: usize = date.get(5..7)?.parse().ok()?;
    let dom: u32 = date.get(8..10)?.parse().ok()?;
    Some(format!("{}-above-on-{}-{dom}-{year}", slug_coin(coin)?, MONTHS.get(month.checked_sub(1)?)?))
}

/// One strike of a daily event, as recorded in `data/polymarket_markets.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PmMarket {
    pub coin: String,
    pub event: String,
    pub question: String,
    pub strike: f64,
    /// Resolution time (Unix ms): the end of the 12:00 ET Binance candle.
    pub close_ts: i64,
    pub yes_token: String,
    pub no_token: String,
}

#[derive(Deserialize)]
struct GammaEvent {
    slug: String,
    #[serde(rename = "endDate")]
    end_date: String,
    #[serde(default)]
    markets: Vec<GammaMarket>,
}

#[derive(Deserialize)]
struct GammaMarket {
    question: String,
    #[serde(rename = "groupItemTitle", default)]
    group_item_title: Option<String>,
    #[serde(rename = "endDate", default)]
    end_date: Option<String>,
    /// JSON-encoded arrays inside a string, e.g. "[\"Yes\", \"No\"]".
    #[serde(default)]
    outcomes: Option<String>,
    #[serde(rename = "clobTokenIds", default)]
    clob_token_ids: Option<String>,
}

/// "84,000" -> 84000.0; also accepts "$84,000".
fn parse_strike(s: &str) -> Option<f64> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
    let k: f64 = cleaned.parse().ok()?;
    (k.is_finite() && k > 0.0).then_some(k)
}

/// The strikes of one Gamma `/events?slug=` response. A market without a
/// readable strike, a Yes/No pair or a resolution time is skipped, never
/// guessed.
pub fn parse_event(coin: &str, json: &str) -> Result<Vec<PmMarket>> {
    let events: Vec<GammaEvent> = serde_json::from_str(json).context("unexpected Gamma JSON")?;
    let Some(ev) = events.into_iter().next() else { return Ok(vec![]) };
    let event_close = parse_utc(&ev.end_date);
    let mut out = Vec::new();
    for m in ev.markets {
        let strike = m.group_item_title.as_deref().and_then(parse_strike);
        let close_ts = m.end_date.as_deref().and_then(parse_utc).or(event_close);
        let outcomes: Option<Vec<String>> = m.outcomes.as_deref().and_then(|s| serde_json::from_str(s).ok());
        let tokens: Option<Vec<String>> = m.clob_token_ids.as_deref().and_then(|s| serde_json::from_str(s).ok());
        let (Some(strike), Some(close_ts), Some(outcomes), Some(tokens)) = (strike, close_ts, outcomes, tokens) else {
            continue;
        };
        if outcomes.len() != 2 || tokens.len() != 2 || outcomes[0] != "Yes" || outcomes[1] != "No" {
            continue;
        }
        out.push(PmMarket {
            coin: coin.to_string(),
            event: ev.slug.clone(),
            question: m.question,
            strike,
            close_ts,
            yes_token: tokens[0].clone(),
            no_token: tokens[1].clone(),
        });
    }
    out.sort_by(|a, b| a.strike.total_cmp(&b.strike));
    Ok(out)
}

#[derive(Deserialize)]
struct History {
    history: Vec<Point>,
}

#[derive(Deserialize)]
struct Point {
    t: i64,
    p: f64,
}

/// A `/prices-history` response as (time in Unix ms, price) pairs, sorted.
/// Non-finite or out-of-[0, 1] prices are dropped.
pub fn parse_history(json: &str) -> Result<Vec<(i64, f64)>> {
    let h: History = serde_json::from_str(json).context("unexpected prices-history JSON")?;
    let mut out: Vec<(i64, f64)> = h
        .history
        .into_iter()
        .filter(|p| p.p.is_finite() && (0.0..=1.0).contains(&p.p))
        .map(|p| (p.t * 1000, p.p))
        .collect();
    out.sort_by_key(|x| x.0);
    Ok(out)
}

/// One ladder per minute end in `(from_ms, to_ms]`, point in time: a minute
/// ending at `m` sees, for each strike, its last price stamped at or before
/// `m`, if that price is at most `MAX_QUOTE_AGE_MS` old. A price is usable at
/// the end of the minute it was printed in, never earlier. The ladder is
/// cleaned and trimmed exactly as a Kalshi one (`ladder_snapshot`); fewer
/// than three informative strikes is no snapshot.
pub fn minute_ladders(
    coin: &str,
    event: &str,
    close_ts: i64,
    per_strike: &[(f64, Vec<(i64, f64)>)],
    from_ms: i64,
    to_ms: i64,
) -> Vec<PredictionMarket> {
    let mut out = Vec::new();
    let mut m = from_ms.div_euclid(MINUTE_MS) * MINUTE_MS + MINUTE_MS;
    while m <= to_ms {
        let raw: Vec<(f64, f64)> = per_strike
            .iter()
            .filter_map(|(k, hist)| {
                let i = hist.partition_point(|(t, _)| *t <= m);
                let (t, p) = *hist.get(i.checked_sub(1)?)?;
                (m - t <= MAX_QUOTE_AGE_MS).then_some((*k, p))
            })
            .collect();
        if let Some(pm) = ladder_snapshot(VENUE, coin, event, m, close_ts, raw) {
            out.push(pm);
        }
        m += MINUTE_MS;
    }
    out
}

fn agent() -> Result<ureq::Agent> {
    Ok(ureq::AgentBuilder::new()
        .tls_connector(std::sync::Arc::new(native_tls::TlsConnector::new()?))
        .timeout(Duration::from_secs(30))
        .user_agent("mft-engine research (read-only)")
        .build())
}

/// GET `url` as text, from `cache` if it is there, else from the network
/// (then written to `cache`). Retries with backoff on failure.
fn get_cached(url: &str, cache: &Path) -> Result<String> {
    if let Ok(text) = std::fs::read_to_string(cache) {
        return Ok(text);
    }
    let mut wait = Duration::from_secs(2);
    let mut last_err = anyhow!("no attempt");
    for _ in 0..5 {
        std::thread::sleep(REQUEST_PAUSE);
        match agent()?.get(url).call() {
            Ok(resp) => {
                let text = resp.into_string()?;
                crate::artifacts::write_text(cache, &text)?;
                return Ok(text);
            }
            Err(e) => {
                last_err = anyhow!("GET {url} failed: {e}");
                eprintln!("{last_err}; retrying in {wait:?}");
                std::thread::sleep(wait);
                wait *= 2;
            }
        }
    }
    Err(last_err)
}

/// What a backfill produced.
pub struct Backfill {
    pub markets: Vec<PmMarket>,
    pub ladders: Vec<PredictionMarket>,
}

/// Backfill one coin over `[start_ms, end_ms]`: every daily event whose
/// resolution falls in `(start_ms, end_ms + 1 day]`, each covering the 24
/// hours before its resolution (so exactly one event is live at any minute),
/// clipped to the window.
pub fn backfill(coin: &str, start_ms: i64, end_ms: i64, cache_dir: &Path) -> Result<Backfill> {
    if slug_coin(coin).is_none() {
        bail!("no Polymarket daily ladder for {coin}");
    }
    let mut markets_all = Vec::new();
    let mut ladders = Vec::new();
    let first_day = start_ms.div_euclid(DAY_MS);
    let last_day = end_ms.div_euclid(DAY_MS) + 1;
    for day in first_day..=last_day {
        let slug = event_slug(coin, day).ok_or_else(|| anyhow!("bad day {day}"))?;
        let url = format!("{GAMMA_API}/events?slug={slug}");
        let markets = parse_event(coin, &get_cached(&url, &cache_dir.join(format!("event_{slug}.json")))?)?;
        let Some(close_ts) = markets.first().map(|m| m.close_ts) else {
            println!("{coin}: {slug}: no markets");
            continue;
        };
        let from = (close_ts - DAY_MS).max(start_ms);
        let to = close_ts.min(end_ms);
        if to <= from {
            continue;
        }
        let mut per_strike = Vec::new();
        for m in &markets {
            // Ask from 10 minutes before the coverage so the first minute
            // has a last price to carry.
            let url = format!(
                "{CLOB_API}/prices-history?market={}&startTs={}&endTs={}&fidelity=1",
                m.yes_token,
                (from - 600_000) / 1000,
                to / 1000
            );
            let cache = cache_dir.join(format!("history_{slug}_{}.json", m.strike));
            per_strike.push((m.strike, parse_history(&get_cached(&url, &cache)?)?));
        }
        let snaps = minute_ladders(coin, &slug, close_ts, &per_strike, from, to);
        println!(
            "{coin}: {slug} (resolves {}): {} strikes, {} minute ladders",
            format_utc(close_ts),
            markets.len(),
            snaps.len()
        );
        ladders.extend(snaps);
        markets_all.extend(markets);
    }
    Ok(Backfill {
        markets: markets_all,
        ladders,
    })
}

/// Where the raw responses are cached by default.
pub fn default_cache_dir() -> PathBuf {
    PathBuf::from("data/polymarket_raw")
}
