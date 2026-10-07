//! Replay verification: a session run through the live code path (warm-up,
//! bar builder, event loop, arrival clock, session and decision logs) and
//! then replayed from its log gives the same decisions; a log missing one
//! input the live engine saw is caught at the right place. Plus arrival
//! keying, and the ledger entries that must keep replaying by exchange time.

mod common;

use common::*;
use mft_engine::artifacts::write_jsonl;
use mft_engine::backtest::load_session;
use mft_engine::bars::{read_events, read_recorded};
use mft_engine::clock::TimeKey;
use mft_engine::engine::{Decision, Engine, EngineConfig};
use mft_engine::event::{Event, Recorded, Side, TextSignal, Trade, WarmupBar};
use mft_engine::event_loop::Envelope;
use mft_engine::paper::{build_bars, run_session, SessionLog};
use mft_engine::verify;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::sync::mpsc;

const MIN: i64 = 60_000;
/// Receive latency of every synthetic trade, ms.
const FEED_LAG: i64 = 120;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("mft-engine-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn config() -> EngineConfig {
    let mut c = EngineConfig::v1();
    c.text.enabled = true;
    c
}

/// A scored post from the recorded fixture log (mock scorer output, never a
/// model call), moved to `at` on the session's clock: the scores, ids and
/// the publish-to-score delay are the recorded ones.
fn recorded_signal(post_id: &str, at: i64) -> TextSignal {
    let line = std::fs::read_to_string(root().join("tests/fixtures/synthetic_text_signals.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Event>(l).unwrap())
        .find_map(|e| match e {
            Event::TextSignal(s) if s.post_id == post_id && s.coin == "BTC" => Some(s),
            _ => None,
        })
        .expect("fixture post");
    let delay = line.ts - line.published_ts;
    TextSignal { ts: at, published_ts: at - delay, ..line }
}

/// The closes, by minute: 61 minutes of noise (the warm-up), four minutes up
/// 0.5% each (a long), noise, four minutes down (a short, unless vetoed),
/// noise.
fn closes() -> Vec<f64> {
    let mut c = trending_closes(0);
    let mut px = *c.last().unwrap();
    let mut push = |c: &mut Vec<f64>, n: usize, f: &dyn Fn(usize) -> f64| {
        for i in 0..n {
            px *= f(i);
            c.push(px);
        }
    };
    push(&mut c, 4, &|_| 1.005);
    push(&mut c, 7, &|i| if i % 2 == 0 { 1.0005 } else { 0.9995 });
    push(&mut c, 4, &|_| 0.995);
    push(&mut c, 7, &|i| if i % 2 == 0 { 1.0005 } else { 0.9995 });
    c
}

/// Warm-up bars for minutes 0..61 (fetched once, just after minute 61
/// opens), and the live inputs from minute 61 on: three trades a minute
/// (exchange time, received `FEED_LAG` later) and the given text signals,
/// arriving 300 ms after they were scored. Sorted by arrival, so a single
/// producer delivers them in order.
fn synthetic(signals: &[TextSignal]) -> (Vec<WarmupBar>, Vec<Envelope>) {
    let closes = closes();
    let fetched = T0 + 61 * MIN + 400;
    let warmup = (0..61).map(|i| WarmupBar { recv_ts: fetched, bar: bar("BTC", i as i64, closes[i]) }).collect();
    let mut live: Vec<(i64, Event)> = Vec::new();
    // One extra minute of trades closes the last bar.
    for (m, px) in closes.iter().enumerate().skip(61).chain([(closes.len(), closes.last().unwrap())]) {
        for s in [5_000, 25_000, 45_000] {
            let ts = T0 + m as i64 * MIN + s;
            let t = Trade { coin: "BTC".into(), ts, recv_ts: ts + FEED_LAG, px: *px, sz: 0.01, aggressor: Side::Buy };
            live.push((t.recv_ts, Event::Trade(t)));
        }
    }
    for s in signals {
        live.push((s.ts + 300, Event::TextSignal(s.clone())));
    }
    live.sort_by_key(|(at, _)| *at);
    let envelopes = live.into_iter().map(|(at, e)| Envelope::live(e, Instant::now(), at)).collect();
    (warmup, envelopes)
}

/// Run the live path: feed -> bar builder -> engine channel -> run_session.
async fn run_live(warmup: Vec<WarmupBar>, inputs: Vec<Envelope>) -> SessionLog {
    let (raw_tx, raw_rx) = mpsc::channel::<Envelope>(1_024);
    let (tx, rx) = mpsc::channel::<Envelope>(1_024);
    tokio::spawn(build_bars(raw_rx, tx));
    tokio::spawn(async move {
        for env in inputs {
            if raw_tx.send(env).await.is_err() {
                break;
            }
        }
    });
    let mut engine = Engine::new(config());
    run_session(rx, &mut engine, warmup, None, |_, _, _| {}).await
}

/// The bullish post (recorded) lands before the down move and vetoes the
/// short; the bearish one is the input a log could miss.
fn bullish_at_minute_70() -> TextSignal {
    recorded_signal("syn-042", T0 + 70 * MIN + 10_000)
}
fn bearish_at_minute_63() -> TextSignal {
    recorded_signal("syn-037", T0 + 63 * MIN + 30_000)
}

fn write_logs(tag: &str, log: &SessionLog, events: &[Recorded]) -> (PathBuf, PathBuf) {
    let (ev, dec) = (scratch(&format!("{tag}-events.jsonl")), scratch(&format!("{tag}-decisions.jsonl")));
    write_jsonl(&ev, events).unwrap();
    write_jsonl(&dec, &log.decisions).unwrap();
    (ev, dec)
}

fn cleanup(paths: &[&Path]) {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}

#[tokio::test]
async fn live_session_replays_to_identical_decisions() {
    let (warmup, inputs) = synthetic(&[bullish_at_minute_70()]);
    let log = run_live(warmup, inputs).await;

    // The log starts with the warm-up, in order, and its clock never goes back.
    assert!(log.events[..61].iter().all(|r| matches!(r.event, Event::Warmup(_))));
    assert!(!log.events[61..].iter().any(|r| matches!(r.event, Event::Warmup(_))));
    assert!(log.events.windows(2).all(|w| w[0].arrival_ts <= w[1].arrival_ts));
    assert!(log.events.iter().any(|r| matches!(r.event, Event::TextSignal(_))));
    let fills = log.decisions.iter().filter(|d| matches!(d.decision, Decision::Filled { .. })).count();
    assert!(fills >= 2, "the session should trade: {:?}", log.decisions);
    // The warm-up is what lets it trade on the first live bar.
    let first = &log.decisions[0];
    assert!(matches!(&log.events[first.event_index].event, Event::Bar(b) if b.open_ts == T0 + 61 * MIN), "{first:?}");

    let (ev, dec) = write_logs("identical", &log, &log.events);
    let live = verify::read_decisions(&dec).unwrap();
    assert_eq!(live, log.decisions, "the decision log round-trips exactly");
    let report = verify::verify(std::slice::from_ref(&ev), live.clone(), config(), TimeKey::Arrival).await.unwrap();
    verify::print(&report);
    assert!(report.identical(), "{:?}", report.first_divergence);
    assert_eq!(report.live_fingerprint, report.replay_fingerprint);
    assert_eq!(report.warmup_events, 61);
    assert_eq!(report.live_decisions, log.decisions.len());

    // Without the warm-up the replay starts cold and cannot match.
    let cold: Vec<Recorded> = log.events.iter().filter(|r| !matches!(r.event, Event::Warmup(_))).cloned().collect();
    let cold_path = scratch("cold-events.jsonl");
    write_jsonl(&cold_path, &cold).unwrap();
    let cold_report = verify::verify(std::slice::from_ref(&cold_path), live.clone(), config(), TimeKey::Arrival).await.unwrap();
    assert_eq!(cold_report.first_divergence.as_ref().map(|d| d.decision_index), Some(0));

    // By exchange time the clock is the trades' exchange stamps, FEED_LAG
    // earlier than what the live engine had: the decisions carry different
    // times, so the stream differs from the first decision on.
    let by_exchange = verify::verify(std::slice::from_ref(&ev), live, config(), TimeKey::Exchange).await.unwrap();
    let d = by_exchange.first_divergence.expect("exchange keying is not what the live engine ran on");
    assert_eq!(d.decision_index, 0);
    assert_eq!(d.live.unwrap().decision.ts() - d.replay.unwrap().decision.ts(), FEED_LAG);
    cleanup(&[&ev, &dec, &cold_path]);
}

#[tokio::test]
async fn an_unrecorded_input_is_reported_where_it_changed_a_decision() {
    // Live, the engine sees a bearish post during the long; the log loses it.
    let injected = bearish_at_minute_63();
    let (warmup, inputs) = synthetic(&[injected.clone(), bullish_at_minute_70()]);
    let log = run_live(warmup, inputs).await;
    let j = log
        .events
        .iter()
        .position(|r| matches!(&r.event, Event::TextSignal(s) if s.post_id == injected.post_id))
        .expect("the live engine consumed it");
    let mut recorded = log.events.clone();
    recorded.remove(j);

    // Live: the long from minute 61 is closed by the post at the next bar
    // after it. That is decision #1, at live event index L.
    let exit = &log.decisions[1];
    match &exit.decision {
        Decision::Filled { raw_target, target, why, .. } => {
            assert!(*raw_target > 0.0 && *target == 0.0, "the post vetoed a held long: {why}");
            assert!(why.contains("social x0.00"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    let l = exit.event_index;
    assert!(j < l);

    let (ev, dec) = write_logs("injected", &log, &recorded);
    let live = verify::read_decisions(&dec).unwrap();
    let report = verify::verify(std::slice::from_ref(&ev), live, config(), TimeKey::Arrival).await.unwrap();
    verify::print(&report);
    let d = report.first_divergence.expect("a missing input must be caught");
    assert_eq!(d.decision_index, 1, "decision #0 (the entry) came before the post and still matches");
    // In the replayed stream, which lacks line j, that bar is at L - 1.
    assert_eq!(d.event_index, Some(l - 1));
    let bar_open = match &log.events[l].event {
        Event::Bar(b) => b.open_ts,
        other => panic!("{other:?}"),
    };
    assert!(matches!(&d.event, Some(Event::Bar(b)) if b.open_ts == bar_open));
    assert_eq!(d.live.as_ref().unwrap(), exit);
    assert!(d.replay.as_ref().map_or(true, |r| r.event_index > l - 1), "the replay held the long past that bar");
    assert_ne!(report.live_fingerprint, report.replay_fingerprint);
    cleanup(&[&ev, &dec]);
}

#[test]
fn session_log_lines_stay_readable_as_plain_events() {
    let w = WarmupBar { recv_ts: T0 + 5, bar: bar("BTC", 0, 100.0) };
    let rec = Recorded { event: Event::Warmup(w.clone()), arrival_ts: Some(T0 + 7) };
    let line = serde_json::to_string(&rec).unwrap();
    assert!(line.starts_with(r#"{"type":"Warmup","#), "{line}");
    assert!(line.contains(r#""arrival_ts":"#));
    assert_eq!(serde_json::from_str::<Recorded>(&line).unwrap(), rec);
    // An older reader that parses `Event` ignores the arrival field.
    assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), Event::Warmup(w));
    // A plain event line is a session-log line with no recorded arrival.
    let plain = serde_json::to_string(&Event::Bar(bar("BTC", 1, 101.0))).unwrap();
    let r: Recorded = serde_json::from_str(&plain).unwrap();
    assert_eq!(r.arrival_ts, None);
    assert_eq!(r.arrival(), bar("BTC", 1, 101.0).ts, "falls back to the event's own time");
}

#[test]
fn arrival_keying_orders_by_receipt_not_exchange_time() {
    // As on subscribe: the feed delivers recent history first, so exchange
    // times run backwards relative to receipt.
    let mk = |ts: i64, recv: i64, px: f64| Event::Trade(Trade { coin: "BTC".into(), ts, recv_ts: recv, px, sz: 1.0, aggressor: Side::Sell });
    let lines = [mk(T0 + 30_000, T0 + 40_000, 1.0), mk(T0 + 10_000, T0 + 40_000, 2.0), mk(T0 + 20_000, T0 + 41_000, 3.0)];
    let path = scratch("arrival-order.jsonl");
    write_jsonl(&path, &lines).unwrap();
    let px = |envs: &[Envelope]| -> Vec<f64> {
        envs.iter().filter_map(|e| match &e.event { Event::Trade(t) => Some(t.px), _ => None }).collect()
    };
    let (by_arrival, key) = load_session(std::slice::from_ref(&path), Some(TimeKey::Arrival)).unwrap();
    assert_eq!(key, TimeKey::Arrival);
    assert_eq!(px(&by_arrival), vec![1.0, 2.0, 3.0], "receipt order, ties kept in file order");
    assert_eq!(by_arrival[0].arrival_ts, Some(T0 + 40_000));
    let (by_exchange, _) = load_session(std::slice::from_ref(&path), Some(TimeKey::Exchange)).unwrap();
    assert_eq!(px(&by_exchange), vec![2.0, 3.0, 1.0]);
    // No recorded arrival stamps: auto picks exchange, the ledger's key.
    assert_eq!(load_session(std::slice::from_ref(&path), None).unwrap().1, TimeKey::Exchange);
    // The same events written as a session log: auto picks arrival.
    let recorded: Vec<Recorded> = lines.iter().map(|e| Recorded { event: e.clone(), arrival_ts: Some(e.own_arrival_ts()) }).collect();
    write_jsonl(&path, &recorded).unwrap();
    assert_eq!(load_session(std::slice::from_ref(&path), None).unwrap().1, TimeKey::Arrival);
    assert_eq!(read_recorded(&path).unwrap(), recorded);
    assert_eq!(read_events(&path).unwrap(), lines.to_vec());
    cleanup(&[&path]);
}

#[tokio::test]
async fn every_ledger_trial_replays_on_its_pinned_key() {
    // Entries 0 to 13 carry no time key and were replayed by exchange time;
    // `ledger dsr` replays each from what its entry recorded and refuses
    // unless it reproduces the logged result (and v4 its fingerprint).
    let entries = mft_engine::ledger::read(&root().join("results/ledger.jsonl")).unwrap();
    for e in entries.iter().filter(|e| e.base != "v4") {
        assert_eq!(mft_engine::trials::time_key_of(e), TimeKey::Exchange, "ledger #{}", e.seq);
    }
    let trials = mft_engine::trials::trials(&root(), &entries).await.unwrap();
    // 18 entries: one pre-registration (14) and one exact rerun (7 of 0).
    assert_eq!(trials.len(), 16);
    assert_eq!(trials[0].entries, vec![0, 7]);
    assert!(!trials.iter().any(|t| t.entries.contains(&14)));
    for t in &trials {
        let e = &entries[t.entries[0] as usize];
        if e.base == "v4" || e.seq >= 7 {
            assert_eq!(mft_engine::trials::logged_fingerprint(e), t.fingerprint, "ledger #{}", e.seq);
        }
    }
    let d = mft_engine::trials::entry_dsr(&entries, &trials, 17).unwrap();
    let logged = entries[17].result["report"]["sharpe_annualised"].as_f64().unwrap();
    assert!((d.sr_annualised - logged).abs() < 1e-9, "same Sharpe as the ledger: {} vs {logged}", d.sr_annualised);
    assert_eq!(d.n_trials, 16);
    assert!(d.deflated.dsr <= d.deflated.psr_vs_zero);
    assert!(mft_engine::trials::entry_dsr(&entries, &trials, 14).is_err(), "a pre-registration has no returns");
}
