//! Each risk rule blocks as specified, errors block, and the engine routes
//! every order through the rules.

mod common;

use anyhow::{bail, Result};
use common::*;
use perp_engine::clock::ReplayClock;
use perp_engine::engine::{Decision, Engine, EngineConfig};
use perp_engine::event::Event;
use perp_engine::risk::*;

fn ctx(position_qty: f64, daily_pnl: f64) -> RiskContext {
    RiskContext {
        now_ms: 1_000_000,
        position_qty,
        last_data_ms: Some(1_000_000),
        daily_pnl,
    }
}

fn order(qty: f64, px: f64) -> OrderIntent {
    OrderIntent { coin: "BTC".into(), qty, ref_px: px }
}

fn standard() -> RiskEngine {
    RiskEngine::new(RiskLimits::default())
}

#[test]
fn allows_a_normal_order() {
    assert_eq!(standard().check(&order(10.0, 100.0), &ctx(0.0, 0.0)), Ok(()));
}

#[test]
fn max_order_notional_blocks() {
    // 30 * 100 = 3,000 > 2,500 limit.
    let r = standard().check(&order(30.0, 100.0), &ctx(0.0, 0.0));
    assert!(r.unwrap_err().starts_with("max_order_notional"));
}

#[test]
fn max_position_blocks_increase_but_allows_reduction() {
    // Holding 10 @ 100 = 1,000; buying 6 more makes 1,600 > 1,500.
    let r = standard().check(&order(6.0, 100.0), &ctx(10.0, 0.0));
    assert!(r.unwrap_err().starts_with("max_position"));
    // Already over the limit (price moved), selling some is still allowed.
    assert_eq!(standard().check(&order(-5.0, 100.0), &ctx(20.0, 0.0)), Ok(()));
}

#[test]
fn max_daily_loss_blocks_new_risk_but_lets_you_out() {
    let lost = ctx(5.0, -60.0); // limit is 50
    let more = standard().check(&order(1.0, 100.0), &lost);
    assert!(more.unwrap_err().starts_with("max_daily_loss"));
    assert_eq!(standard().check(&order(-5.0, 100.0), &lost), Ok(()));
    // Flipping through zero is not a reduction.
    let flip = standard().check(&order(-10.0, 100.0), &lost);
    assert!(flip.unwrap_err().starts_with("max_daily_loss"));
}

#[test]
fn stale_data_blocks() {
    let mut c = ctx(0.0, 0.0);
    c.last_data_ms = Some(c.now_ms - 91_000); // limit 90 s
    assert!(standard().check(&order(1.0, 100.0), &c).unwrap_err().starts_with("stale_data"));
    c.last_data_ms = None;
    assert!(standard().check(&order(1.0, 100.0), &c).unwrap_err().contains("no market data"));
}

#[test]
fn data_from_the_future_is_an_error_and_blocks() {
    let mut c = ctx(0.0, 0.0);
    c.last_data_ms = Some(c.now_ms + 60_000);
    let r = standard().check(&order(1.0, 100.0), &c).unwrap_err();
    assert!(r.contains("check errored"), "{r}");
}

#[test]
fn bad_numbers_block() {
    for (qty, px) in [(f64::NAN, 100.0), (1.0, f64::NAN), (1.0, 0.0), (1.0, -5.0), (f64::INFINITY, 1.0), (0.0, 100.0)] {
        let r = standard().check(&order(qty, px), &ctx(0.0, 0.0));
        assert!(r.unwrap_err().starts_with("sane_inputs"), "qty {qty} px {px}");
    }
}

struct AlwaysErrors;
impl RiskRule for AlwaysErrors {
    fn name(&self) -> &'static str {
        "always_errors"
    }
    fn check(&self, _: &OrderIntent, _: &RiskContext) -> Result<Verdict> {
        bail!("simulated failure inside a risk check")
    }
}

#[test]
fn a_failing_check_blocks() {
    let risk = RiskEngine::with_rules(vec![Box::new(SaneInputs), Box::new(AlwaysErrors)]);
    let r = risk.check(&order(1.0, 100.0), &ctx(0.0, 0.0)).unwrap_err();
    assert!(r.contains("always_errors: check errored"), "{r}");
}

#[test]
fn no_rules_means_no_trading() {
    let risk = RiskEngine::with_rules(vec![]);
    assert!(risk.check(&order(1.0, 100.0), &ctx(0.0, 0.0)).is_err());
}

#[test]
fn engine_routes_orders_through_a_failing_rule_and_blocks() {
    let events = bars_from("BTC", &trending_closes(3));
    // Sanity: with the standard rules this series produces a fill.
    let mut ok = Engine::new(EngineConfig::default());
    assert!(fills(&replay(&mut ok, &events)) >= 1);

    let risk = RiskEngine::with_rules(vec![Box::new(AlwaysErrors)]);
    let mut eng = Engine::with_risk(EngineConfig::default(), risk);
    let decisions = replay(&mut eng, &events);
    assert_eq!(fills(&decisions), 0);
    assert!(!decisions.is_empty());
    assert!(blocked_reasons(&decisions).iter().all(|r| r.contains("check errored")));
    assert_eq!(eng.portfolio.position("BTC"), 0.0);
}

#[test]
fn engine_stale_data_kill() {
    // Noise then one trend bar (the trigger), delivered ten minutes late:
    // the clock has moved on, the bar's own timestamp has not.
    let events = bars_from("BTC", &trending_closes(1));
    let (last, history) = events.split_last().unwrap();
    let mut eng = Engine::new(EngineConfig::default());
    let early = replay(&mut eng, history);
    assert_eq!(fills(&early), 0, "should not trade before the trigger bar");

    let mut clock = ReplayClock::new(0);
    clock.advance_to(last.ts() + 10 * 60_000);
    let d = eng.on_event(last, &clock).expect("strategy wants a position");
    match d {
        Decision::Blocked { reason, .. } => assert!(reason.starts_with("stale_data"), "{reason}"),
        other => panic!("expected a stale-data block, got {other:?}"),
    }
}

#[test]
fn daily_loss_limit_stops_new_entries_in_the_engine() {
    let mut cfg = EngineConfig::default();
    cfg.risk.max_daily_loss = 0.0; // any loss at all (fees) trips it
    let mut closes = trending_closes(3);
    // Reverse hard so the long loses and exits, then trend again.
    let mut px = *closes.last().unwrap();
    for _ in 0..3 {
        px *= 0.99;
        closes.push(px);
    }
    let events: Vec<Event> = bars_from("BTC", &closes);
    let mut eng = Engine::new(cfg);
    let decisions = replay(&mut eng, &events);
    assert!(blocked_reasons(&decisions).iter().any(|r| r.starts_with("max_daily_loss")));
}
