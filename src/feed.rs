//! The live market data feed: connect, subscribe, normalise, reconnect.
//!
//! Runs as its own tokio task and sends `Envelope`s down a channel. Both
//! `record` and `paper` use it, so the recorded file contains exactly what the
//! paper engine would have seen, gaps included.
//!
//! Reconnect policy: exponential backoff from 1 s, doubling to a 30 s cap,
//! reset once a connection delivers data. A connection that is silent for
//! 20 s is treated as dead (Hyperliquid's book alone ticks every ~0.5 s).
//! Every reconnect emits one `Gap` per coin covering the outage.

use crate::clock::wall_now_ms;
use crate::event::{Event, Gap};
use crate::event_loop::Envelope;
use crate::gap::GapDetector;
use crate::hyperliquid::{parse_ws_message, ping_message, subscribe_messages, WS_URL};
use anyhow::{bail, Result};
use futures_util::{SinkExt, StreamExt};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const READ_TIMEOUT: Duration = Duration::from_secs(20);
const PING_EVERY: Duration = Duration::from_secs(30);
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Run forever (until the receiver is dropped). Never returns an error for a
/// network failure; it logs, waits, and reconnects.
pub async fn run_feed(coins: Vec<String>, tx: mpsc::Sender<Envelope>) {
    let mut detector = GapDetector::new();
    let mut backoff = BACKOFF_START;
    let mut connected_before = false;

    loop {
        match connect_and_stream(&coins, &tx, &mut detector, &mut connected_before, &mut backoff).await {
            Ok(()) => return, // receiver dropped: the caller is done with us
            Err(e) => eprintln!("[feed] connection ended: {e:#}; reconnecting in {backoff:?}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// One connection's lifetime. `Ok(())` only when the receiver has gone away.
async fn connect_and_stream(
    coins: &[String],
    tx: &mpsc::Sender<Envelope>,
    detector: &mut GapDetector,
    connected_before: &mut bool,
    backoff: &mut Duration,
) -> Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async(WS_URL).await?;
    let (mut write, mut read) = ws.split();
    for msg in subscribe_messages(coins) {
        write.send(Message::Text(msg)).await?;
    }
    eprintln!("[feed] connected to {WS_URL}, subscribed trades + l2Book for {coins:?}");

    // After a reconnect, report the outage as a gap per coin before any new data.
    if *connected_before {
        let now = wall_now_ms();
        let last = detector.last_seen();
        for coin in coins {
            let gap = Gap {
                coin: coin.clone(),
                stream: "connection".into(),
                ts: now,
                from_ts: last.get(coin).copied().unwrap_or(now),
                to_ts: now,
                reason: "websocket reconnected".into(),
            };
            if send(tx, Event::Gap(gap)).await.is_err() {
                return Ok(());
            }
        }
        detector.reset();
    }
    *connected_before = true;

    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await; // the first tick fires immediately; skip it

    loop {
        tokio::select! {
            _ = ping.tick() => {
                write.send(Message::Text(ping_message())).await?;
            }
            next = tokio::time::timeout(READ_TIMEOUT, read.next()) => {
                let frame = match next {
                    Err(_) => bail!("no message for {READ_TIMEOUT:?}"),
                    Ok(None) => bail!("server closed the stream"),
                    Ok(Some(frame)) => frame?,
                };
                let text = match frame {
                    Message::Text(t) => t,
                    Message::Close(c) => bail!("server sent close: {c:?}"),
                    _ => continue, // binary, ping, pong: tungstenite answers pings itself
                };
                let received = Instant::now();
                let recv_ts = wall_now_ms();
                let events = match parse_ws_message(&text, recv_ts) {
                    Ok(evs) => evs,
                    Err(e) => {
                        // A frame we cannot parse is dropped and logged, not
                        // guessed at. The gap detector will notice if it matters.
                        eprintln!("[feed] unparseable frame dropped: {e:#}");
                        continue;
                    }
                };
                if !events.is_empty() {
                    *backoff = BACKOFF_START;
                }
                for ev in events {
                    let stream = match &ev { Event::Trade(_) => "trades", _ => "book" };
                    if let Some(gap) = detector.observe(stream, ev.coin(), ev.ts(), recv_ts) {
                        if send_at(tx, Event::Gap(gap), received).await.is_err() { return Ok(()); }
                    }
                    if send_at(tx, ev, received).await.is_err() { return Ok(()); }
                }
            }
        }
    }
}

async fn send(tx: &mpsc::Sender<Envelope>, event: Event) -> Result<(), ()> {
    send_at(tx, event, Instant::now()).await
}

async fn send_at(tx: &mpsc::Sender<Envelope>, event: Event, received: Instant) -> Result<(), ()> {
    tx.send(Envelope { event, received: Some(received) }).await.map_err(|_| ())
}
