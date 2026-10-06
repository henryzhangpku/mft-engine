//! `paper`: the same engine and loop as `backtest`, fed by the live websocket
//! and timed by the wall clock. Fills are simulated; nothing is sent anywhere.
//!
//! Every decision prints one human-readable SIGNAL line (time, instrument,
//! direction, size suggestion, reason, risk status). That line is the only
//! "output" of a decision. Anyone acting on it does so by hand, outside this
//! program.

use crate::bars::{BarBuilder, BAR_MS};
use crate::clock::{format_utc, wall_now_ms, WallClock};
use crate::engine::{Decision, Engine, EngineConfig};
use crate::event::Event;
use crate::event_loop::{self, Envelope, LoopStats};
use crate::feed::run_feed;
use crate::hyperliquid::fetch_candles;
use crate::metrics::percentile;
use anyhow::Result;
use serde::Serialize;
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
    pub events_by_kind: std::collections::BTreeMap<&'static str, u64>,
    pub decisions: u64,
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

/// Fill the strategy's 60-bar window from recent REST candles so it can make
/// decisions from the first live bar. History only touches strategy state.
async fn warm_up(engine: &mut Engine, coins: &[String]) {
    let now = wall_now_ms();
    for coin in coins {
        let coin_owned = coin.clone();
        let fetched = tokio::task::spawn_blocking(move || {
            fetch_candles(&coin_owned, "1m", now - 90 * BAR_MS, now)
        })
        .await;
        match fetched {
            Ok(Ok(bars)) => {
                let complete: Vec<_> = bars.into_iter().filter(|b| b.ts <= now).collect();
                println!("[paper] warm-up: {} historical bars for {coin}", complete.len());
                for b in &complete {
                    engine.warm_up(b);
                }
            }
            Ok(Err(e)) => eprintln!("[paper] warm-up failed for {coin}: {e:#}; strategy starts cold"),
            Err(e) => eprintln!("[paper] warm-up task failed for {coin}: {e}"),
        }
    }
}

pub struct PaperOptions {
    pub coins: Vec<String>,
    pub duration: Duration,
    pub config: EngineConfig,
    pub out: PathBuf,
    /// Where to write every event the engine saw, for replay.
    pub events_out: PathBuf,
    /// The sidecar's live TextSignal file, if any.
    pub text_feed: Option<PathBuf>,
    pub kalshi_every: Duration,
    /// Zero turns positioning off.
    pub positioning_every: Duration,
}

pub async fn run(opts: PaperOptions) -> Result<PaperReport> {
    let PaperOptions { coins, duration, config, out, events_out, text_feed, kalshi_every, positioning_every } = opts;
    let started = wall_now_ms();
    let mut engine = Engine::new(config);
    warm_up(&mut engine, &coins).await;

    // Live sources, all into one engine channel:
    // websocket -> bar builder; Kalshi poller; sidecar text file.
    let (raw_tx, mut raw_rx) = mpsc::channel::<Envelope>(10_000);
    let (tx, rx) = mpsc::channel::<Envelope>(10_000);
    let feed = tokio::spawn(run_feed(coins.clone(), raw_tx));
    let kalshi = tokio::spawn(crate::pollers::poll_kalshi(coins.clone(), tx.clone(), kalshi_every));
    let text = text_feed.map(|p| tokio::spawn(crate::pollers::tail_text_feed(p, tx.clone())));
    let crowd = (!positioning_every.is_zero())
        .then(|| tokio::spawn(crate::pollers::poll_positioning(coins.clone(), tx.clone(), positioning_every, 100)));
    let bars = tokio::spawn(async move {
        let mut builder = BarBuilder::new();
        while let Some(env) = raw_rx.recv().await {
            let bar = match &env.event {
                Event::Trade(t) => builder.push(t),
                _ => None,
            };
            let received = env.received;
            if tx.send(env).await.is_err() {
                break;
            }
            if let Some(b) = bar {
                // The bar inherits the arrival instant of the trade that
                // closed it, so its latency is measured from that frame.
                if tx.send(Envelope { event: Event::Bar(b), received }).await.is_err() {
                    break;
                }
            }
        }
    });

    println!("[paper] running for {duration:?} on {coins:?}; paper fills only, no orders are sent anywhere");
    let mut clock = WallClock;
    let deadline = Instant::now() + duration;
    let mut seen: Vec<Event> = Vec::new();
    // A gate veto on a flat book produces no order and so no decision; log
    // it anyway, so a quiet run can be told apart from a blocked one.
    let (mut vetoes_seen, mut text_seen) = (0u64, 0u64);
    let stats: LoopStats = event_loop::run(rx, &mut engine, &mut clock, Some(deadline), |event, decision, eng| {
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
        seen.push(event.clone());
    })
    .await;
    feed.abort();
    bars.abort();
    kalshi.abort();
    for t in [text, crowd].into_iter().flatten() {
        t.abort();
    }

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
        events_by_kind: stats.events_by_kind.clone(),
        decisions: stats.decisions,
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
    write_jsonl(&events_out, &seen)?;
    Ok(report)
}
