//! Strategy v4 (funding carry): hourly funding accrual and its sign,
//! dollar-neutral rebalancing, point-in-time windows, deterministic replay,
//! and the out-of-sample seal.

use mft_engine::carry::{self, funding_payment, CarryParams, Panel, SplitMix64, Weighting, DAY_MS, HOUR_MS};
use mft_engine::carry_research::{self, Options, Phase};
use mft_engine::event::{Bar, Event};
use mft_engine::execution::FillModel;
use mft_engine::hyperliquid::FundingRecord;
use mft_engine::ledger;
use std::path::PathBuf;

/// A UTC midnight, so daily marks line up.
const T0: i64 = 1_790_000_000_000 - (1_790_000_000_000 % DAY_MS);

fn bar(coin: &str, hour: i64, close: f64) -> Bar {
    let open_ts = T0 + (hour - 1) * HOUR_MS;
    Bar { coin: coin.into(), ts: open_ts + HOUR_MS, open_ts, open: close, high: close, low: close, close, volume: 1.0, trades: 1 }
}

fn funding(coin: &str, hour: i64, rate: f64) -> FundingRecord {
    // Settled a few ms after the hour, as on the exchange.
    FundingRecord { coin: coin.into(), ts: T0 + hour * HOUR_MS + 37, rate, premium: 0.0 }
}

/// `n` coins over `days` days: random-walk prices, and funding that differs
/// by coin (coin k pays k * 1e-5 per hour plus noise).
fn synthetic(n: usize, days: i64, seed: u64) -> (Vec<Bar>, Vec<FundingRecord>) {
    let mut rng = SplitMix64(seed);
    let mut u = || (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
    let (mut bars, mut rates) = (vec![], vec![]);
    for k in 0..n {
        let coin = format!("C{k:02}");
        let mut px = 10.0 + k as f64;
        for h in 0..=days * 24 {
            px *= 1.0 + 0.01 * u();
            bars.push(bar(&coin, h, px));
            rates.push(funding(&coin, h, k as f64 * 1e-5 + 2e-5 * u()));
        }
    }
    (bars, rates)
}

fn no_costs() -> CarryParams {
    CarryParams { fills: FillModel { taker_fee_bps: 0.0, slippage_bps: 0.0 }, min_trade_notional: 0.0, ..CarryParams::default() }
}

#[test]
fn positive_funding_means_longs_pay_shorts() {
    assert_eq!(funding_payment(2.0, 100.0, 0.0001), -0.02, "a long pays");
    assert_eq!(funding_payment(-2.0, 100.0, 0.0001), 0.02, "a short receives");
    assert_eq!(funding_payment(2.0, 100.0, -0.0001), 0.02, "negative funding: the long is paid");
}

#[test]
fn funding_accrues_every_hour_on_held_positions() {
    // Two coins, flat prices, constant funding: HI pays +0.01%/h, LO -0.01%/h.
    // Bucket of one: short HI, long LO, $500 each side. Both legs are paid
    // 0.01% of $500 = $0.05 per hour, for 24 hours of a one-day window.
    let mut bars = vec![];
    let mut rates = vec![];
    for h in 0..=96 {
        bars.push(bar("HI", h, 100.0));
        bars.push(bar("LO", h, 50.0));
        rates.push(funding("HI", h, 1e-4));
        rates.push(funding("LO", h, -1e-4));
    }
    let panel = Panel::build(&bars, &rates).unwrap();
    let p = CarryParams { bucket_size: 1, gross_notional: 1_000.0, lookback_hours: 24, ..no_costs() };
    let run = carry::run(&panel, &p, T0 + 48 * HOUR_MS, T0 + 72 * HOUR_MS).unwrap();
    let opened = &run.rebalances[0];
    assert_eq!(opened.shorts[0].0, "HI");
    assert_eq!(opened.longs[0].0, "LO");
    // 24 settlements after the open (hours 49..=72); none at the open itself.
    assert!((run.funding_pnl - 24.0 * 2.0 * 0.05).abs() < 1e-9, "funding {}", run.funding_pnl);
    assert!((run.pnl() - run.funding_pnl).abs() < 1e-9, "flat prices, no costs: P&L is all funding");
    // The equity curve steps up by $0.10 every hour.
    let steps: Vec<f64> = run.equity.windows(2).map(|w| w[1].1 - w[0].1).collect();
    assert!(steps.iter().all(|s| (s - 0.10).abs() < 1e-9), "{steps:?}");
}

#[test]
fn funding_settled_a_few_ms_after_the_hour_snaps_to_it() {
    assert_eq!(carry::funding_hour(T0 + 5 * HOUR_MS + 45), T0 + 5 * HOUR_MS);
    assert_eq!(carry::funding_hour(T0 + 5 * HOUR_MS - 20), T0 + 5 * HOUR_MS);
}

#[test]
fn rebalances_are_dollar_neutral_with_the_stated_gross() {
    let (bars, rates) = synthetic(14, 12, 7);
    let panel = Panel::build(&bars, &rates).unwrap();
    for weighting in [Weighting::Equal, Weighting::InverseVol] {
        let p = CarryParams { bucket_size: 4, weighting, ..no_costs() };
        let run = carry::run(&panel, &p, T0 + 8 * DAY_MS, T0 + 12 * DAY_MS).unwrap();
        let opening: Vec<_> = run.rebalances.iter().filter(|r| !r.longs.is_empty()).collect();
        assert_eq!(opening.len(), 4, "daily rebalances inside the window");
        for r in opening {
            assert!((r.long_notional - 5_000.0).abs() < 1e-6, "long side {}", r.long_notional);
            assert!((r.short_notional - 5_000.0).abs() < 1e-6, "short side {}", r.short_notional);
            assert_eq!(r.shorts.len(), 4);
            assert_eq!(r.longs.len(), 4);
            // Shorts are the highest funding, longs the lowest.
            let min_short = r.shorts.iter().map(|s| s.1).fold(f64::INFINITY, f64::min);
            let max_long = r.longs.iter().map(|s| s.1).fold(f64::NEG_INFINITY, f64::max);
            assert!(min_short > max_long);
        }
        let last = run.rebalances.last().unwrap();
        assert_eq!((last.long_notional, last.short_notional), (0.0, 0.0), "everything is closed at the window end");
    }
}

#[test]
fn costs_are_charged_on_every_rebalance_trade() {
    let (bars, rates) = synthetic(14, 12, 7);
    let panel = Panel::build(&bars, &rates).unwrap();
    let p = CarryParams { bucket_size: 4, min_trade_notional: 0.0, ..CarryParams::default() };
    let run = carry::run(&panel, &p, T0 + 8 * DAY_MS, T0 + 12 * DAY_MS).unwrap();
    let fees: f64 = run.rebalances.iter().flat_map(|r| &r.trades).map(|t| t.fee).sum();
    assert!((fees - run.fees).abs() < 1e-9);
    assert!((run.fees - run.traded_notional * 4.5e-4).abs() < 1e-6, "4.5 bp taker fee on traded notional");
    let free = carry::run(&panel, &no_costs(), T0 + 8 * DAY_MS, T0 + 12 * DAY_MS).unwrap();
    assert!(run.pnl() < free.pnl(), "costs only ever reduce P&L");
}

#[test]
fn in_sample_result_cannot_depend_on_later_data() {
    // Cut the panel at the split, or change every number after it: the
    // in-sample run is identical either way.
    let (bars, rates) = synthetic(12, 20, 3);
    let panel = Panel::build(&bars, &rates).unwrap();
    let split = T0 + 14 * DAY_MS;
    let p = CarryParams { bucket_size: 3, ..CarryParams::default() };
    let a = carry::run(&panel, &p, T0 + 7 * DAY_MS, split).unwrap();
    let b = carry::run(&panel.truncated_after(split), &p, T0 + 7 * DAY_MS, split).unwrap();
    let mut scrambled = panel.clone();
    let k = scrambled.index_of(split).unwrap();
    for c in 0..scrambled.coins.len() {
        for i in k + 1..scrambled.hours.len() {
            scrambled.close[c][i] = Some(1.0 + c as f64);
            scrambled.funding[c][i] = Some(-0.01);
        }
    }
    let c = carry::run(&scrambled, &p, T0 + 7 * DAY_MS, split).unwrap();
    assert_eq!(a, b);
    assert_eq!(a, c);
}

#[test]
fn replay_is_deterministic_and_fingerprinted() {
    let (bars, rates) = synthetic(12, 20, 11);
    let panel = Panel::build(&bars, &rates).unwrap();
    let p = CarryParams { bucket_size: 3, ..CarryParams::default() };
    let a = carry::run(&panel, &p, T0 + 7 * DAY_MS, T0 + 20 * DAY_MS).unwrap();
    let b = carry::run(&panel, &p, T0 + 7 * DAY_MS, T0 + 20 * DAY_MS).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.fingerprint(), b.fingerprint());
    let other = carry::run(&panel, &CarryParams { bucket_size: 2, ..p }, T0 + 7 * DAY_MS, T0 + 20 * DAY_MS).unwrap();
    assert_ne!(a.fingerprint(), other.fingerprint(), "a different config trades differently");
    let ra = carry::report("x", &a, 10_000.0, 5, 2_000, 42);
    let rb = carry::report("x", &b, 10_000.0, 5, 2_000, 42);
    assert_eq!(ra, rb, "the bootstrap is seeded, so reports repeat exactly");
}

#[test]
fn block_bootstrap_brackets_the_mean() {
    let values: Vec<f64> = (0..60).map(|i| if i % 3 == 0 { 3.0 } else { -0.5 }).collect();
    let mean = values.iter().sum::<f64>() / 60.0;
    let (lo, hi, below) = carry::block_bootstrap_mean(&values, 5, 5_000, 1).unwrap();
    assert!(lo < mean && mean < hi, "{lo} {mean} {hi}");
    assert!((0.0..=1.0).contains(&below));
    assert!(carry::block_bootstrap_mean(&[1.0], 5, 100, 1).is_none());
}

/// Write a synthetic data set, universe file and spec to a scratch folder.
fn scratch_spec(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("mft-engine-carry-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (bars, rates) = synthetic(12, 20, 5);
    let events: Vec<Event> = bars.into_iter().map(Event::Bar).collect();
    mft_engine::bars::write_events(&dir.join("bars.jsonl"), &events).unwrap();
    mft_engine::artifacts::write_jsonl(&dir.join("funding.jsonl"), &rates).unwrap();
    let coins: Vec<String> = (0..12).map(|k| format!("C{k:02}")).collect();
    let universe = serde_json::json!({
        "rule": "test", "window_start": mft_engine::clock::format_utc(T0),
        "formation_end": mft_engine::clock::format_utc(T0 + 7 * DAY_MS),
        "window_end": mft_engine::clock::format_utc(T0 + 20 * DAY_MS), "universe": coins,
    });
    mft_engine::artifacts::write_json(&dir.join("universe.json"), &universe).unwrap();
    let spec = |bucket: usize| {
        format!(
            r#"name = "v4_test"
hypothesis = "test"
rationale = "test"
bars = "{d}/bars.jsonl"
funding = "{d}/funding.jsonl"
universe = "{d}/universe.json"
benchmark = "C00"
in_sample_fraction = 0.6
max_design_choices = 1
kill_rule = "killed if OOS net Sharpe <= 0 or net P&L <= 0"
capital_note = "test"
[bootstrap]
block_days = 3
resamples = 500
seed = 9
[params]
lookback_hours = 72
rebalance_every_hours = 24
bucket_size = {bucket}
weighting = "equal"
vol_lookback_hours = 168
gross_notional = 10000.0
min_trade_notional = 50.0
min_funding_coverage = 0.9
stale_price_hours = 2
fills = {{ taker_fee_bps = 4.5, slippage_bps = 3.0 }}
"#,
            d = dir.display().to_string().replace('\\', "/")
        )
    };
    std::fs::write(dir.join("spec.toml"), spec(3)).unwrap();
    std::fs::write(dir.join("spec_b2.toml"), spec(2)).unwrap();
    std::fs::write(dir.join("spec_b4.toml"), spec(4)).unwrap();
    (dir.join("spec.toml"), dir.join("ledger.jsonl"), dir)
}

fn opts<'a>(spec: &'a std::path::Path, ledger: &'a std::path::Path, dir: &'a std::path::Path, note: Option<&str>, force: bool) -> Options<'a> {
    Options { spec, ledger, note: note.map(String::from), force, results_dir: dir }
}

#[test]
fn oos_is_sealed_after_one_run_and_a_forced_rerun_is_recorded() {
    let (spec, ledger_path, dir) = scratch_spec("seal");
    let run = |phase, note: Option<&str>, force| carry_research::run(phase, opts(&spec, &ledger_path, &dir, note, force));

    // Nothing runs before the specification is on the ledger.
    assert!(run(Phase::InSample, None, false).unwrap_err().to_string().contains("pre-registration"));
    assert!(run(Phase::Oos, None, false).is_err());
    run(Phase::Preregister, None, false).unwrap();
    assert!(run(Phase::Preregister, None, false).unwrap_err().to_string().contains("already pre-registered"));
    // The sealed window comes after an in-sample run of the same config.
    assert!(run(Phase::Oos, None, false).unwrap_err().to_string().contains("no in-sample run"));
    run(Phase::InSample, None, false).unwrap();

    let first = run(Phase::Oos, None, false).unwrap();
    assert!(first.verdict == "kept" || first.verdict == "killed");
    let err = run(Phase::Oos, None, false).unwrap_err().to_string();
    assert!(err.contains("already run") && err.contains("--force"), "{err}");
    assert!(run(Phase::Oos, None, true).unwrap_err().to_string().contains("--note"), "force needs a reason");
    // In-sample design is closed once the seal is broken.
    assert!(run(Phase::InSample, Some("late idea"), false).is_err());

    let forced = run(Phase::Oos, Some("post-OOS bug fix: test"), true).unwrap();
    assert!(forced.verdict.starts_with("oos_forced_rerun_"), "{}", forced.verdict);
    assert_eq!(forced.result["forced"], true);
    assert_eq!(forced.result["fingerprint"], first.result["fingerprint"], "same code, same data, same trades");

    let entries = ledger::read(&ledger_path).unwrap();
    assert_eq!(ledger::verify(&entries).unwrap(), 4, "preregistered, in-sample, OOS, forced OOS");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn design_choices_are_logged_and_capped() {
    let (spec, ledger_path, dir) = scratch_spec("design");
    let b2 = dir.join("spec_b2.toml");
    let b4 = dir.join("spec_b4.toml");
    carry_research::run(Phase::Preregister, opts(&spec, &ledger_path, &dir, None, false)).unwrap();
    carry_research::run(Phase::InSample, opts(&spec, &ledger_path, &dir, None, false)).unwrap();
    // A changed config is a design choice: it needs a note.
    assert!(carry_research::run(Phase::InSample, opts(&b2, &ledger_path, &dir, None, false)).is_err());
    let e = carry_research::run(Phase::InSample, opts(&b2, &ledger_path, &dir, Some("smaller buckets"), false)).unwrap();
    assert_eq!(e.verdict, "design_choice");
    // The spec allows one; a second is refused.
    let err = carry_research::run(Phase::InSample, opts(&b4, &ledger_path, &dir, Some("bigger"), false)).unwrap_err();
    assert!(err.to_string().contains("maximum"), "{err}");
    // The OOS must use the config last run in-sample.
    assert!(carry_research::run(Phase::Oos, opts(&spec, &ledger_path, &dir, None, false)).unwrap_err().to_string().contains("last run in-sample"));
    let _ = std::fs::remove_dir_all(&dir);
}
