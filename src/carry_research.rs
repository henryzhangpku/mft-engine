//! `carry`: strategy v4 under a pre-registration protocol, on the ledger.
//!
//! Three phases, each one command, each appended to the hash-chained ledger:
//!
//! 1. `preregister`: the full specification (universe rule, parameters, cost
//!    model, the 60/40 in-sample / sealed out-of-sample split, the kill rule)
//!    is written to the ledger before any v4 result exists. It reads data
//!    timestamps only, to fix the window boundaries, never returns.
//! 2. `in-sample`: runs v4 on the first 60% of the trading window, on a panel
//!    physically cut at the split so later hours cannot reach it. A config
//!    that differs from the pre-registered one is a *design choice*: it needs
//!    a note, is logged as one, and at most `max_design_choices` are allowed.
//! 3. `oos`: runs the sealed last 40% once. It refuses if the config was not
//!    the one last run in-sample (no untested changes), if the data differ
//!    from what was pre-registered, or if an out-of-sample result is already
//!    on the ledger. `--force` overrides the last check only; it needs a note
//!    and the rerun is recorded as forced, next to the first result.
//!
//! The verdict is the pre-registered kill rule, applied by code.

use crate::artifacts::write_json;
use crate::carry::{self, CarryParams, CarryReport, Panel, DAY_MS};
use crate::clock::{format_utc, parse_utc, wall_now_ms};
use crate::ledger::{self, sha256_hex, LedgerEntry, NewRecord};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bootstrap {
    pub block_days: usize,
    pub resamples: usize,
    pub seed: u64,
}

/// The v4 specification file (`experiments/carry_v4.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CarrySpec {
    pub name: String,
    pub hypothesis: String,
    pub rationale: String,
    pub bars: PathBuf,
    pub funding: PathBuf,
    pub universe: PathBuf,
    pub benchmark: String,
    /// Share of the trading window (whole days) used for design.
    pub in_sample_fraction: f64,
    pub max_design_choices: usize,
    pub kill_rule: String,
    pub capital_note: String,
    pub bootstrap: Bootstrap,
    pub params: CarryParams,
}

impl CarrySpec {
    pub fn load(path: &Path) -> Result<(CarrySpec, String)> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Ok((toml::from_str(&text).context("parsing the v4 spec")?, text))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Phase {
    Preregister,
    InSample,
    Oos,
}

/// The windows, from the universe file's timestamps only.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Windows {
    pub trading_start: i64,
    pub split: i64,
    pub end: i64,
    pub trading_days: i64,
    pub in_sample_days: i64,
}

pub fn windows(spec: &CarrySpec, universe_doc: &Value) -> Result<Windows> {
    let ts = |k: &str| universe_doc[k].as_str().and_then(parse_utc).with_context(|| format!("universe file has no {k}"));
    let (trading_start, end) = (ts("formation_end")?, ts("window_end")?);
    let trading_days = (end - trading_start) / DAY_MS;
    let in_sample_days = (trading_days as f64 * spec.in_sample_fraction).floor() as i64;
    if in_sample_days < 1 || in_sample_days >= trading_days {
        bail!("split leaves an empty window");
    }
    Ok(Windows { trading_start, split: trading_start + in_sample_days * DAY_MS, end, trading_days, in_sample_days })
}

pub fn config_hash(p: &CarryParams) -> String {
    sha256_hex(serde_json::to_string(p).unwrap_or_default().as_bytes())
}

/// The pre-registered kill rule, in code: killed if the sealed
/// out-of-sample net Sharpe is not positive or its net P&L is not positive.
pub fn killed(oos: &CarryReport) -> bool {
    oos.sharpe_annualised.map_or(true, |s| s <= 0.0) || oos.pnl_net <= 0.0
}

/// The v4 entries already on the ledger, by phase.
pub struct History<'a> {
    pub preregistered: Option<&'a LedgerEntry>,
    pub in_sample: Vec<&'a LedgerEntry>,
    pub oos: Vec<&'a LedgerEntry>,
}

pub fn history<'a>(entries: &'a [LedgerEntry], name: &str) -> History<'a> {
    let of = |phase: &str| -> Vec<&'a LedgerEntry> {
        entries.iter().filter(|e| e.variant == name && e.result["phase"] == phase).collect()
    };
    History {
        preregistered: of("preregistered").into_iter().next(),
        in_sample: of("in_sample"),
        oos: of("oos"),
    }
}

/// What a run needs, loaded once.
struct Loaded {
    spec: CarrySpec,
    spec_text: String,
    universe_doc: Value,
    universe: Vec<String>,
    windows: Windows,
    data_files: Vec<String>,
    data_sha: String,
}

fn load(spec_path: &Path) -> Result<Loaded> {
    let (spec, spec_text) = CarrySpec::load(spec_path)?;
    let universe_doc: Value = serde_json::from_str(&std::fs::read_to_string(&spec.universe)?)?;
    let universe: Vec<String> = serde_json::from_value(universe_doc["universe"].clone())?;
    let windows = windows(&spec, &universe_doc)?;
    let paths = [&spec.bars, &spec.funding, &spec.universe];
    let mut bytes = Vec::new();
    for p in paths {
        bytes.extend(std::fs::read(p).with_context(|| format!("reading {}", p.display()))?);
    }
    Ok(Loaded {
        data_files: paths.iter().map(|p| p.display().to_string().replace('\\', "/")).collect(),
        data_sha: sha256_hex(&bytes),
        spec,
        spec_text,
        universe_doc,
        universe,
        windows,
    })
}

fn record(ledger_path: &Path, l: &Loaded, verdict: &str, result: Value) -> Result<LedgerEntry> {
    ledger::append_record(
        ledger_path,
        NewRecord {
            variant: &l.spec.name,
            hypothesis: &l.spec.hypothesis,
            base: "v4",
            overrides: serde_json::to_value(l.spec.params)?,
            experiment_file_sha256: sha256_hex(l.spec_text.as_bytes()),
            data_files: l.data_files.clone(),
            data_sha256: l.data_sha.clone(),
            verdict,
            verdict_rule: &l.spec.kill_rule,
            result,
            recorded_at: format_utc(wall_now_ms()),
        },
    )
}

/// Run v4 and the benchmark over one window and report both.
pub fn evaluate_window(spec: &CarrySpec, universe: &[String], full: &Panel, window: &str, from: i64, to: i64) -> Result<(CarryReport, CarryReport, carry::CarryRun)> {
    let b = &spec.bootstrap;
    let run = carry::run(&full.restricted_to(universe), &spec.params, from, to)?;
    let rep = carry::report(window, &run, spec.params.gross_notional, b.block_days, b.resamples, b.seed);
    let bench = carry::buy_and_hold(full, &spec.benchmark, spec.params.gross_notional, &spec.params.fills, from, to)?;
    let bench_rep = carry::report(&format!("{window}_buy_and_hold_{}", spec.benchmark), &bench, spec.params.gross_notional, b.block_days, b.resamples, b.seed);
    Ok((rep, bench_rep, run))
}

pub fn print_reports(reports: &[&CarryReport]) {
    println!(
        "{:<28} {:>5} {:>9} {:>9} {:>8} {:>9} {:>7} {:>8} {:>8} {:>8} {:>18}",
        "window", "days", "pnl_net", "funding", "costs", "price", "sharpe", "ann_%", "max_dd", "turn/d", "mean/day CI95"
    );
    for r in reports {
        println!(
            "{:<28} {:>5} {:>9.2} {:>9.2} {:>8.2} {:>9.2} {:>7} {:>8.1} {:>8.2} {:>8.2} {:>18}",
            r.window,
            r.days,
            r.pnl_net,
            r.funding_pnl,
            r.fees + r.slippage,
            r.price_pnl,
            r.sharpe_annualised.map_or("n/a".into(), |s| format!("{s:.2}")),
            r.annualised_return_pct,
            r.max_drawdown,
            r.turnover_per_day,
            r.mean_daily_ci95.map_or("n/a".into(), |(lo, hi)| format!("[{lo:.2}, {hi:.2}]")),
        );
    }
}

pub struct Options<'a> {
    pub spec: &'a Path,
    pub ledger: &'a Path,
    pub note: Option<String>,
    pub force: bool,
    /// Directory for the per-phase result files.
    pub results_dir: &'a Path,
}

pub fn run(phase: Phase, opts: Options) -> Result<LedgerEntry> {
    let l = load(opts.spec)?;
    let entries = ledger::read(opts.ledger)?;
    ledger::verify(&entries).context("the ledger is broken")?;
    let h = history(&entries, &l.spec.name);
    let w = l.windows;
    let chash = config_hash(&l.spec.params);

    if phase == Phase::Preregister {
        if let Some(e) = h.preregistered {
            bail!("{} is already pre-registered (ledger #{}); a specification is written once", l.spec.name, e.seq);
        }
        let result = json!({
            "phase": "preregistered",
            "spec": l.spec,
            "config_hash": chash,
            "universe_rule": l.universe_doc["rule"],
            "universe": l.universe,
            "window": {
                "data_start": l.universe_doc["window_start"],
                "formation_end_trading_start": format_utc(w.trading_start),
                "in_sample": [format_utc(w.trading_start), format_utc(w.split)],
                "out_of_sample_sealed": [format_utc(w.split), format_utc(w.end)],
                "trading_days": w.trading_days,
                "in_sample_days": w.in_sample_days,
            },
            "note": opts.note,
        });
        let e = record(opts.ledger, &l, "preregistered", result)?;
        println!("ledger #{} {} preregistered ({}); in-sample {} to {}, sealed {} to {}",
            e.seq, l.spec.name, &e.hash[..12], format_utc(w.trading_start), format_utc(w.split), format_utc(w.split), format_utc(w.end));
        return Ok(e);
    }

    let Some(pre) = h.preregistered else {
        bail!("{} has no pre-registration on the ledger; run `carry preregister` first", l.spec.name);
    };
    if pre.data_sha256 != l.data_sha {
        bail!("the data files differ from the ones pre-registered (ledger #{})", pre.seq);
    }
    let pre_hash = pre.result["config_hash"].as_str().unwrap_or_default().to_string();
    let full = Panel::load(&l.spec.bars, &l.spec.funding)?;

    match phase {
        Phase::Preregister => unreachable!(),
        Phase::InSample => {
            if let Some(e) = h.oos.first() {
                bail!("the out-of-sample window was already run (ledger #{}); in-sample design is closed", e.seq);
            }
            let seen = |hash: &str| h.in_sample.iter().any(|e| e.result["config_hash"] == hash);
            let is_design_choice = chash != pre_hash && !seen(&chash);
            if is_design_choice {
                let used = h.in_sample.iter().filter(|e| e.result["design_choice"] == true).count();
                if used >= l.spec.max_design_choices {
                    bail!("{used} design choices already logged; the pre-registered maximum is {}", l.spec.max_design_choices);
                }
                if opts.note.as_deref().map_or(true, |n| n.trim().is_empty()) {
                    bail!("this config differs from the pre-registered one: a design choice needs --note saying what changed and why");
                }
            }
            // The in-sample run never sees an hour after the split.
            let panel = full.truncated_after(w.split);
            let (rep, bench, run) = evaluate_window(&l.spec, &l.universe, &panel, "in_sample", w.trading_start, w.split)?;
            print_reports(&[&rep, &bench]);
            write_json(&opts.results_dir.join("carry_v4_in_sample.json"), &json!({"report": rep, "benchmark": bench, "params": l.spec.params, "rebalances": run.rebalances}))?;
            let verdict = if is_design_choice { "design_choice" } else { "in_sample" };
            let e = record(opts.ledger, &l, verdict, json!({
                "phase": "in_sample", "design_choice": is_design_choice, "config_hash": chash,
                "note": opts.note, "window": [format_utc(w.trading_start), format_utc(w.split)],
                "report": rep, "benchmark": bench,
            }))?;
            println!("ledger #{} {} {} ({})", e.seq, l.spec.name, verdict, &e.hash[..12]);
            Ok(e)
        }
        Phase::Oos => {
            let Some(last_is) = h.in_sample.last() else {
                bail!("no in-sample run on the ledger; the sealed window comes last");
            };
            if last_is.result["config_hash"] != chash.as_str() {
                bail!("this config is not the one last run in-sample (ledger #{}); every change must be run and logged in-sample first", last_is.seq);
            }
            let forced = match (h.oos.first(), opts.force) {
                (None, _) => false,
                (Some(e), false) => bail!(
                    "the sealed out-of-sample window was already run (ledger #{}, verdict {}). It is run once; a rerun needs --force and --note, and is recorded as forced",
                    e.seq, e.verdict
                ),
                (Some(_), true) => {
                    if opts.note.as_deref().map_or(true, |n| n.trim().is_empty()) {
                        bail!("a forced out-of-sample rerun needs --note saying why (for example, a post-OOS bug fix)");
                    }
                    true
                }
            };
            let (rep, bench, run) = evaluate_window(&l.spec, &l.universe, &full, "out_of_sample", w.split, w.end)?;
            print_reports(&[&rep, &bench]);
            let outcome = if killed(&rep) { "killed" } else { "kept" };
            let verdict = if forced { format!("oos_forced_rerun_{outcome}") } else { outcome.to_string() };
            let name = if forced { format!("carry_v4_oos_rerun_{}.json", h.oos.len()) } else { "carry_v4_oos.json".into() };
            write_json(&opts.results_dir.join(name), &json!({"report": rep, "benchmark": bench, "params": l.spec.params, "rebalances": run.rebalances}))?;
            let e = record(opts.ledger, &l, &verdict, json!({
                "phase": "oos", "forced": forced, "config_hash": chash, "note": opts.note,
                "window": [format_utc(w.split), format_utc(w.end)],
                "kill_rule_triggered": outcome == "killed",
                "report": rep, "benchmark": bench, "fingerprint": rep.fingerprint,
            }))?;
            println!("ledger #{} {} {} ({}), fingerprint {}", e.seq, l.spec.name, verdict, &e.hash[..12], rep.fingerprint);
            Ok(e)
        }
    }
}
