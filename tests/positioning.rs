//! Leaderboard wallet selection, positioning aggregation, the v3 gate and its
//! fail-closed behaviour, and the universe's funding arithmetic.

mod common;

use common::*;
use mft_engine::engine::{Engine, EngineConfig};
use mft_engine::event::{Event, Positioning};
use mft_engine::positioning::*;
use mft_engine::text::Caution;
use mft_engine::universe::funding_zone;

fn row(addr: &str, account_value: &str, month_pnl: &str) -> LeaderRow {
    serde_json::from_value(serde_json::json!({
        "ethAddress": addr,
        "accountValue": account_value,
        "displayName": null,
        "windowPerformances": [
            ["day", {"pnl": "1", "roi": "0", "vlm": "0"}],
            ["month", {"pnl": month_pnl, "roi": "0", "vlm": "0"}],
            ["allTime", {"pnl": "5", "roi": "0", "vlm": "0"}]
        ]
    }))
    .unwrap()
}

#[test]
fn wallets_are_chosen_by_30_day_pnl_above_a_size_floor() {
    let rows = vec![
        row("0xAAA", "500000", "10"),
        row("0xbbb", "50000", "999"), // too small
        row("0xccc", "200000", "50"),
        row("0xddd", "200000", "50"), // tie with ccc: address order decides
        row("0xeee", "900000", "-5"),
    ];
    assert_eq!(rows[0].window_pnl("month"), 10.0);
    assert_eq!(rows[0].window_pnl("week"), 0.0, "a missing window reads as zero");
    let picked = select_wallets(&rows, 3, 100_000.0);
    assert_eq!(picked, vec!["0xccc", "0xddd", "0xaaa"], "lower-cased, sorted, deterministic");
}

fn pos(coin: &str, szi: f64, value: f64, lev: f64) -> WalletPosition {
    WalletPosition { coin: coin.into(), szi, position_value: value, leverage: lev }
}

#[test]
fn aggregation_sums_long_and_short_value_per_coin() {
    let wallets = vec![
        vec![pos("BTC", 1.0, 300.0, 10.0), pos("ETH", -2.0, 50.0, 5.0)],
        vec![pos("BTC", -0.5, 100.0, 20.0)],
        vec![pos("BTC", 2.0, 600.0, 4.0)],
        vec![], // a wallet with no positions still counts as scanned
    ];
    let agg = aggregate(&wallets, 42);
    let btc = &agg["BTC"];
    assert_eq!((btc.wallets_scanned, btc.wallets_holding, btc.long_count, btc.short_count), (4, 3, 2, 1));
    assert_eq!((btc.long_value, btc.short_value), (900.0, 100.0));
    assert!((btc.long_share - 0.9).abs() < 1e-12);
    assert_eq!((btc.avg_long_leverage, btc.avg_short_leverage), (7.0, 20.0));
    assert_eq!(agg["ETH"].long_share, 0.0);
    assert_eq!(btc.ts, 42);
    assert_eq!(crowd_label(0.9), "bullish");
    assert_eq!(crowd_label(0.5), "neutral");
    assert_eq!(crowd_label(0.39), "bearish");
}

fn snapshot(coin: &str, ts: i64, long_share: f64, holders: u32) -> Positioning {
    Positioning {
        coin: coin.into(),
        ts,
        source: "test".into(),
        wallets_scanned: 100,
        wallets_holding: holders,
        long_count: holders,
        short_count: 0,
        long_value: long_share * 1000.0,
        short_value: (1.0 - long_share) * 1000.0,
        long_share,
        long_share_change: None,
        avg_long_leverage: 1.0,
        avg_short_leverage: 1.0,
    }
}

fn gate() -> PositioningState {
    PositioningState::new(PositioningParams { enabled: true, ..PositioningParams::default() })
}

#[test]
fn gate_follows_the_crowd_and_fails_closed() {
    let mut g = gate();
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 0), Caution::new(0.0), "no snapshot yet");
    g.on_snapshot(&snapshot("BTC", 0, 0.8, 10));
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 1_000), Caution::NONE, "crowd 80% long, long entry");
    assert_eq!(g.caution_for("BTC", -1_000.0, 0.0, 1_000), Caution::new(0.0), "short against the crowd");
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 16 * 60_000), Caution::new(0.0), "stale snapshot");
    assert_eq!(g.caution_for("BTC", -1_000.0, -1_000.0, 1_000), Caution::NONE, "held positions are not re-gated");
    g.on_snapshot(&snapshot("BTC", 0, 0.55, 10));
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 1_000), Caution::new(0.0), "55% is not a crowd");
    g.on_snapshot(&snapshot("BTC", 0, 0.95, 3));
    assert_eq!(g.caution_for("BTC", 1_000.0, 0.0, 1_000), Caution::new(0.0), "three holders are not a crowd");
    let off = PositioningState::new(PositioningParams::default());
    assert_eq!(off.caution_for("BTC", 1_000.0, 0.0, 1_000), Caution::NONE, "off outside v3");
}

#[test]
fn v3_enters_only_with_the_crowd() {
    let with = |share: f64| {
        let mut events = bars_from("BTC", &trending_closes(1));
        let trigger = events.last().unwrap().ts();
        events.insert(events.len() - 1, Event::Positioning(snapshot("BTC", trigger - 60_000, share, 10)));
        events
    };
    assert!(fills(&replay(&mut Engine::new(EngineConfig::v3()), &with(0.8))) >= 1);
    assert_eq!(fills(&replay(&mut Engine::new(EngineConfig::v3()), &with(0.3))), 0);
    assert_eq!(fills(&replay(&mut Engine::new(EngineConfig::v3()), &bars_from("BTC", &trending_closes(1)))), 0);
    assert!(fills(&replay(&mut Engine::new(EngineConfig::v1()), &with(0.3))) >= 1, "v1 ignores positioning");
}

#[test]
fn positioning_events_round_trip_and_older_files_still_parse() {
    let ev = Event::Positioning(snapshot("ETH", 7, 0.6, 9));
    let line = serde_json::to_string(&ev).unwrap();
    assert!(line.starts_with(r#"{"type":"Positioning""#));
    assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), ev);
    // Configs written before positioning existed still deserialise.
    let mut old = serde_json::to_value(EngineConfig::v1()).unwrap();
    old.as_object_mut().unwrap().remove("positioning");
    let cfg: EngineConfig = serde_json::from_value(old).unwrap();
    assert!(!cfg.positioning.enabled);
}

#[test]
fn funding_zones_use_hourly_funding() {
    // Hyperliquid's baseline 0.00125% per hour is 10.95% a year, not 1.4%.
    let annual: f64 = 0.0000125 * 24.0 * 365.0 * 100.0;
    assert!((annual - 10.95).abs() < 1e-9);
    assert_eq!(funding_zone(annual), "mild long");
    assert_eq!(funding_zone(0.0), "neutral");
    assert_eq!(funding_zone(-60.0), "hot short");
    assert_eq!(funding_zone(150.0), "extreme long");
}
