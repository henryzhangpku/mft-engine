//! `paper`: the same engine and loop as `backtest`, fed by the live websocket
//! and clocked by each event's wall-clock arrival stamp. Fills are simulated;
//! nothing is sent anywhere.
//!
//! The session log (`--events-out`) holds the warm-up history and then every
//! live event in the order the engine consumed it, each with the clock it
//! was handled at; the decision log (`--decisions-out`) holds every decision
//! with the index of the event behind it. `verify-replay` replays the first
//! and diffs against the second.
//!
//! Every decision prints one human-readable SIGNAL line (time, instrument,
//! direction, size suggestion, reason, risk status). That line is the only
//! "output" of a decision. Anyone acting on it does so by hand, outside this
//! program.

use crate::bars::{BarBuilder, BAR_MS};
use crate::clock::{format_utc, wall_now_ms, Clock, ReplayClock, TimeKey};
use crate::engine::{Decision, Engine, EngineConfig};
use crate::event::{Event, Recorded, WarmupBar};
use crate::event_loop::{self, Envelope, LoopStats};
use crate::feed::run_feed;
use crate::hyperliquid::fetch_candles;
use crate::metrics::{fnv1a, percentile};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use crate::artifacts::{write_json, write_jsonl};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Debug, Serialize)]
pub struct LatencySummary {
    pub samples: usize,
    pub p50_us: Option<f64>,
    pub p99_us: Option<f64>,
    pub max_us: Option<f64>,
}

fn summarise(ns: &[u64]) -> LatencySummary {
    let us = |v: Option<u64>| v.map(|n| n as f64 / 1_000.0);
    LatencySummary {
        samples: ns.len(),
        p50_us: us(percentile(ns, 50.0)),
        p99_us: us(percentile(ns, 99.0)),
        max_us: us(ns.iter().copied().max()),
    }
}

#[derive(Debug, Serialize)]
pub struct PaperReport {
    pub started: String,
    pub duration_secs: u64,
    pub coins: Vec<String>,
    pub strategy: String,
    /// Warm-up bars at the head of the session log.
    pub warmup_bars: usize,
    /// The clock the live engine ran on, and the key to replay it by.
    pub time_key: String,
    pub events_by_kind: std::collections::BTreeMap<&'static str, u64>,
    pub decisions: u64,
    pub decisions_fingerprint: String,
    pub fills: u64,
    pub pm_vetoes: u64,
    pub crowd_vetoes: u64,
    pub text_reductions: u64,
    pub event_to_decision_all_events: LatencySummary,
    pub event_to_decision_bar_events: LatencySummary,
    pub engine_compute_only: LatencySummary,
    pub exchange_to_receive_ms_p50: Option<i64>,
    pub exchange_to_receive_ms_p99: Option<i64>,
    pub paper_pnl_after_costs: f64,
}

/// One line a person can read and act on by hand.
pub fn signal_line(decision: &Decision) -> String {
    let (qty, target, why, risk) = match decision {
        Decision::Filled { fill, target, why, .. } => (
            fill.qty,
            *target,
            why.as_str(),
            format!("PASSED, paper fill {:+.5} @ {:.2} fee {:.4}", fill.qty, fill.px, fill.fee),
        ),
        Decision::Blocked { qty, target, why, reason, .. } => (*qty, *target, why.as_str(), format!("BLOCKED ({reason})")),
    };
    let coin = decision.coin();
    let direction = if target > 0.0 {
        "LONG"
    } else if target < 0.0 {
        "SHORT"
    } else {
        "FLAT"
    };
    format!(
        "SIGNAL {} {coin}-PERP {direction} target {target:+.0} USD (order {qty:+.5} {coin}) | reason: {why} | risk: {risk}",
        format_utc(decision.ts())
    )
}

/// Recent REST candles to fill the strategy's 60-bar window, so it can make
/// decisions from the first live bar. Each bar is returned as a `WarmupBar`
/// stamped with the time its fetch completed; the session log records them
/// first, so a replay warms up exactly as the live engine did. `minutes` of
/// zero fetches nothing (the strategy starts cold).
pub async fn fetch_warmup(coins: &[String], minutes: i64) -> Vec<WarmupBar> {
    let mut out = Vec::new();
    if minutes <= 0 {
        return out;
    }
    let now = wall_now_ms();
    for coin in coins {
        let coin_owned = coin.clone();
        let fetched = tokio::task::spawn_blocking(move || {
            fetch_candles(&coin_owned, "1m", now - minutes * BAR_MS, now)
        })
        .await;
        match fetched {
            Ok(Ok(bars)) => {
                let recv_ts = wall_now_ms();
                let complete: Vec<_> = bars.into_iter().filter(|b| b.ts <= now).collect();
                println!("[warm-up] {} historical bars for {coin}", complete.len());
                out.extend(complete.into_iter().map(|bar| WarmupBar { recv_ts, bar }));
            }
            Ok(Err(e)) => eprintln!("[warm-up] failed for {coin}: {e:#}; strategy starts cold"),
            Err(e) => eprintln!("[warm-up] task failed for {coin}: {e}"),
        }
    }
    out
}

/// One live decision, with the index (in the session log) of the event that
/// produced it. The list of these is the live decision stream that
/// `verify-replay` compares a replay against; its fingerprint is the same
/// FNV-1a over the decisions that `backtest` reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub event_index: usize,
    pub decision: Decision,
}

/// What a live session consumed and decided, in order.
pub struct SessionLog {
    /// Every event the engine consumed, warm-up first, each with the engine
    /// clock it was handled at. Written to the session log.
    pub events: Vec<Recorded>,
    pub decisions: Vec<DecisionRecord>,
    pub stats: LoopStats,
}

/// The live engine path, without the network: warm up from `warmup`, then
/// run the event loop on `rx` with a clock keyed by arrival, recording every
/// event consumed (with that clock) and every decision.
///
/// `paper` feeds it the live sources; the replay-verification tests feed it
/// a synthetic session with virtual arrival stamps. Warm-up goes through
/// `Engine::on_event` as `Event::Warmup`, the same call a replay makes.
pub async fn run_session(
    rx: mpsc::Receiver<Envelope>,
    engine: &mut Engine,
    warmup: Vec<WarmupBar>,
    deadline: Option<Instant>,
    mut on_event: impl FnMut(&Event, Option<&Decision>, &Engine),
) -> SessionLog {
    let mut clock = ReplayClock::keyed(0, TimeKey::Arrival);
    let mut events: Vec<Recorded> = Vec::new();
    let mut decisions: Vec<DecisionRecord> = Vec::new();
    for w in warmup {
        let event = Event::Warmup(w);
        let at = event.own_arrival_ts();
        clock.observe_at(&event, Some(at));
        let none = engine.on_event(&event, &clock);
        debug_assert!(none.is_none(), "warm-up cannot decide");
        events.push(Recorded { event, arrival_ts: Some(clock.now_ms()) });
    }
    let stats = event_loop::run(rx, engine, &mut clock, deadline, |event, decision, eng, now| {
        if let Some(d) = decision {
            decisions.push(DecisionRecord { event_index: events.len(), decision: d.clone() });
        }
        events.push(Recorded { event: event.clone(), arrival_ts: Some(now) });
        on_event(event, decision, eng);
    })
    .await;
    SessionLog { events, decisions, stats }
}

/// The live bar builder: forwards every raw feed event and, after each trade
/// that closes a bar, that bar. The bar inherits the arrival of the trade
/// that closed it, so its latency is measured from that frame and its clock
/// is that frame's.
pub async fn build_bars(mut raw_rx: mpsc::Receiver<Envelope>, tx: mpsc::Sender<Envelope>) {
    let mut builder = BarBuilder::new();
    while let Some(env) = raw_rx.recv().await {
        let bar = match &env.event {
            Event::Trade(t) => builder.push(t),
            _ => None,
        };
        let (received, arrival_ts) = (env.received, env.arrival_ts);
        if tx.send(env).await.is_err() {
            break;
        }
        if let Some(b) = bar {
            if tx.send(Envelope { event: Event::Bar(b), received, arrival_ts }).await.is_err() {
                break;
            }
        }
    }
}

/// The FNV-1a fingerprint of a decision list, as `backtest` computes it.
pub fn decisions_fingerprint(decisions: &[Decision]) -> String {
    format!("{:016x}", fnv1a(serde_json::to_string(decisions).unwrap_or_default().as_bytes()))
}

pub struct PaperOptions {
    pub coins: Vec<String>,
    pub duration: Duration,
    pub config: EngineConfig,
    pub out: PathBuf,
    /// Where to write every event the engine saw, warm-up included, for replay.
    pub events_out: PathBuf,
    /// Where to write the live decision stream, for `verify-replay`.
    pub decisions_out: PathBuf,
    /// Minutes of REST history to warm the strategy with (0 = start cold).
    pub warmup_minutes: i64,
    /// The sidecar's live TextSignal file, if any.
    pub text_feed: Option<PathBuf>,
    pub kalshi_every: Duration,
    /// Zero turns positioning off.
    pub positioning_every: Duration,
}

pub async fn run(opts: PaperOptions) -> Result<PaperReport> {
    let PaperOptions { coins, duration, config, out, events_out, decisions_out, warmup_minutes, text_feed, kalshi_every, positioning_every } = opts;
    let started = wall_now_ms();
    let mut engine = Engine::new(config);
    let warmup = fetch_warmup(&coins, warmup_minutes).await;
    let warmup_bars = warmup.len();

    // Live sources, all into one engine channel:
    // websocket -> bar builder; Kalshi poller; sidecar text file.
    let (raw_tx, raw_rx) = mpsc::channel::<Envelope>(10_000);
    let (tx, rx) = mpsc::channel::<Envelope>(10_000);
    let feed = tokio::spawn(run_feed(coins.clone(), raw_tx));
    let kalshi = tokio::spawn(crate::pollers::poll_kalshi(coins.clone(), tx.clone(), kalshi_every));
    let text = text_feed.map(|p| tokio::spawn(crate::pollers::tail_text_feed(p, tx.clone())));
    let crowd = (!positioning_every.is_zero())
        .then(|| tokio::spawn(crate::pollers::poll_positioning(coins.clone(), tx.clone(), positioning_every, 100)));
    let bars = tokio::spawn(build_bars(raw_rx, tx));

    println!("[paper] running for {duration:?} on {coins:?}; paper fills only, no orders are sent anywhere");
    let deadline = Instant::now() + duration;
    // A gate veto on a flat book produces no order and so no decision; log
    // it anyway, so a quiet run can be told apart from a blocked one.
    let (mut vetoes_seen, mut text_seen) = (0u64, 0u64);
    let session = run_session(rx, &mut engine, warmup, Some(deadline), |event, decision, eng| {
        if let Some(d) = decision {
            println!("{}", signal_line(d));
        }
        if let Event::Bar(b) = event {
            if eng.pm_vetoes + eng.crowd_vetoes > vetoes_seen || eng.text_reductions > text_seen {
                let z = eng.signal_z(&b.coin).map_or("n/a".into(), |z| format!("{z:+.2}"));
                println!(
                    "[gate] {} {} momentum z={z}: Kalshi vetoes {}, positioning vetoes {}, social reductions {} (no order if the gated target equals the position)",
                    format_utc(b.ts), b.coin, eng.pm_vetoes, eng.crowd_vetoes, eng.text_reductions
                );
            }
            vetoes_seen = eng.pm_vetoes + eng.crowd_vetoes;
            text_seen = eng.text_reductions;
        }
        match event {
            Event::Gap(g) => println!("[paper] GAP {} {} {}", g.coin, g.stream, g.reason),
            Event::PredictionMarket(p) => {
                let spot = eng.marks().get(&p.coin).copied().unwrap_or(f64::NAN);
                let v = eng.pm_view(&p.coin, spot, p.ts);
                let show = |x: Option<f64>, d: usize| x.map_or("n/a".into(), |x| format!("{x:.d$}"));
                println!(
                    "[kalshi] {} {} {} strikes, implied median {}, P(close > spot {spot:.1}) = {}",
                    p.coin, p.event, p.strikes.len(), show(v.median, 0), show(v.p_up, 3)
                );
            }
            Event::Positioning(p) => println!(
                "[positioning] {} {} of {} top wallets hold it, {:.1}% long by value",
                p.coin, p.wallets_holding, p.wallets_scanned, p.long_share * 100.0
            ),
            Event::TextSignal(t) if t.relevance >= 0.5 => println!(
                "[social] {} {} relevance {:.2} bullish {:.2} bearish {:.2} ({})",
                t.coin, t.post_id, t.relevance, t.bullish, t.bearish, t.scorer
            ),
            _ => {}
        }
    })
    .await;
    feed.abort();
    bars.abort();
    kalshi.abort();
    for t in [text, crowd].into_iter().flatten() {
        t.abort();
    }

    let stats = &session.stats;
    let decided: Vec<Decision> = session.decisions.iter().map(|d| d.decision.clone()).collect();
    let report = PaperReport {
        started: format_utc(started),
        duration_secs: duration.as_secs(),
        coins,
        strategy: if config.positioning.enabled {
            "v3"
        } else if config.prediction.enabled {
            "v2"
        } else {
            "v1"
        }
        .into(),
        warmup_bars,
        time_key: TimeKey::Arrival.name().into(),
        events_by_kind: stats.events_by_kind.clone(),
        decisions: stats.decisions,
        decisions_fingerprint: decisions_fingerprint(&decided),
        fills: engine.portfolio.fills,
        pm_vetoes: engine.pm_vetoes,
        crowd_vetoes: engine.crowd_vetoes,
        text_reductions: engine.text_reductions,
        event_to_decision_all_events: summarise(&stats.event_latency_ns),
        event_to_decision_bar_events: summarise(&stats.bar_latency_ns),
        engine_compute_only: summarise(&stats.engine_compute_ns),
        exchange_to_receive_ms_p50: percentile(&stats.feed_latency_ms, 50.0),
        exchange_to_receive_ms_p99: percentile(&stats.feed_latency_ms, 99.0),
        paper_pnl_after_costs: engine.equity(),
    };
    write_json(&out, &report)?;
    write_jsonl(&events_out, &session.events)?;
    write_jsonl(&decisions_out, &session.decisions)?;
    Ok(report)
}
