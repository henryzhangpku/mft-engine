//! `record`: write the normalised live feed to a JSONL file.
//!
//! Each line is one `Event`. Trades and book tops carry both the exchange
//! timestamp and our receive timestamp; gaps (silences, reconnects) are
//! recorded as events in the same stream so a replay sees them in place.

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

pub async fn run(coins: Vec<String>, out: &Path, duration: Duration) -> Result<()> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Append, so restarting a recording never destroys earlier data.
    let file = OpenOptions::new().create(true).append(true).open(out)?;
    let mut w = BufWriter::new(file);

    let (tx, mut rx) = mpsc::channel::<Envelope>(10_000);
    let feed = tokio::spawn(run_feed(coins, tx));

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
                if let Event::Gap(g) = &env.event {
                    println!("gap: {} {} {}", g.coin, g.stream, g.reason);
                }
                serde_json::to_writer(&mut w, &env.event)?;
                w.write_all(b"\n")?;
                *counts.entry(env.event.kind()).or_default() += 1;
            }
        }
    }
    w.flush()?;
    feed.abort();
    println!("recorded: {counts:?}");
    Ok(())
}
