//! Polymarket: parsing real responses, point-in-time minute ladders, the v2b
//! gate (fail closed, can only shrink, held positions not re-gated), venue
//! separation, replay determinism, and the read-only guard.
//!
//! Fixtures under `tests/fixtures/polymarket/` are REAL responses cached by
//! `fetch-polymarket` on 2026-10-09: the Gamma event response unchanged, and
//! a CLOB prices-history response cut to its first 20 points.

mod common;

use common::*;
use mft_engine::backtest::{load_events, run_backtest};
use mft_engine::clock::parse_utc;
use mft_engine::engine::{Engine, EngineConfig};
use mft_engine::event::{Event, PredictionMarket};
use mft_engine::polymarket::{event_slug, minute_ladders, parse_event, parse_history, MAX_QUOTE_AGE_MS};
use mft_engine::prediction::*;
use mft_engine::text::Caution;
use std::path::PathBuf;

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/polymarket").join(name);
    std::fs::read_to_string(p).unwrap()
}

fn ladder(venue: &str, ts: i64, close_ts: i64, strikes: &[f64], probs: &[f64]) -> PredictionMarket {
    PredictionMarket {
        coin: "BTC".into(),
        ts,
        venue: venue.into(),
        event: "TEST".into(),
        close_ts,
        strikes: strikes.to_vec(),
        prob_above: probs.to_vec(),
    }
}

#[test]
fn real_gamma_event_parses_to_eleven_strikes() {
    let markets = parse_event("BTC", &fixture("real_gamma_event_bitcoin-above-on-october-3-2026.json")).unwrap();
    assert_eq!(markets.len(), 11);
    let strikes: Vec<f64> = markets.iter().map(|m| m.strike).collect();
    assert_eq!(strikes, (0..11).map(|i| 74_000.0 + 2_000.0 * i as f64).collect::<Vec<_>>());
    let close = parse_utc("2026-10-03T16:00:00Z").unwrap();
    assert!(markets.iter().all(|m| m.close_ts == close && m.event == "bitcoin-above-on-october-3-2026"));
    let m = markets.iter().find(|m| m.strike == 84_000.0).unwrap();
    assert!(m.question.contains("$84,000"), "{}", m.question);
    assert!(m.yes_token.starts_with("8029824883") && m.no_token.starts_with("2710785505"));
    assert_eq!(event_slug("BTC", close / 86_400_000).as_deref(), Some("bitcoin-above-on-october-3-2026"));
    assert_eq!(event_slug("SOL", 0), None);
}

#[test]
fn real_prices_history_parses_in_ms_sorted() {
    let h = parse_history(&fixture("real_prices_history_btc_84000_oct3_first20.json")).unwrap();
    assert_eq!(h.len(), 20);
    assert_eq!(h[0], (1_790_956_213_000, 0.9355));
    assert!(h.windows(2).all(|w| w[0].0 < w[1].0));
    assert!(parse_history("{\"history\":[{\"t\":1,\"p\":1.5},{\"t\":2,\"p\":0.4}]}").unwrap() == vec![(2_000, 0.4)]);
    assert!(parse_history("not json").is_err());
}

const M: i64 = 60_000;

#[test]
fn a_price_is_usable_only_from_the_end_of_its_minute() {
    let t0 = 1_000 * M; // a minute boundary
    // Three strikes; prices printed 30 s into the minute starting at t0.
    let per_strike = vec![
        (100.0, vec![(t0 + 30_000, 0.99)]),
        (110.0, vec![(t0 + 30_000, 0.50)]),
        (120.0, vec![(t0 + 30_000, 0.01)]),
    ];
    let out = minute_ladders("BTC", "E", t0 + 60 * M, &per_strike, t0 - M, t0 + M);
    // The minute ending at t0 has nothing yet; the one ending at t0 + 1 min does.
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].ts, t0 + M, "stamped at the end of the minute the price was printed in");
    assert_eq!(out[0].venue, "polymarket");
    assert_eq!(out[0].prob_above, vec![0.99, 0.5, 0.01]);
}

#[test]
fn stale_strikes_are_dropped_and_a_thin_ladder_is_no_reading() {
    let t0 = 1_000 * M;
    let per_strike = vec![
        (100.0, vec![(t0, 0.99)]),
        (110.0, vec![(t0, 0.50)]),
        (120.0, vec![(t0, 0.01), (t0 + 10 * M, 0.02)]),
    ];
    let out = minute_ladders("BTC", "E", t0 + 60 * M, &per_strike, t0, t0 + 10 * M);
    let last = out.last().unwrap();
    assert!(last.ts - t0 <= MAX_QUOTE_AGE_MS, "no ladder once two strikes are older than the limit");
    assert_eq!(out.len() as i64, MAX_QUOTE_AGE_MS / M);
    // Two informative strikes only (spot between two near-certain strikes):
    // fewer than three, so no snapshot.
    let two = vec![(100.0, vec![(t0, 1.0)]), (110.0, vec![(t0, 0.02)]), (120.0, vec![(t0, 0.0)])];
    assert!(minute_ladders("BTC", "E", t0 + 60 * M, &two, t0, t0 + M).is_empty());
}

fn v2b_gate() -> PredictionState {
    PredictionState::new(PredictionParams { enabled: true, require_polymarket: true, ..PredictionParams::default() })
}

#[test]
fn v2b_gate_needs_both_markets_and_fails_closed() {
    let up = |venue: &str, ts: i64| ladder(venue, ts, 3_600_000, &[100.0, 110.0], &[0.9, 0.5]); // P(up at 105) = 0.7
    let down = |venue: &str, ts: i64| ladder(venue, ts, 3_600_000, &[100.0, 110.0], &[0.5, 0.1]); // 0.3
    let mut g = v2b_gate();
    g.on_snapshot(&up(KALSHI, 0));
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 105.0, 1_000), Caution::new(0.0), "Polymarket missing");
    g.on_snapshot(&down(POLYMARKET, 0));
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 105.0, 1_000), Caution::new(0.0), "Polymarket disagrees");
    g.on_snapshot(&up(POLYMARKET, 0));
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 105.0, 1_000), Caution::NONE, "both agree");
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 105.0, 181_000), Caution::new(0.0), "both stale");
    g.on_snapshot(&up(KALSHI, 170_000));
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 105.0, 181_000), Caution::new(0.0), "Polymarket stale");
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 150.0, 171_000), Caution::new(0.0), "spot outside the ladders");
    // Held positions are not re-gated, as in v2.
    assert_eq!(g.caution_for("BTC", -1_000.0, -1_000.0, 105.0, 171_000), Caution::NONE);
}

#[test]
fn a_polymarket_snapshot_never_replaces_kalshi_for_v2() {
    let mut g = PredictionState::new(PredictionParams { enabled: true, ..PredictionParams::default() });
    g.on_snapshot(&ladder(KALSHI, 0, 3_600_000, &[100.0, 110.0], &[0.9, 0.5]));
    g.on_snapshot(&ladder(POLYMARKET, 10, 3_600_000, &[100.0, 110.0], &[0.5, 0.1]));
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 105.0, 1_000), Caution::NONE, "v2 reads Kalshi only");
    assert_eq!(g.view("BTC", 105.0, 1_000).p_up, Some(0.7));
    let p = g.venue_view(POLYMARKET, "BTC", 105.0, 1_000).p_up.unwrap();
    assert!((p - 0.3).abs() < 1e-12);
}

#[test]
fn v2b_gate_can_only_shrink_and_vetoes_at_least_what_v2_vetoes() {
    let v2 = PredictionState::new(PredictionParams { enabled: true, ..PredictionParams::default() });
    let mut v2 = v2;
    let mut v2b = v2b_gate();
    let probs = [0.05, 0.3, 0.49, 0.5, 0.51, 0.7, 0.95];
    for (i, &pk) in probs.iter().enumerate() {
        for &pp in &probs {
            let t = i as i64;
            for g in [&mut v2, &mut v2b] {
                g.on_snapshot(&ladder(KALSHI, t, 3_600_000, &[100.0, 110.0], &[pk, pk]));
                g.on_snapshot(&ladder(POLYMARKET, t, 3_600_000, &[100.0, 110.0], &[pp, pp]));
            }
            for target in [-1_000.0, -1.0, 0.0, 1.0, 1_000.0] {
                for position in [-1_000.0, 0.0, 1_000.0] {
                    let a = v2.caution_for("BTC", target, position, 105.0, t + 1).value();
                    let b = v2b.caution_for("BTC", target, position, 105.0, t + 1).value();
                    assert!((0.0..=1.0).contains(&b));
                    assert!(b <= a, "v2b may only veto more than v2 (pk {pk}, pp {pp}, target {target})");
                    let shrunk = Caution::new(b).apply(target);
                    assert!(shrunk.abs() <= target.abs() && shrunk * target >= 0.0, "never larger, never flipped");
                }
            }
        }
    }
}

#[test]
fn v2b_engine_trades_only_when_both_agree() {
    let make = |pk: f64, pp: f64| {
        let mut events = bars_from("BTC", &trending_closes(1));
        let trigger = events.last().unwrap().ts();
        let spot = match events.last().unwrap() {
            Event::Bar(b) => b.close,
            _ => unreachable!(),
        };
        for (venue, p) in [(KALSHI, pk), (POLYMARKET, pp)] {
            let pm = ladder(venue, trigger - 30_000, trigger + 1_800_000, &[spot - 1.0, spot + 1.0], &[p, p]);
            events.insert(events.len() - 1, Event::PredictionMarket(pm));
        }
        events
    };
    assert!(fills(&replay(&mut Engine::new(EngineConfig::v2()), &make(0.7, 0.3))) >= 1, "v2 ignores Polymarket");
    assert_eq!(fills(&replay(&mut Engine::new(EngineConfig::v2b()), &make(0.7, 0.3))), 0);
    assert_eq!(fills(&replay(&mut Engine::new(EngineConfig::v2b()), &make(0.3, 0.7))), 0);
    assert!(fills(&replay(&mut Engine::new(EngineConfig::v2b()), &make(0.7, 0.7))) >= 1);
}

#[tokio::test]
async fn v2b_replay_on_the_recorded_data_is_deterministic_and_matches_the_ledger() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let files: Vec<PathBuf> = ["data/bars_1m.jsonl", "data/kalshi_ladders.jsonl", "data/polymarket_ladders.jsonl", "data/text_signals.jsonl"]
        .iter()
        .map(|f| root.join(f))
        .collect();
    let events = load_events(&files).unwrap();
    let a = run_backtest("full", events.clone(), EngineConfig::v2b()).await;
    let b = run_backtest("full", events.clone(), EngineConfig::v2b()).await;
    assert_eq!(a.decisions, b.decisions);
    assert_eq!(a.report, b.report);
    assert_eq!(a.report.decisions_fingerprint, "23fa36838a9dd14c", "ledger entry 23");
    // Adding the Polymarket file changes nothing for v1 and v2.
    let v1 = run_backtest("full", events.clone(), EngineConfig::v1()).await;
    let v2 = run_backtest("full", events, EngineConfig::v2()).await;
    assert_eq!(v1.report.decisions_fingerprint, "5ccac06aa73491d6");
    assert_eq!(v2.report.decisions_fingerprint, "ce040be98f47646d");
}

#[test]
fn polymarket_access_is_read_only_public_get() {
    let src = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/polymarket.rs")).unwrap();
    let lower = src.to_lowercase();
    for banned in [".post(", ".put(", ".delete(", "/order", "poly_api_key", "poly_signature", "poly_passphrase", "private_key", "eip712", "authorization", "set(\"poly"] {
        assert!(!lower.contains(banned), "src/polymarket.rs mentions {banned}");
    }
    // Every URL built is a Gamma event lookup or a CLOB price history.
    for (i, _) in src.match_indices("{CLOB_API}/") {
        assert!(src[i..].starts_with("{CLOB_API}/prices-history?"), "unexpected CLOB path");
    }
    for (i, _) in src.match_indices("{GAMMA_API}/") {
        assert!(src[i..].starts_with("{GAMMA_API}/events?"), "unexpected Gamma path");
    }
    assert_eq!(src.matches("{CLOB_API}/").count(), 1);
    assert_eq!(src.matches("{GAMMA_API}/").count(), 1);
}
