//! Prediction-market ladder math, the v2 gate, and its fail-closed behaviour.

mod common;

use common::*;
use mft_engine::clock::{format_utc, parse_utc};
use mft_engine::engine::{Engine, EngineConfig};
use mft_engine::event::{Event, PredictionMarket};
use mft_engine::kalshi::{ladder_event, mid};
use mft_engine::prediction::*;
use mft_engine::text::Caution;

fn ladder(ts: i64, close_ts: i64, strikes: &[f64], probs: &[f64]) -> PredictionMarket {
    PredictionMarket {
        coin: "BTC".into(),
        ts,
        venue: "test".into(),
        event: "TEST".into(),
        close_ts,
        strikes: strikes.to_vec(),
        prob_above: probs.to_vec(),
    }
}

#[test]
fn ladder_is_cleaned_to_non_increasing() {
    let (k, p) = clean_ladder(vec![(102.0, 0.30), (100.0, 0.80), (101.0, 0.85), (103.0, f64::NAN), (99.0, 1.2)]);
    assert_eq!(k, vec![99.0, 100.0, 101.0, 102.0]);
    assert_eq!(p, vec![1.0, 0.8, 0.8, 0.3], "clamped, and 0.85 cannot exceed the 0.80 below it");
}

#[test]
fn interpolation_and_quantiles() {
    let k = [100.0, 110.0, 120.0];
    let p = [0.9, 0.5, 0.1];
    assert_eq!(prob_above(&k, &p, 105.0), Some(0.7));
    assert_eq!(prob_above(&k, &p, 110.0), Some(0.5));
    assert_eq!(prob_above(&k, &p, 99.0), None, "no extrapolation below the ladder");
    assert_eq!(prob_above(&k, &p, 121.0), None, "or above it");
    assert_eq!(quantile(&k, &p, 0.5), Some(110.0));
    let q75 = quantile(&k, &p, 0.75).unwrap();
    assert!((q75 - 116.25).abs() < 1e-9, "{q75}"); // P(above) = 0.25 is 62.5% of the way from 110 to 120
    assert_eq!(quantile(&k, &p, 0.99), None, "beyond the ladder's range");
}

#[test]
fn kalshi_quotes_and_ladder_trimming() {
    assert!((mid(Some("0.40"), Some("0.42")).unwrap() - 0.41).abs() < 1e-12);
    assert_eq!(mid(Some("0.00"), Some("1.00")), None, "a 0/1 quote says nothing");
    assert_eq!(mid(Some("0.5"), Some("0.4")), None, "crossed");
    assert_eq!(mid(None, Some("0.4")), None);
    let raw = vec![(1.0, 0.995), (2.0, 0.99), (3.0, 0.8), (4.0, 0.5), (5.0, 0.2), (6.0, 0.01), (7.0, 0.005)];
    let pm = ladder_event("BTC", "E", 0, 1, raw).unwrap();
    assert_eq!(pm.strikes, vec![2.0, 3.0, 4.0, 5.0, 6.0], "only the informative part is kept");
    assert!(ladder_event("BTC", "E", 0, 1, vec![(1.0, 0.5)]).is_none());
}

#[test]
fn utc_parse_round_trips() {
    for s in ["2026-10-06T04:00:00Z", "2000-02-29T23:59:59Z", "1970-01-01T00:00:00Z"] {
        assert_eq!(format_utc(parse_utc(s).unwrap()), s);
    }
    assert_eq!(parse_utc("2026-10-05T09:00:33.381895Z"), Some(parse_utc("2026-10-05T09:00:33Z").unwrap() + 381));
    assert_eq!(parse_utc("2026-10-05T09:00:33+02:00"), None, "only UTC is accepted");
    assert_eq!(parse_utc("garbage"), None);
}

fn gate() -> PredictionState {
    PredictionState::new(PredictionParams { enabled: true, ..PredictionParams::default() })
}

#[test]
fn gate_keeps_agreeing_and_vetoes_disagreeing_entries() {
    let mut g = gate();
    // Market says 70% above 105 at the close.
    g.on_snapshot(&ladder(0, 3_600_000, &[100.0, 110.0], &[0.9, 0.5]));
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 105.0, 1_000), Caution::NONE, "long agrees");
    assert_eq!(g.caution_for("BTC", -1_000.0, 0.0, 105.0, 1_000), Caution::new(0.0), "short disagrees");
}

#[test]
fn gate_fails_closed() {
    let mut g = gate();
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 105.0, 0), Caution::new(0.0), "no ladder at all");
    g.on_snapshot(&ladder(0, 3_600_000, &[100.0, 110.0], &[0.9, 0.5]));
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 105.0, 181_000), Caution::new(0.0), "stale ladder");
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 150.0, 1_000), Caution::new(0.0), "spot outside the ladder");
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 105.0, 3_600_001), Caution::new(0.0), "event already closed");
    assert_eq!(g.caution_for("ETH", 1_000.0, 0.0, 105.0, 1_000), Caution::new(0.0), "other coin has no ladder");
}

#[test]
fn gate_is_off_in_v1_and_ignores_held_positions_by_default() {
    let off = PredictionState::new(PredictionParams::default());
    assert_eq!(off.caution_for("BTC", 1_000.0, 0.0, 105.0, 0), Caution::NONE);
    let mut g = gate();
    g.on_snapshot(&ladder(0, 3_600_000, &[100.0, 110.0], &[0.9, 0.5]));
    // Holding a short while the market leans up: entries-only, so no veto.
    assert_eq!(g.caution_for("BTC", -1_000.0, -1_000.0, 105.0, 1_000), Caution::NONE);
    let mut every_bar = PredictionState::new(PredictionParams { enabled: true, gate_entries_only: false, ..PredictionParams::default() });
    every_bar.on_snapshot(&ladder(0, 3_600_000, &[100.0, 110.0], &[0.9, 0.5]));
    assert_eq!(every_bar.caution_for("BTC", -1_000.0, -1_000.0, 105.0, 1_000), Caution::new(0.0));
}

/// The trending series with a ladder in force for the trigger bar.
fn with_ladder(p_at_spot: f64) -> Vec<Event> {
    let mut events = bars_from("BTC", &trending_closes(1));
    let trigger = events.last().unwrap().ts();
    let spot = match events.last().unwrap() {
        Event::Bar(b) => b.close,
        _ => unreachable!(),
    };
    let pm = ladder(trigger - 30_000, trigger + 1_800_000, &[spot - 1.0, spot + 1.0], &[p_at_spot, p_at_spot]);
    events.insert(events.len() - 1, Event::PredictionMarket(pm));
    events
}

#[test]
fn v2_takes_the_long_only_when_the_market_agrees() {
    assert!(fills(&replay(&mut Engine::new(EngineConfig::v1()), &with_ladder(0.2))) >= 1, "v1 ignores Kalshi");
    assert!(fills(&replay(&mut Engine::new(EngineConfig::v2()), &with_ladder(0.7))) >= 1);
    let vetoed = replay(&mut Engine::new(EngineConfig::v2()), &with_ladder(0.3));
    assert_eq!(fills(&vetoed), 0);
    // And with no ladder at all, v2 does not trade.
    let none = replay(&mut Engine::new(EngineConfig::v2()), &bars_from("BTC", &trending_closes(1)));
    assert_eq!(fills(&none), 0);
}
