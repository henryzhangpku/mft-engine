//! The one event loop.
//!
//! Backtest and paper both run this function. The only things that differ are
//! what feeds the channel (a file replay task or the live websocket task) and
//! which `Clock` is passed in. The loop itself is four steps per event:
//! advance the clock, call the engine, measure, report.
//!
//! Latency is measured from `Envelope::received` (the instant the websocket
//! frame was read) to the moment `Engine::on_event` returns, i.e. the time to
//! reach a decision, including the risk check and the paper fill. Replayed
//! events carry no `received` instant and are not timed.

use crate::clock::Clock;
use crate::engine::{Decision, Engine};
use crate::event::Event;
use std::collections::BTreeMap;
use std::time::Instant;
use tokio::sync::mpsc;

/// An event plus the local instant it arrived, if it arrived live.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub event: Event,
    pub received: Option<Instant>,
}

impl Envelope {
    pub fn replayed(event: Event) -> Self {
        Self { event, received: None }
    }
}

#[derive(Debug, Default, Clone)]
pub struct LoopStats {
    pub events_by_kind: BTreeMap<&'static str, u64>,
    /// Receive-to-decision latency for every live event, in nanoseconds.
    pub event_latency_ns: Vec<u64>,
    /// The same, only for bar events (the ones that can produce an order).
    pub bar_latency_ns: Vec<u64>,
    /// Time spent inside `Engine::on_event` alone, for live events. The gap
    /// between this and `event_latency_ns` is channel hops and queueing.
    pub engine_compute_ns: Vec<u64>,
    /// Local receive time minus exchange time, ms, for live trades and books.
    /// Includes any offset between our clock and the exchange's.
    pub feed_latency_ms: Vec<i64>,
    pub decisions: u64,
}

/// Run until the channel closes or `deadline` passes. `after_event` is called
/// after every event (with the decision, if any) once it has been timed, so
/// printing or recording an equity curve does not count towards latency.
pub async fn run<C: Clock>(
    mut rx: mpsc::Receiver<Envelope>,
    engine: &mut Engine,
    clock: &mut C,
    deadline: Option<Instant>,
    mut after_event: impl FnMut(&Event, Option<&Decision>, &Engine),
) -> LoopStats {
    let mut stats = LoopStats::default();
    loop {
        let next = match deadline {
            Some(d) => match tokio::time::timeout_at(d.into(), rx.recv()).await {
                Ok(next) => next,
                Err(_) => break, // deadline reached
            },
            None => rx.recv().await,
        };
        let Some(env) = next else { break }; // source finished

        clock.observe(&env.event);
        let started = Instant::now();
        let decision = engine.on_event(&env.event, clock);

        if let Some(received) = env.received {
            let ns = received.elapsed().as_nanos() as u64;
            stats.engine_compute_ns.push(started.elapsed().as_nanos() as u64);
            stats.event_latency_ns.push(ns);
            if matches!(env.event, Event::Bar(_)) {
                stats.bar_latency_ns.push(ns);
            }
            match &env.event {
                Event::Trade(t) => stats.feed_latency_ms.push(t.recv_ts - t.ts),
                Event::BookTop(b) => stats.feed_latency_ms.push(b.recv_ts - b.ts),
                _ => {}
            }
        }
        *stats.events_by_kind.entry(env.event.kind()).or_default() += 1;
        if decision.is_some() {
            stats.decisions += 1;
        }
        after_event(&env.event, decision.as_ref(), engine);
    }
    stats
}
