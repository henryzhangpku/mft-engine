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
use std::path::Path;
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
    pub events_by_kind: std::collections::BTreeMap<&'static str, u64>,
    pub decisions: u64,
    pub fills: u64,
    pub event_to_decision_all_events: LatencySummary,
    pub event_to_decision_bar_events: LatencySummary,
    pub exchange_to_receive_ms_p50: Option<i64>,
    pub exchange_to_receive_ms_p99: Option<i64>,
    pub paper_pnl_after_costs: f64,
}

/// One line a person can read and act on by hand.
pub fn signal_line(decision: &Decision, engine: &Engine) -> String {
    let (coin, ts, qty, target, raw, risk) = match decision {
        Decision::Filled { fill, raw_target, target } => (
            fill.coin.as_str(),
            fill.ts,
            fill.qty,
            *target,
            *raw_target,
            format!("PASSED, paper fill {:+.5} @ {:.2} fee {:.4}", fill.qty, fill.px, fill.fee),
        ),
        Decision::Blocked { coin, ts, qty, raw_target, target, reason } => {
            (coin.as_str(), *ts, *qty, *target, *raw_target, format!("BLOCKED ({reason})"))
        }
    };
    let direction = if target > 0.0 {
        "LONG"
    } else if target < 0.0 {
        "SHORT"
    } else {
        "FLAT"
    };
    let z = engine.signal_z(coin).map_or("n/a".to_string(), |z| format!("{z:+.2}"));
    let text = if target != raw {
        format!(", text signal cut target from {raw:+.0} to {target:+.0}")
    } else {
        String::new()
    };
    format!(
        "SIGNAL {} {coin}-PERP {direction} target {target:+.0} USD (order {qty:+.5} {coin}) | reason: 5m momentum z={z}{text} | risk: {risk}",
        format_utc(ts)
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

pub async fn run(coins: Vec<String>, duration: Duration, config: EngineConfig, out: &Path) -> Result<PaperReport> {
    let started = wall_now_ms();
    let mut engine = Engine::new(config);
    warm_up(&mut engine, &coins).await;

    // Live source: websocket -> bar builder -> engine channel.
    let (raw_tx, mut raw_rx) = mpsc::channel::<Envelope>(10_000);
    let (tx, rx) = mpsc::channel::<Envelope>(10_000);
    let feed = tokio::spawn(run_feed(coins.clone(), raw_tx));
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
    let stats: LoopStats = event_loop::run(rx, &mut engine, &mut clock, Some(deadline), |event, decision, eng| {
        if let Some(d) = decision {
            println!("{}", signal_line(d, eng));
        }
        if let Event::Gap(g) = event {
            println!("[paper] GAP {} {} {}", g.coin, g.stream, g.reason);
        }
    })
    .await;
    feed.abort();
    bars.abort();

    let report = PaperReport {
        started: format_utc(started),
        duration_secs: duration.as_secs(),
        coins,
        events_by_kind: stats.events_by_kind.clone(),
        decisions: stats.decisions,
        fills: engine.portfolio.fills,
        event_to_decision_all_events: summarise(&stats.event_latency_ns),
        event_to_decision_bar_events: summarise(&stats.bar_latency_ns),
        exchange_to_receive_ms_p50: percentile(&stats.feed_latency_ms, 50.0),
        exchange_to_receive_ms_p99: percentile(&stats.feed_latency_ms, 99.0),
        paper_pnl_after_costs: engine.equity(),
    };
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(out, serde_json::to_string_pretty(&report)?)?;
    Ok(report)
}
