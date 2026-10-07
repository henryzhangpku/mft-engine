//! `verify-replay`: replay a live session log through the same engine and
//! diff the decision stream against the one the live run recorded.
//!
//! The live run writes two files: the session log (warm-up, then every
//! event in the order the engine consumed it, each with the clock it was
//! handled at) and the decision log (each decision with the index of the
//! event behind it). Replaying the first by arrival must give back the
//! second exactly: same decisions, at the same event indices, to the same
//! fingerprint. If it does not, the first point where the two streams part
//! is reported: an input the live engine saw that the log is missing, a
//! clock that differed, or a code change between the run and the replay.

use crate::backtest::{load_session, run_replay, time_of};
use crate::clock::{format_utc, TimeKey};
use crate::engine::{Decision, EngineConfig};
use crate::event::Event;
use crate::paper::{decisions_fingerprint, DecisionRecord};
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Where the live and replayed decision streams first differ.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Divergence {
    /// Position in the decision streams.
    pub decision_index: usize,
    /// Index, in the replayed session, of the event behind the earlier of
    /// the two decisions: the first point where one side decided and the
    /// other did not, or decided differently. (The live decision's own
    /// `event_index` counts the events the live engine saw, which differs if
    /// the log is missing one; every decision comes from a bar, so the live
    /// one is located as the bar of its coin at its clock.)
    pub event_index: Option<usize>,
    /// That event, from the replayed session, and its clock.
    pub event: Option<Event>,
    pub event_time: Option<i64>,
    pub live: Option<DecisionRecord>,
    pub replay: Option<DecisionRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub time_key: TimeKey,
    pub events: usize,
    pub warmup_events: usize,
    pub live_decisions: usize,
    pub replay_decisions: usize,
    pub live_fingerprint: String,
    pub replay_fingerprint: String,
    pub first_divergence: Option<Divergence>,
}

impl VerifyReport {
    pub fn identical(&self) -> bool {
        self.first_divergence.is_none()
    }
}

pub fn read_decisions(path: &Path) -> Result<Vec<DecisionRecord>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("{}:{}: bad decision line", path.display(), i + 1)))
        .collect()
}

/// Two decisions are the same if they serialise the same: the bytes the
/// fingerprint is taken over.
fn same(a: &DecisionRecord, b: &DecisionRecord) -> bool {
    a.event_index == b.event_index && serde_json::to_string(&a.decision).ok() == serde_json::to_string(&b.decision).ok()
}

/// The first position where the streams differ, if any.
pub fn first_difference(live: &[DecisionRecord], replay: &[DecisionRecord]) -> Option<usize> {
    let n = live.len().max(replay.len());
    (0..n).find(|&k| match (live.get(k), replay.get(k)) {
        (Some(a), Some(b)) => !same(a, b),
        _ => true,
    })
}

/// Replay `session` with `config` under `key` and compare with `live`.
pub async fn verify(session: &[PathBuf], live: Vec<DecisionRecord>, config: EngineConfig, key: TimeKey) -> Result<VerifyReport> {
    let (envelopes, key) = load_session(session, Some(key))?;
    let warmup_events = envelopes.iter().filter(|e| matches!(e.event, Event::Warmup(_))).count();
    let events = envelopes.len();
    let run = run_replay("verify", envelopes.clone(), config, key).await;
    let replay: Vec<DecisionRecord> = run
        .decision_events
        .iter()
        .zip(&run.decisions)
        .map(|(&event_index, d)| DecisionRecord { event_index, decision: d.clone() })
        .collect();
    let live_only: Vec<Decision> = live.iter().map(|d| d.decision.clone()).collect();
    // The engine clock at each replayed event (never moves backwards).
    let mut clocks = Vec::with_capacity(envelopes.len());
    let mut now = i64::MIN;
    for e in &envelopes {
        now = now.max(time_of(e, key));
        clocks.push(now);
    }
    let locate = |d: &Decision| {
        (0..envelopes.len()).find(|&i| clocks[i] == d.ts() && matches!(&envelopes[i].event, Event::Bar(b) if b.coin == d.coin()))
    };
    let first_divergence = first_difference(&live, &replay).map(|k| {
        let (l, r) = (live.get(k).cloned(), replay.get(k).cloned());
        let event_index = match (&l, &r) {
            (Some(a), Some(b)) if a.decision.ts() < b.decision.ts() => locate(&a.decision),
            (Some(a), Some(b)) if a.decision.ts() == b.decision.ts() => locate(&a.decision).map(|i| i.min(b.event_index)),
            (_, Some(b)) => Some(b.event_index),
            (Some(a), None) => locate(&a.decision),
            (None, None) => unreachable!("a difference has at least one side"),
        };
        let env = event_index.and_then(|i| envelopes.get(i));
        Divergence {
            decision_index: k,
            event_index,
            event: env.map(|e| e.event.clone()),
            event_time: env.map(|e| time_of(e, key)),
            live: l,
            replay: r,
        }
    });
    Ok(VerifyReport {
        time_key: key,
        events,
        warmup_events,
        live_decisions: live.len(),
        replay_decisions: replay.len(),
        live_fingerprint: decisions_fingerprint(&live_only),
        replay_fingerprint: run.report.decisions_fingerprint,
        first_divergence,
    })
}

pub fn print(r: &VerifyReport) {
    println!(
        "replayed {} events ({} warm-up) by {} time: live {} decisions, replay {}",
        r.events, r.warmup_events, r.time_key.name(), r.live_decisions, r.replay_decisions
    );
    match &r.first_divergence {
        None => println!("identical: live fingerprint {}, replay fingerprint {}", r.live_fingerprint, r.replay_fingerprint),
        Some(d) => {
            let at = d.event_index.map_or("not located".to_string(), |i| format!("#{i}"));
            println!("DIVERGED at decision #{} (session event {at}{})", d.decision_index, d.event_time.map_or(String::new(), |t| format!(", {}", format_utc(t))));
            let show = |x: &Option<DecisionRecord>| x.as_ref().map_or("none".to_string(), |d| serde_json::to_string(d).unwrap_or_default());
            if let Some(ev) = &d.event {
                println!("  event:  {}", serde_json::to_string(ev).unwrap_or_default());
            }
            println!("  live:   {}", show(&d.live));
            println!("  replay: {}", show(&d.replay));
            println!("live fingerprint {}, replay fingerprint {}", r.live_fingerprint, r.replay_fingerprint);
        }
    }
}
