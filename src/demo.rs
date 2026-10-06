//! `export-demo`: one JSON file for the static web demo in `docs/`.
//!
//! The demo replays recorded data; it computes nothing itself. This module
//! reruns the v1 and v2 backtests through the same engine and gathers what the
//! page draws: prices, decisions with reasons, risk blocks, the Kalshi implied
//! band, scored posts, equity curves, the experiment ledger and the measured
//! latencies.

use crate::artifacts::write_json;
use crate::backtest::{evaluate, load_events};
use crate::clock::{format_utc, wall_now_ms};
use crate::engine::{Decision, EngineConfig};
use crate::event::Event;
use crate::ledger;
use crate::prediction::{prob_above, quantile};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct DemoInputs {
    pub data: Vec<PathBuf>,
    pub posts: PathBuf,
    pub ledger: PathBuf,
    pub paper: PathBuf,
    pub jev_stats: PathBuf,
    /// A live-recorded session (feed + Kalshi + positioning + social), if any.
    pub session: PathBuf,
    /// REST bars just before the session, so momentum starts warm (the same
    /// inputs as experiments/recorded_session.toml).
    pub session_warmup: PathBuf,
    pub universe: PathBuf,
    pub out: PathBuf,
}

fn round(x: f64, digits: i32) -> f64 {
    let m = 10f64.powi(digits);
    (x * m).round() / m
}

fn decision_json(d: &Decision) -> Value {
    match d {
        Decision::Filled { fill, raw_target, target, why } => json!({
            "t": fill.ts / 1000, "coin": fill.coin, "kind": "fill",
            "side": if fill.qty > 0.0 { "buy" } else { "sell" },
            "qty": fill.qty, "px": round(fill.px, 2), "fee": round(fill.fee, 4),
            "raw_target": raw_target, "target": target, "why": why,
        }),
        Decision::Blocked { coin, ts, qty, raw_target, target, why, reason } => json!({
            "t": ts / 1000, "coin": coin, "kind": "blocked", "qty": qty,
            "raw_target": raw_target, "target": target, "why": why, "reason": reason,
        }),
    }
}

/// Read a small JSON file, or `null` if it is missing (the page shows "not run").
fn read_json_or_null(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

pub async fn run(inputs: DemoInputs) -> Result<()> {
    let events = load_events(&inputs.data)?;

    // Prices (close per bar) and the implied band, per coin.
    let mut bars: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut band: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut last_close: BTreeMap<String, f64> = BTreeMap::new();
    let mut signals: BTreeMap<String, Vec<&crate::event::TextSignal>> = BTreeMap::new();
    for e in &events {
        match e {
            Event::Bar(b) => {
                bars.entry(b.coin.clone()).or_default().push(json!([b.ts / 1000, b.close]));
                last_close.insert(b.coin.clone(), b.close);
            }
            Event::PredictionMarket(p) => {
                let q = |x| quantile(&p.strikes, &p.prob_above, x).map(|v| round(v, 1));
                let p_up = last_close.get(&p.coin).and_then(|s| prob_above(&p.strikes, &p.prob_above, *s));
                band.entry(p.coin.clone()).or_default().push(json!([
                    p.ts / 1000, q(0.25), q(0.5), q(0.75), p_up.map(|v| round(v, 3)), p.close_ts / 1000
                ]));
            }
            Event::TextSignal(s) => signals.entry(s.post_id.clone()).or_default().push(s),
            _ => {}
        }
    }

    // Scored posts joined with their text.
    let mut posts = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&inputs.posts) {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let p: Value = serde_json::from_str(line)?;
            let id = p["id"].as_str().unwrap_or_default().to_string();
            let Some(sigs) = signals.get(&id) else { continue };
            let mut scores = serde_json::Map::new();
            for s in sigs {
                scores.insert(
                    s.coin.clone(),
                    json!({"relevance": round(s.relevance, 3), "bullish": round(s.bullish, 3), "bearish": round(s.bearish, 3), "novelty": round(s.novelty, 3)}),
                );
            }
            posts.push(json!({
                "id": id, "t": sigs[0].ts / 1000, "published": p["published_ts"].as_i64().unwrap_or(0) / 1000,
                "source": p["source"], "kind": p["kind"], "url": p["url"], "text": p["text"],
                "scorer": sigs[0].scorer, "scores": scores,
            }));
        }
    }

    // The two strategies, through the same engine as everything else.
    let mut evaluations = Vec::new();
    let mut decisions = serde_json::Map::new();
    let mut equity = serde_json::Map::new();
    for (name, config) in [("v1", EngineConfig::v1()), ("v2", EngineConfig::v2())] {
        let (eval, run) = evaluate(name, &events, config).await;
        decisions.insert(name.into(), run.decisions.iter().map(decision_json).collect());
        // Every 5th point is plenty for a chart and keeps the file small.
        let curve: Vec<Value> = run.equity.iter().step_by(5).map(|(t, e)| json!([t / 1000, round(*e, 2)])).collect();
        equity.insert(name.into(), curve.into());
        evaluations.push(eval);
    }

    // The live-recorded session: positioning over time, and the three
    // strategies replayed on it (the only data where v3 can act).
    let mut positioning: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut session_evals = Vec::new();
    let mut session_window = Value::Null;
    if inputs.session.exists() {
        let mut files = vec![inputs.session.clone()];
        if inputs.session_warmup.exists() {
            files.insert(0, inputs.session_warmup.clone());
        }
        let session = load_events(&files)?;
        for e in &session {
            if let Event::Positioning(p) = e {
                positioning.entry(p.coin.clone()).or_default().push(json!([
                    p.ts / 1000, round(p.long_share, 4), p.wallets_holding, round(p.long_value, 0), round(p.short_value, 0)
                ]));
            }
        }
        // The recorded span only, not the warm-up bars before it.
        let recorded = crate::bars::read_events(&inputs.session)?;
        session_window = json!({
            "start": format_utc(recorded.iter().map(Event::ts).min().unwrap_or(0)),
            "end": format_utc(recorded.iter().map(Event::ts).max().unwrap_or(0)),
            "warmup_bars": inputs.session_warmup.exists(),
        });
        for (name, config) in [("v1", EngineConfig::v1()), ("v2", EngineConfig::v2()), ("v3", EngineConfig::v3())] {
            session_evals.push(evaluate(name, &session, config).await.0.full);
        }
    }

    // Universe summary: contracts per dex and the most traded perps.
    let mut universe = Value::Null;
    if let Ok(text) = std::fs::read_to_string(&inputs.universe) {
        let doc: Value = serde_json::from_str(&text)?;
        let contracts: Vec<crate::universe::Contract> = serde_json::from_value(doc["contracts"].clone())?;
        let live: Vec<&crate::universe::Contract> = contracts.iter().filter(|c| !c.delisted).collect();
        let mut per_dex: BTreeMap<String, usize> = BTreeMap::new();
        for c in &live {
            *per_dex.entry(if c.dex.is_empty() { "main".into() } else { c.dex.clone() }).or_default() += 1;
        }
        let mut top = live.clone();
        top.sort_by(|a, b| b.day_volume_usd.unwrap_or(0.0).total_cmp(&a.day_volume_usd.unwrap_or(0.0)));
        let rows: Vec<Value> = top
            .iter()
            .take(20)
            .map(|c| {
                json!({
                    "coin": c.coin, "dex": if c.dex.is_empty() { "main" } else { &c.dex },
                    "mark": c.mark_px, "volume_musd": c.day_volume_usd.map(|v| round(v / 1e6, 1)),
                    "oi_musd": c.open_interest_usd.map(|v| round(v / 1e6, 1)),
                    "funding_pct_year": c.funding_annualized_pct.map(|v| round(v, 1)),
                    "zone": c.funding_annualized_pct.map(crate::universe::funding_zone),
                    "max_leverage": c.max_leverage,
                })
            })
            .collect();
        universe = json!({
            "fetched_at": doc["fetched_at"], "dexes_listed": doc["dexes"].as_array().map_or(0, |d| d.len()),
            "live_contracts": live.len(), "per_dex": per_dex, "top_by_volume": rows,
        });
    }

    let ledger_rows: Vec<Value> = ledger::read(&inputs.ledger)?
        .iter()
        .map(|e| {
            json!({
                "seq": e.seq, "variant": e.variant, "hypothesis": e.hypothesis, "verdict": e.verdict,
                "hash": &e.hash[..12], "prev": &e.prev_hash[..12], "recorded_at": e.recorded_at,
                "full_pnl": e.result["full"]["pnl_after_costs"], "holdout_pnl": e.result["second_half"]["pnl_after_costs"],
                "fills": e.result["full"]["fills"],
            })
        })
        .collect();

    let doc = json!({
        "generated_at": format_utc(wall_now_ms()),
        "notice": "Replays recorded data through the engine. Paper only: no real orders, no order code.",
        "window": { "start": format_utc(events.first().map_or(0, Event::ts)), "end": format_utc(events.last().map_or(0, Event::ts)) },
        "config": { "v1": EngineConfig::v1(), "v2": EngineConfig::v2() },
        "evaluations": evaluations,
        "bars": bars,
        "pm_band": band,
        "posts": posts,
        "decisions": decisions,
        "equity": equity,
        "ledger": ledger_rows,
        "paper": read_json_or_null(&inputs.paper),
        "jev": read_json_or_null(&inputs.jev_stats),
        "session": { "window": session_window, "positioning": positioning, "evaluations": session_evals },
        "universe": universe,
    });
    write_json(&inputs.out, &doc)?;
    let size = std::fs::metadata(&inputs.out).map(|m| m.len()).unwrap_or(0);
    println!("wrote {} ({} KB, {} posts, {} ledger entries)", inputs.out.display(), size / 1024, posts.len(), ledger_rows.len());
    Ok(())
}
