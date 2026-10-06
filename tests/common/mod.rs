//! Shared helpers for the integration tests: synthetic bars with a known
//! shape, so each test can say exactly what the strategy should do.

#![allow(dead_code)]

use mft_engine::clock::ReplayClock;
use mft_engine::engine::{Decision, Engine};
use mft_engine::event::{Bar, Event, TextSignal};

pub const T0: i64 = 1_790_000_000_000 - (1_790_000_000_000 % 60_000);

/// A bar for minute `i` with the given close. Open/high/low are irrelevant
/// to the strategy, which only reads closes.
pub fn bar(coin: &str, i: i64, close: f64) -> Bar {
    let open_ts = T0 + i * 60_000;
    Bar {
        coin: coin.into(),
        ts: open_ts + 60_000,
        open_ts,
        open: close,
        high: close,
        low: close,
        close,
        volume: 1.0,
        trades: 1,
    }
}

/// 61 bars of small zig-zag noise (to give a non-zero volatility), then
/// `trend` bars each up 0.5%: a move far beyond 2 sigma. With the default
/// parameters the strategy goes long on the trend.
pub fn trending_closes(trend: usize) -> Vec<f64> {
    let mut closes = Vec::new();
    let mut px = 100.0;
    for i in 0..61 {
        px *= if i % 2 == 0 { 1.0005 } else { 0.9995 };
        closes.push(px);
    }
    for _ in 0..trend {
        px *= 1.005;
        closes.push(px);
    }
    closes
}

pub fn bars_from(coin: &str, closes: &[f64]) -> Vec<Event> {
    closes
        .iter()
        .enumerate()
        .map(|(i, c)| Event::Bar(bar(coin, i as i64, *c)))
        .collect()
}

/// Replay events synchronously through the engine, the same way the event
/// loop does (observe, then on_event). Returns all decisions.
pub fn replay(engine: &mut Engine, events: &[Event]) -> Vec<Decision> {
    let mut clock = ReplayClock::new(0);
    let mut out = Vec::new();
    for ev in events {
        mft_engine::clock::Clock::observe(&mut clock, ev);
        if let Some(d) = engine.on_event(ev, &clock) {
            out.push(d);
        }
    }
    out
}

pub fn text_signal(coin: &str, ts: i64, bullish: f64, bearish: f64) -> TextSignal {
    TextSignal {
        coin: coin.into(),
        ts,
        published_ts: ts,
        post_id: "test".into(),
        source: "test".into(),
        relevance: 1.0,
        bullish,
        bearish,
        novelty: 1.0,
        scorer: "test".into(),
    }
}

pub fn fills(decisions: &[Decision]) -> usize {
    decisions.iter().filter(|d| matches!(d, Decision::Filled { .. })).count()
}

pub fn blocked_reasons(decisions: &[Decision]) -> Vec<String> {
    decisions
        .iter()
        .filter_map(|d| match d {
            Decision::Blocked { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}
