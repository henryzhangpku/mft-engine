//! `record`: write the normalised live feed to a JSONL file.
//!
//! Each line is one `Event`. Trades and book tops carry both the exchange
//! timestamp and our receive timestamp; gaps (silences, reconnects) are
//! recorded as events in the same stream so a replay sees them in place.
//! Optionally the slow sources too (Kalshi ladders, top-wallet positioning,
//! the sidecar's social file), so one recording is a complete replay input.

use crate::event::Event;
use crate::event_loop::Envelope;
use crate::feed::run_feed;
use anyhow::Result;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;

/// Which slow sources to record alongside the websocket. A zero interval
/// turns a poller off.
pub struct Extras {
    pub kalshi_every: Duration,
    pub positioning_every: Duration,
    pub positioning_wallets: usize,
    pub text_feed: Option<std::path::PathBuf>,
}

pub async fn run(coins: Vec<String>, out: &Path, duration: Duration, extras: Extras) -> Result<()> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Append, so restarting a recording never destroys earlier data.
    let file = OpenOptions::new().create(true).append(true).open(out)?;
    let mut w = BufWriter::new(file);

    let (tx, mut rx) = mpsc::channel::<Envelope>(10_000);
    let mut tasks = vec![tokio::spawn(run_feed(coins.clone(), tx.clone()))];
    if !extras.kalshi_every.is_zero() {
        tasks.push(tokio::spawn(crate::pollers::poll_kalshi(coins.clone(), tx.clone(), extras.kalshi_every)));
    }
    if !extras.positioning_every.is_zero() {
        let p = crate::pollers::poll_positioning(coins.clone(), tx.clone(), extras.positioning_every, extras.positioning_wallets);
        tasks.push(tokio::spawn(p));
    }
    if let Some(path) = extras.text_feed {
        tasks.push(tokio::spawn(crate::pollers::tail_text_feed(path, tx.clone())));
    }
    drop(tx);

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut flush = tokio::time::interval(Duration::from_secs(1));
    let stop = tokio::time::sleep(duration);
    tokio::pin!(stop);

    println!("recording to {} for {:?} (Ctrl-C to stop early)", out.display(), duration);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = flush.tick() => w.flush()?,
            next = rx.recv() => {
                let Some(env) = next else { break };
                match &env.event {
                    Event::Gap(g) => println!("gap: {} {} {}", g.coin, g.stream, g.reason),
                    Event::Positioning(p) => println!(
                        "positioning: {} {} of {} top wallets hold it, {:.1}% long by value ({})",
                        p.coin, p.wallets_holding, p.wallets_scanned, p.long_share * 100.0,
                        crate::positioning::crowd_label(p.long_share)
                    ),
                    Event::PredictionMarket(p) => println!("kalshi: {} {} {} strikes", p.coin, p.event, p.strikes.len()),
                    _ => {}
                }
                serde_json::to_writer(&mut w, &env.event)?;
                w.write_all(b"\n")?;
                *counts.entry(env.event.kind()).or_default() += 1;
            }
        }
    }
    w.flush()?;
    for t in tasks {
        t.abort();
    }
    println!("recorded: {counts:?}");
    Ok(())
}
