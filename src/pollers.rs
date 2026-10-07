//! Slow live sources that sit beside the websocket feed: Kalshi ladders,
//! leaderboard positioning, and the sidecar's social file. Each is a tokio
//! task that sends `Envelope`s into the same channel as market data, so
//! `record` and `paper` see exactly the same stream. Each logs and skips a
//! failed poll; the engine's gates then see an ageing input and close.

use crate::clock::wall_now_ms;
use crate::event::Event;
use crate::event_loop::Envelope;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

/// Poll Kalshi for each coin's next-closing ladder every `every`, sending
/// each snapshot into the engine channel. A failed poll is logged and
/// skipped; the gate then sees an ageing ladder and closes on its own.
pub async fn poll_kalshi(coins: Vec<String>, tx: mpsc::Sender<Envelope>, every: Duration) {
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        for coin in &coins {
            if crate::kalshi::series_for(coin).is_none() {
                continue;
            }
            let c = coin.clone();
            let fetched = tokio::task::spawn_blocking(move || crate::kalshi::fetch_live_ladder(&c, wall_now_ms())).await;
            match fetched {
                Ok(Ok(Some(pm))) => {
                    let event = Event::PredictionMarket(pm);
                    if tx.send(Envelope::now(event)).await.is_err() {
                        return;
                    }
                }
                Ok(Ok(None)) => eprintln!("[kalshi] no usable ladder for {coin}"),
                Ok(Err(e)) => eprintln!("[kalshi] poll failed for {coin}: {e:#}"),
                Err(e) => eprintln!("[kalshi] poll task failed: {e}"),
            }
        }
    }
}

/// Follow a JSONL file of `TextSignal` events written by the sidecar
/// (`sidecar/live_social.py`), sending each new complete line into the engine
/// channel. Reads from the start: old signals expire by their own TTL.
pub async fn tail_text_feed(path: PathBuf, tx: mpsc::Sender<Envelope>) {
    let mut offset = 0usize;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        // Small file, read whole; a missing file just means "nothing yet".
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else { continue };
        if end < offset {
            continue;
        }
        for line in String::from_utf8_lossy(&bytes[offset..=end]).lines() {
            match serde_json::from_str::<Event>(line) {
                Ok(event @ Event::TextSignal(_)) => {
                    if tx.send(Envelope::now(event)).await.is_err() {
                        return;
                    }
                }
                Ok(other) => eprintln!("[text] ignoring non-text event {}", other.kind()),
                Err(e) => eprintln!("[text] bad line skipped: {e}"),
            }
        }
        offset = end + 1;
    }
}


/// Snapshot top-wallet positioning every `every`. The wallet set is chosen
/// once from the leaderboard at start; each snapshot emits one `Positioning`
/// event per coin in `coins`.
pub async fn poll_positioning(coins: Vec<String>, tx: mpsc::Sender<Envelope>, every: Duration, wallets: usize) {
    let scanner = tokio::task::spawn_blocking(move || crate::positioning::Scanner::from_leaderboard(wallets, 100_000.0)).await;
    let mut scanner = match scanner {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return eprintln!("[positioning] leaderboard unavailable, positioning off: {e:#}"),
        Err(e) => return eprintln!("[positioning] task failed: {e}"),
    };
    eprintln!("[positioning] tracking {} top wallets", scanner.wallet_count());
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        // The scan is blocking HTTP; move the scanner into a blocking task
        // and take it back afterwards.
        let result = tokio::task::spawn_blocking(move || {
            let snap = scanner.snapshot(wall_now_ms);
            (scanner, snap)
        })
        .await;
        let snap = match result {
            Ok((s, snap)) => {
                scanner = s;
                snap
            }
            Err(e) => return eprintln!("[positioning] scan task failed: {e}"),
        };
        match snap {
            Ok(by_coin) => {
                for coin in &coins {
                    let Some(p) = by_coin.get(coin) else { continue };
                    let event = Event::Positioning(p.clone());
                    if tx.send(Envelope::now(event)).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => eprintln!("[positioning] snapshot skipped: {e:#}"),
        }
    }
}
