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
    /// The v4 (funding carry) specification; its data and ledger entries
    /// feed the v4 section. Missing files mean no v4 section.
    pub carry_spec: PathBuf,
    /// Polymarket daily ladders. Loaded apart from `data` so the v1 and v2
    /// replays above are exactly the ledger's; used for v2b and the
    /// Kalshi/Polymarket agreement. Missing file means no Polymarket section.
    pub polymarket: PathBuf,
    /// The v5 forward paper run's status file; missing means no card.
    pub forward_status: PathBuf,
    pub out: PathBuf,
}

/// How often Kalshi's hourly and Polymarket's daily ladders put P(close >
/// spot) on the same side of 0.5, at every bar where both have a usable
/// reading (the gate's own view: fresh, unexpired, spanning spot).
fn agreement(events: &[Event]) -> (Value, BTreeMap<String, Vec<Value>>) {
    use crate::prediction::{PredictionParams, PredictionState, KALSHI, POLYMARKET};
    let mut state = PredictionState::new(PredictionParams { enabled: true, ..PredictionParams::default() });
    let mut per_coin: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new(); // bars, both, agree
    let mut series: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for e in events {
        match e {
            Event::PredictionMarket(p) => state.on_snapshot(p),
            Event::Bar(b) => {
                let k = state.venue_view(KALSHI, &b.coin, b.close, b.ts).p_up;
                let p = state.venue_view(POLYMARKET, &b.coin, b.close, b.ts).p_up;
                let c = per_coin.entry(b.coin.clone()).or_default();
                c.0 += 1;
                if let Some(pp) = p {
                    series.entry(b.coin.clone()).or_default().push(json!([b.ts / 1000, round(pp, 3)]));
                }
                if let (Some(k), Some(p)) = (k, p) {
                    c.1 += 1;
                    if (k > 0.5) == (p > 0.5) && k != 0.5 && p != 0.5 {
                        c.2 += 1;
                    }
                }
            }
            _ => {}
        }
    }
    let rows: serde_json::Map<String, Value> = per_coin
        .iter()
        .map(|(coin, (bars, both, agree))| {
            let rate = if *both > 0 { Some(round(*agree as f64 / *both as f64, 3)) } else { None };
            (coin.clone(), json!({ "bars": bars, "both": both, "agree": agree, "rate": rate }))
        })
        .collect();
    let (both, agree): (u64, u64) = per_coin.values().fold((0, 0), |a, c| (a.0 + c.1, a.1 + c.2));
    let rate = if both > 0 { Some(round(agree as f64 / both as f64, 3)) } else { None };
    (json!({ "per_coin": rows, "both": both, "agree": agree, "rate": rate }), series)
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
            Event::PredictionMarket(p) if p.venue == crate::prediction::KALSHI => {
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

    // Polymarket: v2b on the same inputs plus the Polymarket ladders, and
    // how often the two markets agree.
    let mut polymarket = Value::Null;
    if inputs.polymarket.exists() {
        let mut files = inputs.data.clone();
        files.push(inputs.polymarket.clone());
        let with_poly = load_events(&files)?;
        let (agree, series) = agreement(&with_poly);
        let (eval, run) = evaluate("v2b", &with_poly, EngineConfig::v2b()).await;
        decisions.insert("v2b".into(), run.decisions.iter().map(decision_json).collect());
        let curve: Vec<Value> = run.equity.iter().step_by(5).map(|(t, e)| json!([t / 1000, round(*e, 2)])).collect();
        equity.insert("v2b".into(), curve.into());
        println!("Kalshi/Polymarket agreement: {agree}");
        polymarket = json!({ "agreement": agree, "p_up": series, "v2b": eval });
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
                // v4 entries carry one window's report (none when pre-registered):
                // the in-sample P&L as "PnL", the sealed window's as "holdout".
                "full_pnl": if e.base == "v4" { e.result["report"]["pnl_net"].clone() } else { e.result["full"]["pnl_after_costs"].clone() },
                "holdout_pnl": if e.base == "v4" {
                    if e.result["phase"] == "oos" { e.result["report"]["pnl_net"].clone() } else { Value::Null }
                } else {
                    e.result["second_half"]["pnl_after_costs"].clone()
                },
                "fills": if e.base == "v4" { e.result["report"]["trades"].clone() } else { e.result["full"]["fills"].clone() },
            })
        })
        .collect();

    // v5 (hourly trend) as recorded on the ledger: its one historical run,
    // read from the entry itself (the 208-day hourly window is not replayed
    // here), and the forward paper run's status, if it is running.
    let v5 = ledger::read(&inputs.ledger)?
        .iter()
        .rev()
        .find(|e| e.base == "v5" && e.result.get("full").is_some())
        .map(|e| {
            json!({
                "seq": e.seq, "verdict": e.verdict, "verdict_rule": e.verdict_rule, "recorded_at": e.recorded_at,
                "evaluation": { "name": e.variant, "full": e.result["full"], "first_half": e.result["first_half"], "second_half": e.result["second_half"] },
            })
        })
        .unwrap_or(Value::Null);
    let forward_v5 = read_json_or_null(&inputs.forward_status);

    let carry = carry_section(&inputs.carry_spec, &inputs.ledger).unwrap_or_else(|e| {
        println!("no v4 section: {e:#}");
        Value::Null
    });

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
        "carry": carry,
        "polymarket": polymarket,
        "v5": v5,
        "forward_v5": forward_v5,
    });
    write_json(&inputs.out, &doc)?;
    let size = std::fs::metadata(&inputs.out).map(|m| m.len()).unwrap_or(0);
    println!("wrote {} ({} KB, {} posts, {} ledger entries)", inputs.out.display(), size / 1024, posts.len(), ledger_rows.len());
    Ok(())
}

/// Strategy v4 as recorded on the ledger: the pre-registered spec, every
/// v4 entry, the in-sample and sealed out-of-sample reports exactly as
/// logged, and equity curves replayed with the logged configs (the replay
/// reproduces the logged fingerprint; it is not a new result).
fn carry_section(spec_path: &Path, ledger_path: &Path) -> Result<Value> {
    use crate::carry::{self, Panel};
    use crate::carry_research::{evaluate_window, history, windows, CarrySpec};
    let (spec, _) = CarrySpec::load(spec_path)?;
    let entries = ledger::read(ledger_path)?;
    let h = history(&entries, &spec.name);
    let pre = h.preregistered.ok_or_else(|| anyhow::anyhow!("v4 is not pre-registered"))?;
    let universe_doc: Value = serde_json::from_str(&std::fs::read_to_string(&spec.universe)?)?;
    let universe: Vec<String> = serde_json::from_value(universe_doc["universe"].clone())?;
    let w = windows(&spec, &universe_doc)?;
    let full = Panel::load(&spec.bars, &spec.funding)?;
    let curve = |run: &carry::CarryRun| -> Vec<Value> {
        run.equity.iter().step_by(4).chain(run.equity.last()).map(|(t, e)| json!([t / 1000, round(*e, 2)])).collect()
    };
    let replay = |entry: &crate::ledger::LedgerEntry, panel: &Panel, from: i64, to: i64| -> Result<Value> {
        let mut s = spec.clone();
        s.params = serde_json::from_value(entry.overrides.clone())?;
        let (_, _, run) = evaluate_window(&s, &universe, panel, "replay", from, to)?;
        let bench = carry::buy_and_hold(panel, &s.benchmark, s.params.gross_notional, &s.params.fills, from, to)?;
        Ok(json!({ "fingerprint": run.fingerprint(), "strategy": curve(&run), "benchmark": curve(&bench),
            "last_rebalance": run.rebalances.iter().rev().find(|r| !r.longs.is_empty()) }))
    };
    let summary = |e: &crate::ledger::LedgerEntry| json!({
        "seq": e.seq, "verdict": e.verdict, "phase": e.result["phase"], "note": e.result["note"],
        "recorded_at": e.recorded_at, "hash": &e.hash[..12], "config": e.overrides,
        "report": e.result["report"], "benchmark": e.result["benchmark"],
    });
    let is_entry = h.in_sample.last();
    let oos_entry = h.oos.first();
    Ok(json!({
        "name": spec.name,
        "hypothesis": spec.hypothesis,
        "rationale": spec.rationale,
        "kill_rule": spec.kill_rule,
        "capital_note": spec.capital_note,
        "preregistered": { "seq": pre.seq, "recorded_at": pre.recorded_at, "hash": &pre.hash[..12], "config": pre.overrides },
        "universe_rule": universe_doc["rule"],
        "universe": universe,
        "windows": {
            "formation": [universe_doc["window_start"], format_utc(w.trading_start)],
            "in_sample": [format_utc(w.trading_start), format_utc(w.split)],
            "out_of_sample": [format_utc(w.split), format_utc(w.end)],
        },
        "entries": h.in_sample.iter().chain(h.oos.iter()).map(|e| summary(e)).collect::<Vec<_>>(),
        "in_sample": is_entry.map(|e| summary(e)),
        "oos": oos_entry.map(|e| summary(e)),
        "oos_reruns": h.oos.len().saturating_sub(1),
        "curves": {
            "in_sample": is_entry.map(|e| replay(e, &full.truncated_after(w.split), w.trading_start, w.split)).transpose()?,
            "out_of_sample": oos_entry.map(|e| replay(e, &full, w.split, w.end)).transpose()?,
        },
    }))
}
