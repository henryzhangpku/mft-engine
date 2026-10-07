//! `record`: write the normalised live feed to a JSONL file.
//!
//! Each line is one `Event`. Trades and book tops carry both the exchange
//! timestamp and our receive timestamp; gaps (silences, reconnects) are
//! recorded as events in the same stream so a replay sees them in place.
//! Optionally the slow sources too (Kalshi ladders, top-wallet positioning,
//! the sidecar's social file), so one recording is a complete replay input.
//!
//! The file starts with the same warm-up history `paper` fetches (as
//! `Warmup` events), and every line carries its arrival time, raised where
//! needed so it never decreases down the file. Replayed by arrival, a
//! recording therefore starts warm and sees its events in the order and at
//! the times they arrived.

use crate::event::{Event, Recorded};
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
    /// Minutes of REST history written first, as warm-up (0 = none).
    pub warmup_minutes: i64,
}

pub async fn run(coins: Vec<String>, out: &Path, duration: Duration, extras: Extras) -> Result<()> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Append, so restarting a recording never destroys earlier data.
    let file = OpenOptions::new().create(true).append(true).open(out)?;
    let mut w = BufWriter::new(file);

    // Warm-up first, so every live arrival comes after it.
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut last_arrival = i64::MIN;
    let mut write = |w: &mut BufWriter<std::fs::File>, event: Event, stamp: i64| -> Result<()> {
        last_arrival = last_arrival.max(stamp);
        *counts.entry(event.kind()).or_default() += 1;
        let line = serde_json::to_string(&Recorded { event, arrival_ts: Some(last_arrival) })?;
        if let Some(kind) = crate::artifacts::find_secret(&line) {
            anyhow::bail!("refusing to record a line containing a {kind}");
        }
        w.write_all(line.as_bytes())?;
        w.write_all(b"\n")?;
        Ok(())
    };
    for bar in crate::paper::fetch_warmup(&coins, extras.warmup_minutes).await {
        let at = bar.recv_ts;
        write(&mut w, Event::Warmup(bar), at)?;
    }

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
                let at = env.arrival_ts.unwrap_or_else(|| env.event.own_arrival_ts());
                write(&mut w, env.event, at)?;
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
