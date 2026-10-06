//! Deterministic replay, gap handling, and the parsing that feeds them.

mod common;

use common::*;
use perp_engine::backtest::{load_events, run_backtest};
use perp_engine::bars::{detect_bar_gaps, with_built_bars, BarBuilder};
use perp_engine::engine::{Engine, EngineConfig};
use perp_engine::event::{Event, Gap, Side, Trade};
use perp_engine::gap::GapDetector;
use perp_engine::hyperliquid::parse_ws_message;
use std::path::PathBuf;

fn committed_data() -> Vec<Event> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    load_events(&[root.join("data/bars_1m.jsonl"), root.join("data/text_signals.jsonl")]).unwrap()
}

#[tokio::test]
async fn same_input_same_output() {
    let events = committed_data();
    let (a, da) = run_backtest("a", events.clone(), EngineConfig::default()).await;
    let (b, db) = run_backtest("a", events, EngineConfig::default()).await;
    assert!(a.fills > 0, "the sample should produce some trades");
    assert_eq!(da, db, "decisions differ between two identical replays");
    assert_eq!(a, b, "reports differ between two identical replays");
}

#[test]
fn synchronous_and_async_paths_agree() {
    // The event loop (async, channel-fed) and a plain for-loop over
    // engine.on_event must give identical decisions: the loop adds nothing.
    let events = committed_data();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (_, via_loop) = rt.block_on(run_backtest("x", events.clone(), EngineConfig::default()));
    let mut eng = Engine::new(EngineConfig::default());
    let direct = replay(&mut eng, &events);
    assert_eq!(via_loop, direct);
}

#[test]
fn missing_minute_resets_the_signal() {
    // Trend bars come after a one-minute hole: the strategy must not compute
    // a 5-minute return across it, so there is no trade.
    let closes = trending_closes(3);
    let mut events = Vec::new();
    for (i, c) in closes.iter().enumerate() {
        let minute = if i >= 61 { i as i64 + 1 } else { i as i64 }; // skip a minute
        events.push(Event::Bar(bar("BTC", minute, *c)));
    }
    let mut eng = Engine::new(EngineConfig::default());
    assert_eq!(fills(&replay(&mut eng, &events)), 0);
    assert_eq!(detect_bar_gaps(&events).len(), 1);
}

#[test]
fn gap_event_resets_the_signal() {
    let mut events = bars_from("BTC", &trending_closes(3));
    let trigger_ts = events[61].ts();
    events.insert(
        61,
        Event::Gap(Gap {
            coin: "BTC".into(),
            stream: "connection".into(),
            ts: trigger_ts - 1,
            from_ts: trigger_ts - 120_000,
            to_ts: trigger_ts - 1,
            reason: "test".into(),
        }),
    );
    let mut eng = Engine::new(EngineConfig::default());
    assert_eq!(fills(&replay(&mut eng, &events)), 0);
}

#[test]
fn gap_on_another_coin_does_not_reset_this_one() {
    let mut events = bars_from("BTC", &trending_closes(3));
    let ts = events[61].ts() - 1;
    events.insert(
        61,
        Event::Gap(Gap { coin: "ETH".into(), stream: "book".into(), ts, from_ts: ts, to_ts: ts, reason: "test".into() }),
    );
    let mut eng = Engine::new(EngineConfig::default());
    assert!(fills(&replay(&mut eng, &events)) >= 1);
}

#[test]
fn quiet_book_does_not_reset_the_signal() {
    // Bars come from trades; a silent book stream only affects fill pricing.
    let mut events = bars_from("BTC", &trending_closes(3));
    let ts = events[61].ts() - 1;
    events.insert(
        61,
        Event::Gap(Gap { coin: "BTC".into(), stream: "book".into(), ts, from_ts: ts - 30_000, to_ts: ts, reason: "test".into() }),
    );
    let mut eng = Engine::new(EngineConfig::default());
    assert!(fills(&replay(&mut eng, &events)) >= 1);
}

#[test]
fn gap_detector_flags_silence_and_backwards_time() {
    let mut g = GapDetector::new();
    assert!(g.observe("book", "BTC", 1_000, 1_000).is_none());
    assert!(g.observe("book", "BTC", 2_000, 2_000).is_none());
    assert!(g.observe("book", "BTC", 7_500, 7_500).is_none(), "normal ~5.4 s cadence");
    let silence = g.observe("book", "BTC", 30_000, 30_000).expect("22.5 s silence > 20 s");
    assert_eq!((silence.from_ts, silence.to_ts), (7_500, 30_000));
    let back = g.observe("book", "BTC", 29_000, 30_100).expect("time went backwards");
    assert!(back.reason.contains("backwards"));
    // Streams and coins are tracked separately.
    assert!(g.observe("book", "ETH", 50_000, 50_000).is_none());
    assert!(g.observe("trades", "BTC", 50_000, 50_000).is_none());
}

fn trade(ts: i64, px: f64) -> Trade {
    Trade { coin: "BTC".into(), ts, recv_ts: ts, px, sz: 1.0, aggressor: Side::Buy }
}

#[test]
fn bar_builder_closes_on_next_minute() {
    let mut b = BarBuilder::new();
    assert!(b.push(&trade(T0 + 1_000, 100.0)).is_none());
    assert!(b.push(&trade(T0 + 2_000, 105.0)).is_none());
    assert!(b.push(&trade(T0 + 3_000, 99.0)).is_none());
    assert!(b.push(&trade(T0 - 5_000, 1.0)).is_none(), "late print ignored");
    let bar = b.push(&trade(T0 + 61_000, 101.0)).expect("new minute closes the bar");
    assert_eq!((bar.open, bar.high, bar.low, bar.close), (100.0, 105.0, 99.0, 99.0));
    assert_eq!(bar.open_ts, T0);
    assert_eq!(bar.ts, T0 + 61_000, "stamped when it became known");
    assert_eq!(bar.trades, 3);

    let replayed = with_built_bars(vec![
        Event::Trade(trade(T0 + 1_000, 100.0)),
        Event::Trade(trade(T0 + 61_000, 101.0)),
    ]);
    assert_eq!(replayed.len(), 3);
    assert!(matches!(replayed[2], Event::Bar(_)));
}

#[test]
fn parses_hyperliquid_frames() {
    let trades = r#"{"channel":"trades","data":[{"coin":"BTC","side":"A","px":"60000.5","sz":"0.01","time":1700000000000,"hash":"0x","tid":1}]}"#;
    let evs = parse_ws_message(trades, 1700000000123).unwrap();
    match &evs[0] {
        Event::Trade(t) => {
            assert_eq!(t.aggressor, Side::Sell);
            assert_eq!(t.px, 60000.5);
            assert_eq!(t.recv_ts - t.ts, 123);
        }
        other => panic!("{other:?}"),
    }
    let book = r#"{"channel":"l2Book","data":{"coin":"ETH","time":1700000000000,"levels":[[{"px":"2000.1","sz":"3","n":2}],[{"px":"2000.2","sz":"4","n":1}]]}}"#;
    match &parse_ws_message(book, 0).unwrap()[0] {
        Event::BookTop(b) => assert_eq!((b.bid_px, b.ask_px), (2000.1, 2000.2)),
        other => panic!("{other:?}"),
    }
    assert!(parse_ws_message(r#"{"channel":"pong"}"#, 0).unwrap().is_empty());
    assert!(parse_ws_message(r#"{"channel":"trades","data":[{"coin":"BTC","side":"A","px":"abc","sz":"1","time":1}]}"#, 0).is_err());
}

#[test]
fn events_round_trip_through_json() {
    let ev = Event::Trade(trade(T0, 100.0));
    let line = serde_json::to_string(&ev).unwrap();
    assert!(line.starts_with(r#"{"type":"Trade""#));
    assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), ev);
}

#[test]
fn utc_formatting() {
    assert_eq!(perp_engine::clock::format_utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(perp_engine::clock::format_utc(951_782_400_000), "2000-02-29T00:00:00Z");
    assert_eq!(perp_engine::clock::format_utc(1_790_952_360_000), "2026-10-02T14:46:00Z");
}
