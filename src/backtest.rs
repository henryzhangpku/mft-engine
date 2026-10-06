//! `backtest`: replay a file of events through the same engine and loop that
//! paper mode uses, then report what happened after costs.

use crate::bars::{detect_bar_gaps, read_events, with_built_bars};
use crate::clock::{format_utc, ReplayClock};
use crate::engine::{Decision, Engine, EngineConfig};
use crate::event::{sort_for_replay, Event};
use crate::event_loop::{self, Envelope};
use crate::metrics::{fnv1a, hit_rate, max_drawdown};
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BacktestReport {
    pub window: String,
    pub start: String,
    pub end: String,
    pub bars: u64,
    pub text_signals: u64,
    pub prediction_snapshots: u64,
    pub positioning_snapshots: u64,
    pub gaps: u64,
    pub fills: u64,
    pub round_trips: usize,
    /// Share of round trips with positive PnL after fees and slippage.
    pub hit_rate: Option<f64>,
    /// Final equity, USD: realised plus open positions marked at last close.
    pub pnl_after_costs: f64,
    pub pnl_before_costs: f64,
    pub fees: f64,
    pub slippage: f64,
    pub traded_notional: f64,
    /// Traded notional divided by the per-coin target notional.
    pub turnover_multiple: f64,
    pub max_drawdown: f64,
    pub blocked_orders: BTreeMap<String, u64>,
    pub text_reductions: u64,
    pub pm_vetoes: u64,
    pub crowd_vetoes: u64,
    /// FNV-1a of every decision serialised in order. Same input and config
    /// must give the same fingerprint; that is the determinism claim.
    pub decisions_fingerprint: String,
}

/// Load, merge and order events for a replay: read every file, add `Gap`
/// events for missing bars, build bars from any trades, sort.
pub fn load_events(paths: &[PathBuf]) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    for p in paths {
        events.extend(read_events(p)?);
    }
    if events.is_empty() {
        bail!("no events in {paths:?}");
    }
    sort_for_replay(&mut events);
    let mut events = with_built_bars(events);
    let gaps = detect_bar_gaps(&events);
    events.extend(gaps.into_iter().map(Event::Gap));
    sort_for_replay(&mut events);
    Ok(events)
}

/// Everything one replay produced.
#[derive(Debug, Clone)]
pub struct BacktestRun {
    pub report: BacktestReport,
    /// Every decision, in order.
    pub decisions: Vec<Decision>,
    /// (bar time, equity) after every bar.
    pub equity: Vec<(i64, f64)>,
}

/// Replay `events` through a fresh engine.
pub async fn run_backtest(window: &str, events: Vec<Event>, config: EngineConfig) -> BacktestRun {
    let start_ts = events.first().map_or(0, Event::ts);
    let end_ts = events.last().map_or(0, Event::ts);
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    for e in &events {
        *counts.entry(e.kind()).or_default() += 1;
    }

    // The replay source: a task that pushes the file into the same kind of
    // channel the live feed uses. Single producer, so order is preserved.
    let (tx, rx) = mpsc::channel::<Envelope>(4_096);
    let producer = tokio::spawn(async move {
        for ev in events {
            if tx.send(Envelope::replayed(ev)).await.is_err() {
                break;
            }
        }
    });

    let mut engine = Engine::new(config);
    let mut clock = ReplayClock::new(start_ts);
    let mut decisions = Vec::new();
    let mut equity_curve = Vec::new();

    // Equity is sampled after every bar, so drawdown sees every mark, not
    // just the moments we traded.
    event_loop::run(rx, &mut engine, &mut clock, None, |event, decision, eng| {
        if let Some(d) = decision {
            decisions.push(d.clone());
        }
        if let Event::Bar(b) = event {
            equity_curve.push((b.ts, eng.equity()));
        }
    })
    .await;
    // The producer has finished: the loop only ends when the channel closes.
    let _ = producer.await;

    let p = &engine.portfolio;
    let pnl = engine.equity();
    let mut blocked = BTreeMap::new();
    for d in &decisions {
        if let Decision::Blocked { reason, .. } = d {
            let rule = reason.split(':').next().unwrap_or("unknown").to_string();
            *blocked.entry(rule).or_insert(0) += 1;
        }
    }
    let fingerprint = fnv1a(serde_json::to_string(&decisions).unwrap_or_default().as_bytes());
    let report = BacktestReport {
        window: window.to_string(),
        start: format_utc(start_ts),
        end: format_utc(end_ts),
        bars: counts.get("Bar").copied().unwrap_or(0),
        text_signals: counts.get("TextSignal").copied().unwrap_or(0),
        prediction_snapshots: counts.get("PredictionMarket").copied().unwrap_or(0),
        positioning_snapshots: counts.get("Positioning").copied().unwrap_or(0),
        gaps: counts.get("Gap").copied().unwrap_or(0),
        fills: p.fills,
        round_trips: p.round_trips.len(),
        hit_rate: hit_rate(&p.round_trips),
        pnl_after_costs: pnl,
        pnl_before_costs: pnl + p.fees_paid + p.slippage_paid,
        fees: p.fees_paid,
        slippage: p.slippage_paid,
        traded_notional: p.traded_notional,
        turnover_multiple: p.traded_notional / engine.config().strategy.target_notional,
        max_drawdown: max_drawdown(&equity_curve.iter().map(|(_, e)| *e).chain([pnl]).collect::<Vec<_>>()),
        blocked_orders: blocked,
        text_reductions: engine.text_reductions,
        pm_vetoes: engine.pm_vetoes,
        crowd_vetoes: engine.crowd_vetoes,
        decisions_fingerprint: format!("{fingerprint:016x}"),
    };
    BacktestRun {
        report,
        decisions,
        equity: equity_curve,
    }
}

/// One strategy evaluated the standard way: the full sample, then each half.
#[derive(Debug, Clone, Serialize)]
pub struct Evaluation {
    pub name: String,
    pub full: BacktestReport,
    pub first_half: BacktestReport,
    pub second_half: BacktestReport,
}

/// Run `config` over the full sample and both halves. Returns the evaluation
/// and the full run (for its decisions and equity curve).
pub async fn evaluate(name: &str, events: &[Event], config: EngineConfig) -> (Evaluation, BacktestRun) {
    let (early, late) = split_halves(events);
    let full = run_backtest("full", events.to_vec(), config).await;
    let first_half = run_backtest("first_half", early, config).await.report;
    let second_half = run_backtest("second_half", late, config).await.report;
    let eval = Evaluation {
        name: name.to_string(),
        full: full.report.clone(),
        first_half,
        second_half,
    };
    (eval, full)
}

/// The comparison table both `backtest` and `experiment` print.
pub fn print_table(evals: &[Evaluation]) {
    println!(
        "{:<26} {:<12} {:>6} {:>7} {:>10} {:>10} {:>9} {:>8} {:>8} {:>7}",
        "strategy", "window", "fills", "hit", "pnl_net", "pnl_gross", "costs", "turn_x", "max_dd", "vetoes"
    );
    for e in evals {
        for r in [&e.full, &e.first_half, &e.second_half] {
            println!(
                "{:<26} {:<12} {:>6} {:>7} {:>10.2} {:>10.2} {:>9.2} {:>8.1} {:>8.2} {:>7}",
                e.name,
                r.window,
                r.fills,
                r.hit_rate.map_or("n/a".into(), |h| format!("{:.1}%", h * 100.0)),
                r.pnl_after_costs,
                r.pnl_before_costs,
                r.fees + r.slippage,
                r.turnover_multiple,
                r.max_drawdown,
                r.pm_vetoes + r.crowd_vetoes,
            );
        }
    }
}

/// Split events at the midpoint in time: the earlier half is the window you
/// would be allowed to look at while designing, the later half is held out.
/// The signal here was fixed before either was run; the split is reported so
/// a reader can see whether the result is stable across the two.
pub fn split_halves(events: &[Event]) -> (Vec<Event>, Vec<Event>) {
    let (Some(first), Some(last)) = (events.first(), events.last()) else {
        return (vec![], vec![]);
    };
    let mid = first.ts() + (last.ts() - first.ts()) / 2;
    let early = events.iter().filter(|e| e.ts() < mid).cloned().collect();
    let late = events.iter().filter(|e| e.ts() >= mid).cloned().collect();
    (early, late)
}
