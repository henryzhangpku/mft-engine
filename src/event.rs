//! The one event type every part of the engine speaks.
//!
//! Live websocket messages, recorded files, historical bars and scored text
//! posts are all normalised into `Event` before anything else sees them. That
//! is what lets the strategy and risk code run unchanged in backtest and paper
//! mode: they only ever receive an `Event`, never a venue-specific message.
//!
//! All timestamps are Unix milliseconds (UTC) as `i64`. Prices and sizes are
//! `f64`. That is fine for a paper engine; a production engine would keep
//! prices as integer ticks so that no rounding can ever reach an order.

use serde::{Deserialize, Serialize};

/// Which side was the aggressor (the taker) in a trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

/// One public trade print.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub coin: String,
    /// Exchange timestamp of the trade.
    pub ts: i64,
    /// Local wall-clock time we received it. Kept so the recorded file shows
    /// feed latency, and so a replay can be audited against the live run.
    pub recv_ts: i64,
    pub px: f64,
    pub sz: f64,
    pub aggressor: Side,
}

/// Best bid and best ask, taken from a full L2 book snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookTop {
    pub coin: String,
    pub ts: i64,
    pub recv_ts: i64,
    pub bid_px: f64,
    pub bid_sz: f64,
    pub ask_px: f64,
    pub ask_sz: f64,
}

/// A one-minute OHLCV bar.
///
/// `ts` is the time the bar became *known*, not the time it opened. For a
/// historical candle that is its close time; for a bar built from live trades
/// it is the timestamp of the first trade of the next minute. Stamping a bar
/// with its open time would let a replay act on a close price before that
/// price existed, which is the most common look-ahead bug in bar backtests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub coin: String,
    pub ts: i64,
    pub open_ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub trades: u64,
}

/// A scored news or social post, produced by the Python sidecar.
///
/// The probabilities come from Jev (or the offline keyword mock). `ts` is when
/// the score became available to the engine: publication time plus scoring
/// latency. The strategy may only use this to reduce exposure; see `text.rs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextSignal {
    pub coin: String,
    pub ts: i64,
    pub published_ts: i64,
    pub source: String,
    /// Probability the post is about this coin at all, in [0, 1].
    pub relevance: f64,
    pub bullish: f64,
    pub bearish: f64,
    /// Probability the post is new information rather than a repost.
    pub novelty: f64,
    /// Which scorer produced it, e.g. "jev" or "mock-keyword-v1".
    pub scorer: String,
}

/// A hole in the data that we detected and want every consumer to know about.
///
/// Gaps are events rather than log lines so that a replay of a recorded file
/// reacts to them exactly as the live engine did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gap {
    pub coin: String,
    /// Which stream had the hole: "trades", "book", "bars" or "connection".
    pub stream: String,
    /// When the gap was detected.
    pub ts: i64,
    /// Last good timestamp before the hole.
    pub from_ts: i64,
    /// First timestamp after the hole.
    pub to_ts: i64,
    pub reason: String,
}

/// The single event type. Serialised with a `"type"` tag so a JSONL line is
/// self-describing, e.g. `{"type":"Bar","coin":"BTC",...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    Trade(Trade),
    BookTop(BookTop),
    Bar(Bar),
    TextSignal(TextSignal),
    Gap(Gap),
}

impl Event {
    /// The timestamp the event loop orders by and the replay clock advances to.
    pub fn ts(&self) -> i64 {
        match self {
            Event::Trade(e) => e.ts,
            Event::BookTop(e) => e.ts,
            Event::Bar(e) => e.ts,
            Event::TextSignal(e) => e.ts,
            Event::Gap(e) => e.ts,
        }
    }

    pub fn coin(&self) -> &str {
        match self {
            Event::Trade(e) => &e.coin,
            Event::BookTop(e) => &e.coin,
            Event::Bar(e) => &e.coin,
            Event::TextSignal(e) => &e.coin,
            Event::Gap(e) => &e.coin,
        }
    }

    /// A short name used for counting events in summaries.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Trade(_) => "Trade",
            Event::BookTop(_) => "BookTop",
            Event::Bar(_) => "Bar",
            Event::TextSignal(_) => "TextSignal",
            Event::Gap(_) => "Gap",
        }
    }

    /// Tie-break rank for events that share a timestamp, so that sorting a
    /// merged replay is fully deterministic. Gaps first (they invalidate
    /// state), then text (context), then market data, then bars (decisions).
    pub fn sort_rank(&self) -> u8 {
        match self {
            Event::Gap(_) => 0,
            Event::TextSignal(_) => 1,
            Event::BookTop(_) => 2,
            Event::Trade(_) => 3,
            Event::Bar(_) => 4,
        }
    }
}

/// Sort events into replay order: by time, then coin, then kind. Stable, so
/// events that tie on all three keep their file order.
pub fn sort_for_replay(events: &mut [Event]) {
    events.sort_by(|a, b| {
        a.ts()
            .cmp(&b.ts())
            .then_with(|| a.coin().cmp(b.coin()))
            .then_with(|| a.sort_rank().cmp(&b.sort_rank()))
    });
}
