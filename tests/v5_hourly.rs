//! Strategy v5 (hourly trend): the signal on a hand-built series, the entry
//! and exit rules, hourly gap handling, determinism, the stand-alone verdict
//! rule, the forward run's restore, and that adding v5 changed nothing for
//! the strategies already on the ledger.

use mft_engine::backtest::{evaluate, run_backtest};
use mft_engine::bars::{detect_bar_gaps, HOUR_MS};
use mft_engine::engine::EngineConfig;
use mft_engine::event::{Bar, Event};
use mft_engine::experiment::{build_config, verdict_for, VerdictKind};
use mft_engine::forward::{closed_bars_after, restore, ForwardRun};
use mft_engine::strategy::{zscore, Momentum, MomentumParams};
use std::collections::{BTreeMap, VecDeque};

const T0: i64 = 1_790_000_000_000 / HOUR_MS * HOUR_MS; // an hour boundary

fn hourly_bar(coin: &str, i: i64, close: f64) -> Bar {
    let open_ts = T0 + i * HOUR_MS;
    Bar { coin: coin.into(), ts: open_ts + HOUR_MS, open_ts, open: close, high: close, low: close, close, volume: 1.0, trades: 1 }
}

/// Closes from a list of log returns, starting at 100.
fn closes_from(returns: &[f64]) -> Vec<f64> {
    let mut px = 100.0;
    let mut out = vec![px];
    for r in returns {
        px *= r.exp();
        out.push(px);
    }
    out
}

/// Alternating +a, -a log returns: no drift, sample sd known.
fn alternating(n: usize, a: f64) -> Vec<f64> {
    (0..n).map(|i| if i % 2 == 0 { a } else { -a }).collect()
}

#[test]
fn v5_parameters_are_the_preregistered_ones() {
    let p = MomentumParams::v5_hourly();
    assert_eq!((p.lookback_bars, p.vol_window_bars, p.max_hold_bars), (24, 168, 72));
    assert_eq!(p.entry_z, 1.0);
    assert_eq!(p.target_notional, 1_000.0);
    assert_eq!(p.bar_ms, HOUR_MS);
    let c = EngineConfig::v5();
    assert_eq!(c.min_order_notional, 50.0);
    assert_eq!((c.fills.taker_fee_bps, c.fills.slippage_bps), (4.5, 1.0));
    assert!(!c.prediction.enabled && !c.text.enabled && !c.positioning.enabled, "no overlays");
    assert_eq!(c.risk, EngineConfig::v1().risk, "same risk layer as v1");
    assert_eq!(build_config("v5", &BTreeMap::new()).unwrap(), c);
}

#[test]
fn signal_math_on_a_hand_built_series() {
    // 167 alternating returns of +-1%, then one of +6%: 168 returns.
    let mut rets = alternating(167, 0.01);
    rets.push(0.06);
    let closes: VecDeque<f64> = closes_from(&rets).into();
    assert_eq!(closes.len(), 169);

    // By hand: r24 is the sum of the last 24 returns. The last 23 of the
    // alternating run start at index 144 (even, +1%): 12 x +1%, 11 x -1%,
    // so +1%, plus the final +6%: r24 = 0.07.
    let r24: f64 = 0.07;
    let mean: f64 = (0.01 + 0.06) / 168.0; // 84 x +1%, 83 x -1%, 1 x +6%
    let ss: f64 = 84.0 * (0.01 - mean).powi(2) + 83.0 * (-0.01 - mean).powi(2) + (0.06 - mean).powi(2);
    let sigma = (ss / 167.0).sqrt();
    let expected = r24 / (sigma * 24f64.sqrt());

    let z = zscore(&closes, 24, 168).expect("enough history");
    assert!((z - expected).abs() < 1e-9, "z {z} vs hand {expected}");
    assert!(z > 1.0, "this series is an entry: z = {z}");

    // The strategy computes the same number from bars, and enters long.
    let mut m = Momentum::new(MomentumParams::v5_hourly());
    let mut target = 0.0;
    for (i, c) in closes.iter().enumerate() {
        target = m.on_bar(&hourly_bar("BTC", i as i64, *c));
    }
    assert!((m.last_z("BTC").unwrap() - expected).abs() < 1e-9);
    assert_eq!(target, 1_000.0);

    // One close short of the window: no opinion, flat.
    let mut m = Momentum::new(MomentumParams::v5_hourly());
    for (i, c) in closes.iter().take(168).enumerate() {
        assert_eq!(m.on_bar(&hourly_bar("BTC", i as i64, *c)), 0.0);
    }
    assert!(m.last_z("BTC").is_none());
}

fn targets(returns: &[f64]) -> Vec<f64> {
    let mut m = Momentum::new(MomentumParams::v5_hourly());
    closes_from(returns).iter().enumerate().map(|(i, c)| m.on_bar(&hourly_bar("ETH", i as i64, *c))).collect()
}

#[test]
fn exits_when_z_crosses_zero_against_the_position() {
    // Quiet, then one +5% hour: long. Then a faint downward drift. 24 hours
    // later the jump leaves the 24-hour return, which is now slightly
    // negative: z just below zero, so out, and not short.
    let mut rets = alternating(168, 0.002);
    rets.push(0.05);
    rets.extend(alternating(40, 0.002).iter().map(|r| r - 0.0002));
    let t = targets(&rets);
    let entry = 169; // the close after the jump
    assert_eq!(t[entry - 1], 0.0);
    assert!(t[entry..entry + 24].iter().all(|x| *x == 1_000.0), "held while the move is in the window");
    let exit = (entry + 24..entry + 27).find(|i| t[*i] == 0.0).expect("exited on the zero cross");
    assert!(exit - entry < 72, "a zero-cross exit, not the time stop");
    assert!(t[exit..].iter().all(|x| *x == 0.0), "no new entry in a quiet market");
}

#[test]
fn exits_after_72_bars_then_may_reenter() {
    // A steady uptrend: z stays far above zero, so only the time stop ends it.
    let mut rets = alternating(168, 0.002);
    rets.extend((0..200).map(|i| 0.003 + if i % 2 == 0 { 0.002 } else { -0.002 }));
    let t = targets(&rets);
    let entry = t.iter().position(|x| *x != 0.0).expect("entered");
    assert_eq!(t[entry], 1_000.0);
    assert!(t[entry..entry + 72].iter().all(|x| *x == 1_000.0), "72 bars long");
    assert_eq!(t[entry + 72], 0.0, "time stop at 72 bars held");
    assert_eq!(t[entry + 73], 1_000.0, "re-entered: the signal is still on");
}

#[test]
fn short_entry_mirrors_long() {
    let mut rets = alternating(168, 0.002);
    rets.push(-0.05);
    let t = targets(&rets);
    assert_eq!(*t.last().unwrap(), -1_000.0);
}

#[test]
fn a_missing_hour_is_a_gap_and_resets_the_window() {
    let events: Vec<Event> = (0..10).filter(|i| *i != 5).map(|i| Event::Bar(hourly_bar("BTC", i, 100.0))).collect();
    let gaps = detect_bar_gaps(&events);
    assert_eq!(gaps.len(), 1);
    assert!(gaps[0].reason.contains("1 missing 1h bars"), "{}", gaps[0].reason);
    let contiguous: Vec<Event> = (0..10).map(|i| Event::Bar(hourly_bar("BTC", i, 100.0))).collect();
    assert!(detect_bar_gaps(&contiguous).is_empty(), "contiguous hourly bars are not gaps");

    // The strategy itself also refuses a return across a missing hour.
    let rets = alternating(168, 0.002);
    let closes = closes_from(&rets);
    let mut m = Momentum::new(MomentumParams::v5_hourly());
    for (i, c) in closes.iter().enumerate() {
        m.on_bar(&hourly_bar("BTC", i as i64, *c));
    }
    assert!(m.last_z("BTC").is_some());
    m.on_bar(&hourly_bar("BTC", closes.len() as i64 + 1, 120.0)); // skips an hour
    assert!(m.last_z("BTC").is_none(), "history discarded at the hole");
}

/// Two coins of pseudo-random hourly bars (fixed LCG, so the test is fixed).
fn synthetic(hours: i64) -> Vec<Event> {
    let mut seed: u64 = 20261009;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 33) as f64 / (1u64 << 31) as f64) - 0.5
    };
    let mut events = Vec::new();
    let (mut btc, mut eth) = (100_000.0_f64, 4_000.0_f64);
    for i in 0..hours {
        btc *= (0.012 * next() + 0.0005 * ((i / 100) % 2 * 2 - 1) as f64).exp();
        eth *= (0.016 * next()).exp();
        events.push(Event::Bar(hourly_bar("BTC", i, btc)));
        events.push(Event::Bar(hourly_bar("ETH", i, eth)));
    }
    mft_engine::event::sort_for_replay(&mut events);
    events
}

#[tokio::test]
async fn replay_is_deterministic() {
    let events = synthetic(900);
    let a = run_backtest("full", events.clone(), EngineConfig::v5()).await;
    let b = run_backtest("full", events, EngineConfig::v5()).await;
    assert!(a.report.fills > 0, "the synthetic series trades");
    assert_eq!(a.report.decisions_fingerprint, b.report.decisions_fingerprint);
    assert_eq!(a.decisions, b.decisions);
    assert_eq!(a.report.pnl_after_costs, b.report.pnl_after_costs);
    assert!(a.report.sharpe_daily.is_some(), "37 days: a daily Sharpe exists");
}

#[tokio::test]
async fn all_windows_verdict_needs_every_window_positive() {
    let (eval, _) = evaluate("v5", &synthetic(900), EngineConfig::v5()).await;
    let all_positive = [&eval.full, &eval.first_half, &eval.second_half].iter().all(|r| r.pnl_after_costs > 0.0);
    let v = verdict_for(VerdictKind::PositiveAllWindows, &eval, None);
    assert_eq!(v, if all_positive { "kept" } else { "killed" });
    let mut forced = eval.clone();
    forced.full.pnl_after_costs = 1.0;
    forced.first_half.pnl_after_costs = 1.0;
    forced.second_half.pnl_after_costs = -0.01;
    assert_eq!(verdict_for(VerdictKind::PositiveAllWindows, &forced, None), "killed");
    forced.second_half.pnl_after_costs = 0.01;
    assert_eq!(verdict_for(VerdictKind::PositiveAllWindows, &forced, None), "kept");
    // The baseline rule is untouched: a first variant is the baseline.
    assert_eq!(verdict_for(VerdictKind::HoldoutVsBaseline, &forced, None), "baseline");
}

#[test]
fn v1_config_serialises_exactly_as_before() {
    // bar_ms is left out for minute strategies, so every config on the
    // ledger, and its hash, is unchanged by adding v5.
    let json = serde_json::to_string(&EngineConfig::v1()).unwrap();
    assert!(!json.contains("bar_ms"), "{json}");
    let back: EngineConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back, EngineConfig::v1());
    assert!(serde_json::to_string(&EngineConfig::v5()).unwrap().contains("\"bar_ms\":3600000"));
}

#[test]
fn forward_run_takes_only_closed_new_bars_and_restores_exactly() {
    let bars: Vec<Bar> = (0..5).map(|i| hourly_bar("BTC", i, 100.0 + i as f64)).collect();
    // At the close of bar 3 (ts of bar 3), bar 4 is still forming.
    let now = bars[3].ts;
    let got = closed_bars_after(bars.clone(), Some(bars[1].open_ts), now);
    assert_eq!(got.iter().map(|b| b.open_ts).collect::<Vec<_>>(), vec![bars[2].open_ts, bars[3].open_ts]);

    // Live: consume a session and write it as the forward run does; a
    // restore from the file reaches the same state.
    let dir = std::env::temp_dir().join(format!("mft_v5_forward_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let session = dir.join("session.jsonl");
    let mut live = ForwardRun::new(EngineConfig::v5());
    let mut lines = String::new();
    for (k, e) in synthetic(400).into_iter().enumerate() {
        let arrival = e.ts() + 15_000 + (k % 3) as i64; // fetched 15 s after the close
        let (_, at) = live.consume(&e, arrival);
        lines += &serde_json::to_string(&mft_engine::event::Recorded { event: e, arrival_ts: Some(at) }).unwrap();
        lines.push('\n');
    }
    std::fs::write(&session, lines).unwrap();
    let restored = restore(EngineConfig::v5(), &session).unwrap();
    assert!(live.engine.portfolio.fills > 0);
    assert_eq!(restored.engine.portfolio.fills, live.engine.portfolio.fills);
    assert_eq!(restored.engine.equity(), live.engine.equity());
    assert_eq!(restored.last_open, live.last_open);
    assert_eq!(restored.daily_csv(), live.daily_csv());
    let _ = std::fs::remove_dir_all(&dir);
}
