//! Gap detection on the live feed.
//!
//! Hyperliquid's public websocket has no sequence numbers on trades or book
//! snapshots, so we cannot prove a message was missed. What we can detect:
//!
//! * silence: a stream that normally ticks every second goes quiet for longer
//!   than its threshold;
//! * time running backwards: an exchange timestamp older than the last one on
//!   the same stream, which means reordering or a replayed message;
//! * reconnects: any disconnect is a gap by definition (see `feed.rs`).
//!
//! These are heuristics and the README says so. Each detection becomes a
//! `Gap` event, so a recorded file replays the same resets the live run saw.

use crate::event::Gap;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct GapDetector {
    /// Silence threshold per stream name, in ms.
    max_silence_ms: BTreeMap<String, i64>,
    /// Last exchange timestamp per (stream, coin).
    last_ts: BTreeMap<(String, String), i64>,
}

impl GapDetector {
    /// Default thresholds: the L2 book publishes about twice a second, so 5 s
    /// of silence is abnormal. Trades are bursty; 60 s without a BTC or ETH
    /// print on Hyperliquid is unusual enough to flag.
    pub fn new() -> Self {
        let mut max_silence_ms = BTreeMap::new();
        max_silence_ms.insert("book".to_string(), 5_000);
        max_silence_ms.insert("trades".to_string(), 60_000);
        Self {
            max_silence_ms,
            last_ts: BTreeMap::new(),
        }
    }

    pub fn with_threshold(mut self, stream: &str, ms: i64) -> Self {
        self.max_silence_ms.insert(stream.to_string(), ms);
        self
    }

    /// Observe a message on `stream` for `coin` with exchange time `ts`;
    /// `now` is the local detection time. Returns a gap if one is detected.
    pub fn observe(&mut self, stream: &str, coin: &str, ts: i64, now: i64) -> Option<Gap> {
        let key = (stream.to_string(), coin.to_string());
        let threshold = self.max_silence_ms.get(stream).copied().unwrap_or(60_000);
        let gap = match self.last_ts.get(&key) {
            Some(&last) if ts < last => Some(Gap {
                coin: coin.to_string(),
                stream: stream.to_string(),
                ts: now,
                from_ts: last,
                to_ts: ts,
                reason: format!("exchange time went backwards by {} ms", last - ts),
            }),
            Some(&last) if ts - last > threshold => Some(Gap {
                coin: coin.to_string(),
                stream: stream.to_string(),
                ts: now,
                from_ts: last,
                to_ts: ts,
                reason: format!("{} ms of silence, threshold {threshold} ms", ts - last),
            }),
            _ => None,
        };
        // Never move the high-water mark backwards.
        let entry = self.last_ts.entry(key).or_insert(ts);
        *entry = (*entry).max(ts);
        gap
    }

    /// Forget everything; called after a reconnect, which is itself reported
    /// as a gap, so the first message afterwards should not be reported twice.
    pub fn reset(&mut self) {
        self.last_ts.clear();
    }

    /// Last seen exchange time per coin across all streams, for the reconnect
    /// gap's `from_ts`.
    pub fn last_seen(&self) -> BTreeMap<String, i64> {
        let mut out: BTreeMap<String, i64> = BTreeMap::new();
        for ((_, coin), ts) in &self.last_ts {
            let e = out.entry(coin.clone()).or_insert(*ts);
            *e = (*e).max(*ts);
        }
        out
    }
}

impl Default for GapDetector {
    fn default() -> Self {
        Self::new()
    }
}
