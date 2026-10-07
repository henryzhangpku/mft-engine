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
    /// Id of the scored post, e.g. `hn-45567890`, so a signal can be traced
    /// back to its text.
    #[serde(default)]
    pub post_id: String,
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

/// A prediction-market snapshot: a ladder of "price at close above strike K"
/// contracts, turned into probabilities.
///
/// Kalshi's KXBTCD / KXETHD hourly events are binary contracts that pay $1 if
/// the index is above a strike at the event's close. The mid price of each
/// contract is the market's probability that the price ends above that strike,
/// so the ladder is the market's survival function for the price at
/// `close_ts`. `prediction.rs` turns it into an implied median and
/// P(close above current spot).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictionMarket {
    pub coin: String,
    /// When this snapshot was known (minute end for backfilled candles, poll
    /// time live).
    pub ts: i64,
    pub venue: String,
    /// Exchange event id, e.g. `KXBTCD-26OCT0600`.
    pub event: String,
    /// When the contracts settle: the horizon of the implied distribution.
    pub close_ts: i64,
    /// Strikes in ascending order.
    pub strikes: Vec<f64>,
    /// P(price at close > strike), one per strike, non-increasing.
    pub prob_above: Vec<f64>,
}

/// Crowd positioning: what a fixed set of top Hyperliquid leaderboard wallets
/// hold in one coin, aggregated. Built from public data only (the public
/// leaderboard and `clearinghouseState` of public addresses); no address is
/// stored, only the totals.
///
/// Hyperliquid exposes only *current* wallet state, so there is no history
/// to backfill: positioning exists in a replay only if it was recorded live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Positioning {
    pub coin: String,
    /// When the scan of all wallets finished.
    pub ts: i64,
    pub source: String,
    /// Wallets scanned, and how many of them hold this coin.
    pub wallets_scanned: u32,
    pub wallets_holding: u32,
    pub long_count: u32,
    pub short_count: u32,
    /// Position value in USD, long and short.
    pub long_value: f64,
    pub short_value: f64,
    /// Long share of the gross value, in [0, 1]. 0.5 when nobody holds it.
    pub long_share: f64,
    /// Change in `long_share` since the previous snapshot of the same wallet
    /// set, if there was one.
    #[serde(default)]
    pub long_share_change: Option<f64>,
    pub avg_long_leverage: f64,
    pub avg_short_leverage: f64,
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

/// A historical bar used only to warm the strategy up before a live session,
/// recorded at the head of the session log so a replay starts in the same
/// state the live engine did. It reaches the strategy's window and nothing
/// else: no market state, no clock-dependent rule, no order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WarmupBar {
    /// Wall-clock time the REST history arrived (all bars of one fetch share
    /// it). The arrival time key orders warm-up by this, before live data.
    pub recv_ts: i64,
    pub bar: Bar,
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
    PredictionMarket(PredictionMarket),
    Positioning(Positioning),
    Gap(Gap),
    Warmup(WarmupBar),
}

impl Event {
    /// The timestamp the event loop orders by and the replay clock advances to.
    pub fn ts(&self) -> i64 {
        match self {
            Event::Trade(e) => e.ts,
            Event::BookTop(e) => e.ts,
            Event::Bar(e) => e.ts,
            Event::TextSignal(e) => e.ts,
            Event::PredictionMarket(e) => e.ts,
            Event::Positioning(e) => e.ts,
            Event::Gap(e) => e.ts,
            Event::Warmup(e) => e.bar.ts,
        }
    }

    /// The best arrival time the event itself carries: our receive time for
    /// trades, book tops and warm-up history, otherwise its own timestamp
    /// (bars, gaps, polled ladders and positioning are stamped when we made
    /// or received them). A session log written by `paper` or `record` also
    /// stores the exact arrival per line (`Recorded::arrival_ts`), which wins.
    pub fn own_arrival_ts(&self) -> i64 {
        match self {
            Event::Trade(e) => e.recv_ts,
            Event::BookTop(e) => e.recv_ts,
            Event::Warmup(e) => e.recv_ts,
            other => other.ts(),
        }
    }

    pub fn coin(&self) -> &str {
        match self {
            Event::Trade(e) => &e.coin,
            Event::BookTop(e) => &e.coin,
            Event::Bar(e) => &e.coin,
            Event::TextSignal(e) => &e.coin,
            Event::PredictionMarket(e) => &e.coin,
            Event::Positioning(e) => &e.coin,
            Event::Gap(e) => &e.coin,
            Event::Warmup(e) => &e.bar.coin,
        }
    }

    /// A short name used for counting events in summaries.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Trade(_) => "Trade",
            Event::BookTop(_) => "BookTop",
            Event::Bar(_) => "Bar",
            Event::TextSignal(_) => "TextSignal",
            Event::PredictionMarket(_) => "PredictionMarket",
            Event::Positioning(_) => "Positioning",
            Event::Gap(_) => "Gap",
            Event::Warmup(_) => "Warmup",
        }
    }

    /// Tie-break rank for events that share a timestamp, so that sorting a
    /// merged replay is fully deterministic. Gaps first (they invalidate
    /// state), then text, prediction markets and positioning (context), then market data,
    /// then bars (decisions). Warm-up history goes with gaps: it only ever
    /// precedes live data.
    pub fn sort_rank(&self) -> u8 {
        match self {
            Event::Gap(_) | Event::Warmup(_) => 0,
            Event::TextSignal(_) => 1,
            Event::PredictionMarket(_) => 1,
            Event::Positioning(_) => 1,
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

/// One line of a session log: an event, plus the engine clock at the moment
/// the live engine consumed it.
///
/// `arrival_ts` is the wall-clock receive stamp, made non-decreasing in the
/// order the engine consumed events (a stamp earlier than the one before it,
/// from a different source racing on the channel, is raised to it). So the
/// file order *is* arrival order, and replaying by arrival gives the live
/// engine's clock and order exactly. The field is omitted when unknown, so a
/// line without it is a plain `Event` line, and older readers that parse
/// `Event` directly ignore it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recorded {
    #[serde(flatten)]
    pub event: Event,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrival_ts: Option<i64>,
}

impl Recorded {
    /// The arrival key: the recorded stamp, else what the event carries.
    pub fn arrival(&self) -> i64 {
        self.arrival_ts.unwrap_or_else(|| self.event.own_arrival_ts())
    }
}
