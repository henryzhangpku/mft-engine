//! The text signal can only reduce exposure; fills and PnL add up; and there
//! is no order path in the source.

mod common;

use common::*;
use mft_engine::engine::{Decision, Engine, EngineConfig};
use mft_engine::event::Event;
use mft_engine::execution::{FillModel, Portfolio};
use mft_engine::text::{check_not_riskier, Caution, TextParams, TextState};

#[test]
fn caution_is_always_in_zero_one() {
    for x in [-5.0, -0.1, 0.0, 0.3, 1.0, 1.0001, 7.0, f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        let c = Caution::new(x).value();
        assert!((0.0..=1.0).contains(&c), "{x} -> {c}");
    }
    assert_eq!(Caution::new(f64::NAN).value(), 0.0, "unknown means most cautious");
}

#[test]
fn caution_never_enlarges_or_flips_any_target() {
    let multipliers = [-1.0, 0.0, 0.25, 0.5, 0.99, 1.0, 1.5, 10.0, f64::NAN];
    let targets = [-2_000.0, -1_000.0, -0.01, 0.0, 0.01, 1_000.0, 2_000.0];
    for m in multipliers {
        for t in targets {
            let after = Caution::new(m).apply(t);
            assert!(check_not_riskier(t, after).is_ok(), "m {m} t {t} -> {after}");
        }
    }
}

#[test]
fn the_engine_check_catches_a_riskier_target() {
    assert!(check_not_riskier(1_000.0, 1_500.0).is_err());
    assert!(check_not_riskier(1_000.0, -500.0).is_err());
    assert!(check_not_riskier(0.0, 100.0).is_err(), "text cannot open a position");
    assert!(check_not_riskier(1_000.0, 0.0).is_ok());
}

#[test]
fn only_the_opposing_probability_matters() {
    let mut s = TextState::new(TextParams::default());
    s.on_signal(&text_signal("BTC", 0, 0.99, 0.0)); // very bullish
    assert_eq!(s.caution_for("BTC", 1_000.0, 1), Caution::NONE, "bullish cannot add to a long");
    assert_eq!(s.caution_for("BTC", -1_000.0, 1), Caution::new(0.0), "bullish vetoes a short");
    s.on_signal(&text_signal("BTC", 0, 0.0, 0.3));
    assert_eq!(s.caution_for("BTC", 1_000.0, 1).value(), 0.7, "mild bearish shrinks a long");
    assert_eq!(s.caution_for("BTC", 0.0, 1), Caution::NONE);
    // Expired signals do nothing; future-dated ones are not used early.
    assert_eq!(s.caution_for("BTC", 1_000.0, 31 * 60_000), Caution::NONE);
    assert_eq!(s.caution_for("BTC", 1_000.0, -1), Caution::NONE);
}

fn with_text(signal_bull: f64, signal_bear: f64) -> Vec<Event> {
    let mut events = bars_from("BTC", &trending_closes(3));
    let ts = events[0].ts();
    // Published before the first bar; config_long_ttl keeps it in force.
    events.insert(0, Event::TextSignal(text_signal("BTC", ts - 1, signal_bull, signal_bear)));
    events
}

fn config_long_ttl() -> EngineConfig {
    let mut c = EngineConfig::default();
    c.text.ttl_ms = 24 * 3_600_000;
    c
}

#[test]
fn bearish_text_vetoes_the_long_in_the_engine() {
    let base = replay(&mut Engine::new(config_long_ttl()), &bars_from("BTC", &trending_closes(3)));
    assert!(fills(&base) >= 1);
    let vetoed = replay(&mut Engine::new(config_long_ttl()), &with_text(0.0, 0.95));
    assert_eq!(fills(&vetoed), 0);
}

#[test]
fn bullish_text_changes_nothing_for_a_long() {
    let base = replay(&mut Engine::new(config_long_ttl()), &bars_from("BTC", &trending_closes(3)));
    let bullish = replay(&mut Engine::new(config_long_ttl()), &with_text(0.99, 0.0));
    assert_eq!(base, bullish, "a bullish post must not make a long any bigger");
}

#[test]
fn text_never_increases_gross_exposure_on_real_data() {
    // On the committed sample, every filled target with text is no larger
    // than the strategy's raw target.
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let events = mft_engine::backtest::load_events(&[
        root.join("data/bars_1m.jsonl"),
        root.join("data/text_signals.jsonl"),
    ])
    .unwrap();
    let decisions = replay(&mut Engine::new(EngineConfig::default()), &events);
    for d in &decisions {
        let (raw, target) = match d {
            Decision::Filled { raw_target, target, .. } => (*raw_target, *target),
            Decision::Blocked { raw_target, target, .. } => (*raw_target, *target),
        };
        assert!(check_not_riskier(raw, target).is_ok(), "{d:?}");
    }
}

#[test]
fn fill_model_charges_slippage_and_fee() {
    let m = FillModel { taker_fee_bps: 10.0, slippage_bps: 5.0 };
    let buy = m.fill("BTC", 0, 2.0, 100.0);
    assert!((buy.px - 100.05).abs() < 1e-9);
    assert!((buy.fee - 2.0 * 100.05 * 0.001).abs() < 1e-9);
    let sell = m.fill("BTC", 0, -2.0, 100.0);
    assert!((sell.px - 99.95).abs() < 1e-9);
}

#[test]
fn round_trip_pnl_and_flip_accounting() {
    let m = FillModel { taker_fee_bps: 0.0, slippage_bps: 0.0 };
    let mut p = Portfolio::default();
    p.apply(&m.fill("BTC", 0, 1.0, 100.0));
    p.apply(&m.fill("BTC", 1, -2.0, 110.0)); // close +10, open short 1 @110
    p.apply(&m.fill("BTC", 2, 1.0, 105.0)); // close short +5
    assert_eq!(p.round_trips, vec![10.0, 5.0]);
    assert_eq!(p.position("BTC"), 0.0);
    assert!((p.equity(&Default::default()) - 15.0).abs() < 1e-9);
}

#[test]
fn there_is_no_order_path_in_the_source() {
    // Paper only by construction: nothing in src/ references Hyperliquid's
    // order endpoint, signing, or private keys.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap().to_lowercase();
        for banned in ["hyperliquid.xyz/exchange", "\"/exchange\"", "private_key", "secret_key", "eip712", "sign_l1_action"] {
            assert!(!text.contains(banned), "{} mentions {banned}", path.display());
        }
    }
}

#[test]
fn paper_signal_line_is_human_readable() {
    let events = bars_from("BTC", &trending_closes(1));
    let mut eng = Engine::new(EngineConfig::default());
    let decisions = replay(&mut eng, &events);
    let line = mft_engine::paper::signal_line(decisions.last().unwrap(), &eng);
    println!("{line}");
    assert!(line.starts_with("SIGNAL 2026-"), "{line}");
    for part in ["BTC-PERP LONG", "target +1000 USD", "reason: 5m momentum z=+", "risk: PASSED"] {
        assert!(line.contains(part), "missing {part:?} in {line}");
    }
}
