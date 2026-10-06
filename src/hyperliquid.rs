//! Hyperliquid public market data: websocket message parsing and the REST
//! candle endpoint. Read-only. This module only ever talks to the public
//! `/info` endpoint and the public websocket; it has no signing code and no
//! order endpoint, so there is no way to place an order through it.
//!
//! Wire formats (as of 2026):
//! * trades:  `{"channel":"trades","data":[{"coin","side":"B"|"A","px","sz","time",...}]}`
//!   where side "B" means the buyer was the aggressor.
//! * l2Book:  `{"channel":"l2Book","data":{"coin","time","levels":[[bids],[asks]]}}`
//!   each level `{"px","sz","n"}`, best first.
//! * candles: `[{"t":open_ms,"T":close_ms,"s":coin,"o","h","l","c","v","n"}]`
//!
//! Prices and sizes arrive as decimal strings; we parse them and fail the
//! whole message if any does not parse.

use crate::event::{Bar, BookTop, Event, Side, Trade};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

pub const WS_URL: &str = "wss://api.hyperliquid.xyz/ws";
pub const INFO_URL: &str = "https://api.hyperliquid.xyz/info";

/// The subscribe messages for trades and L2 book on each coin.
pub fn subscribe_messages(coins: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for coin in coins {
        for kind in ["trades", "l2Book"] {
            let msg = serde_json::json!({
                "method": "subscribe",
                "subscription": { "type": kind, "coin": coin }
            });
            out.push(msg.to_string());
        }
    }
    out
}

/// Hyperliquid closes connections that are silent for 60 s; we ping well
/// inside that.
pub fn ping_message() -> String {
    r#"{"method":"ping"}"#.to_string()
}

#[derive(Deserialize)]
struct WsEnvelope {
    channel: String,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct WsTrade {
    coin: String,
    side: String,
    px: String,
    sz: String,
    time: i64,
}

#[derive(Deserialize)]
struct WsLevel {
    px: String,
    sz: String,
}

#[derive(Deserialize)]
struct WsBook {
    coin: String,
    time: i64,
    levels: Vec<Vec<WsLevel>>,
}

fn num(s: &str) -> Result<f64> {
    let v: f64 = s.parse().with_context(|| format!("not a number: {s:?}"))?;
    if !v.is_finite() {
        bail!("not finite: {s:?}");
    }
    Ok(v)
}

/// Parse one websocket text frame into zero or more events. Control messages
/// (subscription acks, pongs) produce no events.
pub fn parse_ws_message(text: &str, recv_ts: i64) -> Result<Vec<Event>> {
    let env: WsEnvelope = serde_json::from_str(text).context("websocket frame is not JSON")?;
    match env.channel.as_str() {
        "trades" => {
            let trades: Vec<WsTrade> = serde_json::from_value(env.data)?;
            trades
                .into_iter()
                .map(|t| {
                    let aggressor = match t.side.as_str() {
                        "B" => Side::Buy,
                        "A" => Side::Sell,
                        other => bail!("unknown trade side {other:?}"),
                    };
                    Ok(Event::Trade(Trade {
                        coin: t.coin,
                        ts: t.time,
                        recv_ts,
                        px: num(&t.px)?,
                        sz: num(&t.sz)?,
                        aggressor,
                    }))
                })
                .collect()
        }
        "l2Book" => {
            let book: WsBook = serde_json::from_value(env.data)?;
            let bid = book.levels.first().and_then(|l| l.first());
            let ask = book.levels.get(1).and_then(|l| l.first());
            let (Some(bid), Some(ask)) = (bid, ask) else {
                // An empty side is not a usable top of book; skip it.
                return Ok(vec![]);
            };
            Ok(vec![Event::BookTop(BookTop {
                coin: book.coin,
                ts: book.time,
                recv_ts,
                bid_px: num(&bid.px)?,
                bid_sz: num(&bid.sz)?,
                ask_px: num(&ask.px)?,
                ask_sz: num(&ask.sz)?,
            })])
        }
        _ => Ok(vec![]),
    }
}

#[derive(Deserialize)]
struct Candle {
    t: i64,
    #[serde(rename = "T")]
    close_t: i64,
    s: String,
    o: String,
    h: String,
    l: String,
    c: String,
    v: String,
    n: u64,
}

fn agent(timeout_secs: u64) -> Result<ureq::Agent> {
    Ok(ureq::AgentBuilder::new()
        .tls_connector(std::sync::Arc::new(native_tls::TlsConnector::new()?))
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build())
}

/// POST a read-only query to the public `/info` endpoint and parse the reply.
/// Every Hyperliquid read in this crate goes through here (or `get_public`).
pub fn info<T: serde::de::DeserializeOwned>(body: &serde_json::Value) -> Result<T> {
    let resp = agent(20)?
        .post(INFO_URL)
        .send_json(body.clone())
        .map_err(|e| anyhow!("info request {} failed: {e}", body["type"]))?;
    // Parse from the reader: some replies are larger than ureq's 10 MB
    // string limit.
    serde_json::from_reader(resp.into_reader()).context("info reply is not the expected JSON")
}

/// GET a public JSON document (the stats leaderboard is about 40 MB).
pub fn get_public<T: serde::de::DeserializeOwned>(url: &str) -> Result<T> {
    let resp = agent(120)?.get(url).call().map_err(|e| anyhow!("GET {url} failed: {e}"))?;
    serde_json::from_reader(std::io::BufReader::new(resp.into_reader())).with_context(|| format!("unexpected JSON from {url}"))
}

/// One request to `candleSnapshot`. The endpoint returns at most 5,000
/// candles per call and only keeps roughly the most recent 5,000 one-minute
/// candles in total, so "a few days" is all the 1m history there is.
pub fn fetch_candles(coin: &str, interval: &str, start_ms: i64, end_ms: i64) -> Result<Vec<Bar>> {
    let body = serde_json::json!({
        "type": "candleSnapshot",
        "req": { "coin": coin, "interval": interval, "startTime": start_ms, "endTime": end_ms }
    });
    let candles: Vec<Candle> = info(&body).context("candleSnapshot")?;
    candles
        .into_iter()
        .map(|c| {
            Ok(Bar {
                coin: c.s,
                // Known at the close: T is the last millisecond of the bar.
                ts: c.close_t + 1,
                open_ts: c.t,
                open: num(&c.o)?,
                high: num(&c.h)?,
                low: num(&c.l)?,
                close: num(&c.c)?,
                volume: num(&c.v)?,
                trades: c.n,
            })
        })
        .collect()
}

/// One settled funding payment from `fundingHistory`. Hyperliquid settles
/// funding every hour; `rate` is the fraction of position value paid for
/// that hour, by longs to shorts when positive.
#[derive(Debug, Clone, PartialEq, serde::Serialize, Deserialize)]
pub struct FundingRecord {
    pub coin: String,
    /// Exchange settlement time (a few ms after the hour).
    pub ts: i64,
    pub rate: f64,
    pub premium: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireFunding {
    coin: String,
    funding_rate: String,
    premium: String,
    time: i64,
}

/// One request to `fundingHistory`: at most 500 records from `start_ms`.
pub fn fetch_funding(coin: &str, start_ms: i64, end_ms: i64) -> Result<Vec<FundingRecord>> {
    let body = serde_json::json!({ "type": "fundingHistory", "coin": coin, "startTime": start_ms, "endTime": end_ms });
    let rows: Vec<WireFunding> = info(&body).context("fundingHistory")?;
    rows.into_iter()
        .map(|r| {
            Ok(FundingRecord {
                coin: r.coin,
                ts: r.time,
                rate: num(&r.funding_rate)?,
                premium: num(&r.premium)?,
            })
        })
        .collect()
}
