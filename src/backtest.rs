//! `backtest`: replay a file of events through the same engine and loop that
//! paper mode uses, then report what happened after costs.

use crate::bars::{detect_bar_gaps, read_events, read_recorded, with_built_bars, BarBuilder};
use crate::clock::{format_utc, ReplayClock, TimeKey};
use crate::engine::{Decision, Engine, EngineConfig};
use crate::event::{sort_for_replay, Event, Recorded};
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

/// Load, merge and order events for a replay keyed by exchange time: read
/// every file, build bars from any trades, add `Gap` events for missing
/// bars, sort. This is how every experiment and backfill on the ledger was
/// replayed.
pub fn load_events(paths: &[PathBuf]) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    for p in paths {
        events.extend(read_events(p)?);
    }
    if events.is_empty() {
        bail!("no events in {paths:?}");
    }
    Ok(prepare_exchange(events))
}

fn prepare_exchange(mut events: Vec<Event>) -> Vec<Event> {
    sort_for_replay(&mut events);
    let mut events = with_built_bars(events);
    let gaps = detect_bar_gaps(&events);
    events.extend(gaps.into_iter().map(Event::Gap));
    sort_for_replay(&mut events);
    events
}

/// True if, in arrival order, a bar arrives after a trade: the input already
/// holds the bars the live engine built (a `paper` session log), and
/// building them again from its trades would feed every bar twice. Warm-up
/// and backfilled bars arrive before any live trade. (Exchange-time replays
/// keep their original rule, always building, so ledger entries replay
/// unchanged: replay a `paper` log by arrival.)
fn has_live_bars(events: &[Event]) -> bool {
    let mut seen_trade = false;
    for e in events {
        match e {
            Event::Trade(_) => seen_trade = true,
            Event::Bar(_) if seen_trade => return true,
            _ => {}
        }
    }
    false
}

/// The key to replay `records` by when none is given: arrival if the files
/// carry recorded arrival times (session logs written by `paper` or
/// `record`), otherwise exchange, which every older file and every ledger
/// entry was replayed on.
pub fn auto_time_key(records: &[Recorded]) -> TimeKey {
    if records.iter().any(|r| r.arrival_ts.is_some()) {
        TimeKey::Arrival
    } else {
        TimeKey::Exchange
    }
}

/// Load a replay under a time key (`None`: `auto_time_key`). Returns the
/// envelopes in replay order and the key used.
///
/// By arrival, events are ordered by their arrival time (a stable sort, so a
/// session log, whose arrivals never decrease, keeps its exact file order)
/// and each carries that time for the clock. Bars are built from trades the
/// way the live bar builder does, in arrival order, each bar arriving with
/// the trade that closed it, unless the input already has the live bars.
/// No bar-gap events are added: the live engine only sees the gaps its feed
/// reports, and they are in the log.
pub fn load_session(paths: &[PathBuf], key: Option<TimeKey>) -> Result<(Vec<Envelope>, TimeKey)> {
    let mut records = Vec::new();
    for p in paths {
        records.extend(read_recorded(p)?);
    }
    if records.is_empty() {
        bail!("no events in {paths:?}");
    }
    let key = key.unwrap_or_else(|| auto_time_key(&records));
    let envelopes = match key {
        TimeKey::Exchange => {
            prepare_exchange(records.into_iter().map(|r| r.event).collect()).into_iter().map(Envelope::replayed).collect()
        }
        TimeKey::Arrival => prepare_arrival(records),
    };
    Ok((envelopes, key))
}

fn prepare_arrival(mut records: Vec<Recorded>) -> Vec<Envelope> {
    records.sort_by_key(Recorded::arrival);
    let live_bars = has_live_bars(&records.iter().map(|r| r.event.clone()).collect::<Vec<_>>());
    let mut builder = BarBuilder::new();
    let mut out = Vec::with_capacity(records.len());
    for r in records {
        let at = r.arrival();
        let bar = match &r.event {
            Event::Trade(t) if !live_bars => builder.push(t),
            _ => None,
        };
        out.push(Envelope::replayed_at(r.event, at));
        if let Some(b) = bar {
            out.push(Envelope::replayed_at(Event::Bar(b), at));
        }
    }
    out
}

/// The time an envelope is ordered and clocked by under `key`.
pub fn time_of(env: &Envelope, key: TimeKey) -> i64 {
    match key {
        TimeKey::Exchange => env.event.ts(),
        TimeKey::Arrival => env.arrival_ts.unwrap_or_else(|| env.event.own_arrival_ts()),
    }
}

/// Everything one replay produced.
#[derive(Debug, Clone)]
pub struct BacktestRun {
    pub report: BacktestReport,
    /// Every decision, in order.
    pub decisions: Vec<Decision>,
    /// For each decision, the index in the replayed stream of the event
    /// that produced it.
    pub decision_events: Vec<usize>,
    /// (bar time, equity) after every bar.
    pub equity: Vec<(i64, f64)>,
}

/// Replay `events` through a fresh engine, keyed by exchange time.
pub async fn run_backtest(window: &str, events: Vec<Event>, config: EngineConfig) -> BacktestRun {
    run_replay(window, events.into_iter().map(Envelope::replayed).collect(), config, TimeKey::Exchange).await
}

/// Replay envelopes, already in order, through a fresh engine with a clock
/// keyed by `key`.
pub async fn run_replay(window: &str, envelopes: Vec<Envelope>, config: EngineConfig, key: TimeKey) -> BacktestRun {
    let start_ts = envelopes.first().map_or(0, |e| time_of(e, key));
    let end_ts = envelopes.last().map_or(0, |e| time_of(e, key));
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    for e in &envelopes {
        *counts.entry(e.event.kind()).or_default() += 1;
    }

    // The replay source: a task that pushes the file into the same kind of
    // channel the live feed uses. Single producer, so order is preserved.
    let (tx, rx) = mpsc::channel::<Envelope>(4_096);
    let producer = tokio::spawn(async move {
        for env in envelopes {
            if tx.send(env).await.is_err() {
                break;
            }
        }
    });

    let mut engine = Engine::new(config);
    let mut clock = ReplayClock::keyed(start_ts, key);
    let mut decisions = Vec::new();
    let mut decision_events = Vec::new();
    let mut index = 0usize;
    let mut equity_curve = Vec::new();

    // Equity is sampled after every bar, so drawdown sees every mark, not
    // just the moments we traded.
    event_loop::run(rx, &mut engine, &mut clock, None, |event, decision, eng, _now| {
        if let Some(d) = decision {
            decisions.push(d.clone());
            decision_events.push(index);
        }
        if let Event::Bar(b) = event {
            equity_curve.push((b.ts, eng.equity()));
        }
        index += 1;
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
        decision_events,
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
    let envelopes: Vec<Envelope> = events.iter().cloned().map(Envelope::replayed).collect();
    evaluate_keyed(name, &envelopes, config, TimeKey::Exchange).await
}

/// `evaluate` under a time key; the halves are split on that key's time.
pub async fn evaluate_keyed(name: &str, envelopes: &[Envelope], config: EngineConfig, key: TimeKey) -> (Evaluation, BacktestRun) {
    let (early, late) = split_halves_keyed(envelopes, key);
    let full = run_replay("full", envelopes.to_vec(), config, key).await;
    let first_half = run_replay("first_half", early, config, key).await.report;
    let second_half = run_replay("second_half", late, config, key).await.report;
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

/// `split_halves` on the time of `key`.
pub fn split_halves_keyed(envelopes: &[Envelope], key: TimeKey) -> (Vec<Envelope>, Vec<Envelope>) {
    let (Some(first), Some(last)) = (envelopes.first(), envelopes.last()) else {
        return (vec![], vec![]);
    };
    let (t0, t1) = (time_of(first, key), time_of(last, key));
    let mid = t0 + (t1 - t0) / 2;
    let early = envelopes.iter().filter(|e| time_of(e, key) < mid).cloned().collect();
    let late = envelopes.iter().filter(|e| time_of(e, key) >= mid).cloned().collect();
    (early, late)
}
