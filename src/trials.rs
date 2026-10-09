//! `ledger dsr`: the deflated Sharpe ratio of a ledger entry, deflated by
//! every strategy trial on the ledger.
//!
//! The ledger stores each result's summary, not its return series, so every
//! trial is replayed from what its entry recorded (base config and
//! overrides, data files, window, time key), and the replay must reproduce
//! the decision fingerprint on the ledger, and its data the recorded data
//! hash, or the command refuses. Nothing is appended.
//!
//! **What counts as a trial (N).** One ledger entry that carries a result of
//! a strategy evaluation, counted once per distinct (strategy config, data,
//! window, time key):
//!
//! * counted: every `experiment` variant (entries 0 to 13) and every v4 run,
//!   in-sample and sealed out-of-sample (15 to 17), including variants that
//!   never traded (their Sharpe is taken as 0) and killed ones;
//! * not counted: pre-registrations (14), which have no result, and exact
//!   reruns of a config already counted on the same data and window (7
//!   repeats 0), which test nothing new.
//!
//! **Return series.** The engine strategies (v1 to v3) trade 1-minute bars:
//! their series is P&L per minute (equity at the last bar of each UTC
//! minute, differenced; a minute with no bar has zero P&L) over the entry's
//! full window. v4 is evaluated on daily P&L (00:00 UTC marks), as its
//! protocol states. Sharpe ratios are compared across trials annualised
//! (sqrt(525,600) per minute, sqrt(365) per day; crypto trades every day),
//! so V is the variance of annualised trial Sharpe ratios, and SR0 is
//! converted back to the entry's own period before the PSR is taken.

use crate::backtest::{load_session, run_replay};
use crate::carry::{self, CarryParams, Panel};
use crate::carry_research::{self, CarrySpec};
use crate::clock::TimeKey;
use crate::dsr::{self, Deflated};
use crate::engine::EngineConfig;
use crate::experiment::build_config;
use crate::ledger::{sha256_hex, LedgerEntry};
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MINUTES_PER_YEAR: f64 = 525_600.0;
pub const DAYS_PER_YEAR: f64 = 365.0;

/// One distinct strategy trial, replayed.
#[derive(Debug, Clone, Serialize)]
pub struct Trial {
    /// The entry first recording it, then any exact reruns.
    pub entries: Vec<u64>,
    pub variant: String,
    /// "minute" or "day".
    pub period: &'static str,
    pub periods_per_year: f64,
    pub returns: Vec<f64>,
    /// Annualised Sharpe ratio; 0 when the P&L never varied (no trades).
    pub sr_annualised: f64,
    /// The decision fingerprint the replay produced.
    pub fingerprint: String,
}

/// How a replay matched its ledger entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Reproduced {
    /// The decision fingerprint itself.
    Fingerprint,
    /// Every field of the logged report except the fingerprint.
    ReportOnly,
}

/// What makes two entries the same trial, or `None` if the entry is not a
/// strategy evaluation (a pre-registration).
pub fn trial_key(e: &LedgerEntry) -> Option<String> {
    if e.base == "v4" {
        let phase = e.result["phase"].as_str()?;
        if phase != "in_sample" && phase != "oos" {
            return None;
        }
        return Some(format!("v4|{}|{}|{}", e.result["config_hash"], e.data_sha256, e.result["window"]));
    }
    e.result.get("full")?;
    Some(format!("engine|{}|{}|{}|{}|full", e.base, e.overrides, e.data_sha256, time_key_of(e).name()))
}

/// The time key an engine entry was replayed on. Entries written before
/// arrival keying existed carry none: they are pinned to exchange time.
pub fn time_key_of(e: &LedgerEntry) -> TimeKey {
    match e.result.get("time_key").and_then(|v| v.as_str()) {
        Some("arrival") => TimeKey::Arrival,
        _ => TimeKey::Exchange,
    }
}

/// P&L per UTC minute from (bar time, equity) samples: the last equity in
/// each minute, carried through minutes with no bar, differenced from a flat
/// start (equity 0 before the first bar).
pub fn minute_pnl(curve: &[(i64, f64)]) -> Vec<f64> {
    let mut last: BTreeMap<i64, f64> = BTreeMap::new();
    for (ts, eq) in curve {
        last.insert(ts.div_euclid(60_000), *eq);
    }
    let (Some(&first), Some(&end)) = (last.keys().next(), last.keys().next_back()) else {
        return vec![];
    };
    let (mut prev, mut held, mut out) = (0.0, 0.0, Vec::new());
    for m in first..=end {
        if let Some(eq) = last.get(&m) {
            held = *eq;
        }
        out.push(held - prev);
        prev = held;
    }
    out
}

fn annualised(returns: &[f64], periods_per_year: f64) -> f64 {
    dsr::Moments::of(returns).map_or(0.0, |m| m.sharpe() * periods_per_year.sqrt())
}

fn data_sha(root: &Path, files: &[String]) -> Result<String> {
    let mut bytes = Vec::new();
    for f in files {
        let p = root.join(f);
        bytes.extend(std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?);
    }
    Ok(sha256_hex(&bytes))
}

/// Replay an engine (v1 to v3, v5) entry over its full window; per-minute
/// P&L, or per-day for the hourly strategy v5.
async fn replay_engine(root: &Path, e: &LedgerEntry) -> Result<(Vec<f64>, String)> {
    if data_sha(root, &e.data_files)? != e.data_sha256 {
        bail!("ledger #{}: the data files differ from the ones it ran on", e.seq);
    }
    let set: BTreeMap<String, toml::Value> = serde_json::from_value(e.overrides.clone()).context("overrides")?;
    let config: EngineConfig = build_config(&e.base, &set)?;
    let paths: Vec<PathBuf> = e.data_files.iter().map(|f| root.join(f)).collect();
    let key = time_key_of(e);
    let (envelopes, _) = load_session(&paths, Some(key))?;
    let run = run_replay("full", envelopes, config, key).await;
    let logged = e.result["full"]["decisions_fingerprint"].as_str().unwrap_or_default().to_string();
    if run.report.decisions_fingerprint != logged {
        // Entries 0 to 6 carry fingerprints that no committed build gives
        // back, though every number in their reports does. Accept that only
        // if the whole logged report, fingerprint aside, is reproduced
        // exactly: fills, P&L, costs, vetoes, drawdown. (Fields added to
        // the report after the entry was written are not compared.)
        let replayed = serde_json::to_value(&run.report)?;
        let logged_report = e.result["full"].as_object().ok_or_else(|| anyhow!("ledger #{}: no full report", e.seq))?;
        let same = logged_report.iter().all(|(k, v)| k == "decisions_fingerprint" || replayed.get(k) == Some(v));
        if !same {
            bail!("ledger #{}: replay (fingerprint {}) does not reproduce the logged result ({logged})", e.seq, run.report.decisions_fingerprint);
        }
    }
    let mut curve = run.equity.clone();
    if let Some(&(t, _)) = curve.last() {
        curve.push((t, run.report.pnl_after_costs)); // the final mark
    }
    let returns = if e.base == "v5" { crate::metrics::daily_pnl(&curve) } else { minute_pnl(&curve) };
    Ok((returns, run.report.decisions_fingerprint))
}

/// The v4 context, loaded once.
struct V4 {
    universe: Vec<String>,
    windows: carry_research::Windows,
    panel: Panel,
}

fn load_v4(root: &Path) -> Result<V4> {
    let (mut spec, _) = CarrySpec::load(&root.join("experiments/carry_v4.toml"))?;
    for p in [&mut spec.bars, &mut spec.funding, &mut spec.universe] {
        *p = root.join(&*p);
    }
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&spec.universe)?)?;
    let universe: Vec<String> = serde_json::from_value(doc["universe"].clone())?;
    let windows = carry_research::windows(&spec, &doc)?;
    let panel = Panel::load(&spec.bars, &spec.funding)?;
    Ok(V4 { universe, windows, panel })
}

/// Replay a v4 entry over its window; daily P&L.
fn replay_v4(root: &Path, v4: &mut Option<V4>, e: &LedgerEntry) -> Result<(Vec<f64>, String)> {
    if data_sha(root, &e.data_files)? != e.data_sha256 {
        bail!("ledger #{}: the data files differ from the ones it ran on", e.seq);
    }
    if v4.is_none() {
        *v4 = Some(load_v4(root)?);
    }
    let v = v4.as_ref().expect("loaded");
    let params: CarryParams = serde_json::from_value(e.overrides.clone())?;
    let w = v.windows;
    let (panel, from, to) = if e.result["phase"] == "oos" {
        (v.panel.clone(), w.split, w.end)
    } else {
        (v.panel.truncated_after(w.split), w.trading_start, w.split)
    };
    let run = carry::run(&panel.restricted_to(&v.universe), &params, from, to)?;
    let logged = e.result["report"]["fingerprint"].as_str().unwrap_or_default();
    if run.fingerprint() != logged {
        bail!("ledger #{}: replay fingerprint {} does not reproduce the logged {logged}", e.seq, run.fingerprint());
    }
    Ok((carry::daily_pnl(&run.equity), run.fingerprint()))
}

/// Every distinct trial on the ledger, in ledger order, each replayed.
pub async fn trials(root: &Path, entries: &[LedgerEntry]) -> Result<Vec<Trial>> {
    let mut by_key: BTreeMap<String, usize> = BTreeMap::new();
    let mut out: Vec<Trial> = Vec::new();
    let mut v4 = None;
    for e in entries {
        let Some(key) = trial_key(e) else { continue };
        if let Some(&i) = by_key.get(&key) {
            out[i].entries.push(e.seq);
            continue;
        }
        let (period, ppy, (returns, fingerprint)) = if e.base == "v4" {
            ("day", DAYS_PER_YEAR, replay_v4(root, &mut v4, e)?)
        } else if e.base == "v5" {
            ("day", DAYS_PER_YEAR, replay_engine(root, e).await?)
        } else {
            ("minute", MINUTES_PER_YEAR, replay_engine(root, e).await?)
        };
        by_key.insert(key, out.len());
        out.push(Trial {
            entries: vec![e.seq],
            variant: e.variant.clone(),
            period,
            periods_per_year: ppy,
            sr_annualised: annualised(&returns, ppy),
            returns,
            fingerprint,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryDsr {
    pub seq: u64,
    pub variant: String,
    pub verdict: String,
    pub period: &'static str,
    pub n_trials: usize,
    /// Sample variance of the annualised trial Sharpe ratios.
    pub var_trial_sr_annualised: f64,
    pub sr_annualised: f64,
    pub sr0_annualised: f64,
    pub reproduced: Reproduced,
    pub deflated: Deflated,
    /// Sensitivity: the same, deflated only by trials on the same period
    /// (minute or day), whose annualised Sharpe ratios are comparable.
    pub same_period_trials: usize,
    pub same_period_sr0_annualised: f64,
    pub same_period_dsr: f64,
}

/// The fingerprint an entry logged for its decisions.
pub fn logged_fingerprint(e: &LedgerEntry) -> &str {
    let v = if e.base == "v4" { &e.result["report"]["fingerprint"] } else { &e.result["full"]["decisions_fingerprint"] };
    v.as_str().unwrap_or_default()
}

/// SR0 and the DSR against `group`'s trials, for the entry's `trial`.
fn against(trial: &Trial, group: &[&Trial]) -> Result<(f64, f64, Deflated)> {
    let srs: Vec<f64> = group.iter().map(|t| t.sr_annualised).collect();
    let var = dsr::variance(&srs);
    let sr0_annual = dsr::expected_max_sharpe(group.len(), var)?;
    let deflated = dsr::deflated_sharpe(&trial.returns, sr0_annual / trial.periods_per_year.sqrt(), group.len())?;
    Ok((var, sr0_annual, deflated))
}

/// The DSR of one entry against the whole ledger's trials.
pub fn entry_dsr(entries: &[LedgerEntry], trials: &[Trial], seq: u64) -> Result<EntryDsr> {
    let e = entries.iter().find(|e| e.seq == seq).ok_or_else(|| anyhow!("no ledger entry #{seq}"))?;
    let trial = trials
        .iter()
        .find(|t| t.entries.contains(&seq))
        .ok_or_else(|| anyhow!("ledger #{seq} ({}, {}) is not a strategy evaluation: it has no return series", e.variant, e.verdict))?;
    let n = trials.len();
    let all: Vec<&Trial> = trials.iter().collect();
    let (var, sr0_annual, deflated) = against(trial, &all).with_context(|| format!("ledger #{seq} ({})", e.variant))?;
    let peers: Vec<&Trial> = trials.iter().filter(|t| t.period == trial.period).collect();
    let (_, peer_sr0, peer) = against(trial, &peers)?;
    let reproduced = if logged_fingerprint(e) == trial.fingerprint { Reproduced::Fingerprint } else { Reproduced::ReportOnly };
    Ok(EntryDsr {
        seq,
        variant: e.variant.clone(),
        verdict: e.verdict.clone(),
        period: trial.period,
        n_trials: n,
        var_trial_sr_annualised: var,
        sr_annualised: deflated.sr * trial.periods_per_year.sqrt(),
        sr0_annualised: sr0_annual,
        reproduced,
        deflated,
        same_period_trials: peers.len(),
        same_period_sr0_annualised: peer_sr0,
        same_period_dsr: peer.dsr,
    })
}

pub fn print_entry(d: &EntryDsr) {
    let m = &d.deflated.moments;
    println!("ledger #{} {} ({}), P&L per {}", d.seq, d.variant, d.verdict, d.period);
    println!("  T        {}", m.t);
    println!("  SR       {:+.6} per {} ({:+.2} annualised)", d.deflated.sr, d.period, d.sr_annualised);
    println!("  skew     {:+.4}", m.skew);
    println!("  kurtosis {:.4} (normal = 3)", m.kurtosis);
    println!("  N        {} distinct strategy trials on the ledger", d.n_trials);
    println!("  V[SR]    {:.4} (annualised trial Sharpe ratios)", d.var_trial_sr_annualised);
    println!("  SR0      {:+.6} per {} ({:+.2} annualised): expected max Sharpe of N null trials", d.deflated.sr0, d.period, d.sr0_annualised);
    println!("  PSR(0)   {:.6}", d.deflated.psr_vs_zero);
    println!("  DSR      {:.6}  = P(true SR > SR0), z = {:+.3}", d.deflated.dsr, d.deflated.z);
    println!(
        "  (deflated only by the {} trials on P&L per {}: SR0 {:+.2} annualised, DSR {:.6})",
        d.same_period_trials, d.period, d.same_period_sr0_annualised, d.same_period_dsr
    );
    if d.reproduced == Reproduced::ReportOnly {
        println!("  (replay reproduces every number of the logged report, but not its logged fingerprint)");
    }
}

pub fn print_table(rows: &[Result<EntryDsr, String>], seqs: &[u64]) {
    if let Some(Ok(d)) = rows.iter().find(|r| r.is_ok()) {
        println!("N = {} distinct strategy trials, V[annualised SR] = {:.2}, SR0 = {:+.2} annualised", d.n_trials, d.var_trial_sr_annualised, d.sr0_annualised);
    }
    println!(
        "{:>3} {:<30} {:<14} {:>5} {:>8} {:>7} {:>7} {:>9} {:>8} {:>13}",
        "#", "variant", "verdict", "T", "SR_ann", "skew", "kurt", "PSR(0)", "DSR", "DSR same-per."
    );
    for (seq, r) in seqs.iter().zip(rows) {
        match r {
            Ok(d) => {
                let m = &d.deflated.moments;
                println!(
                    "{:>3} {:<30} {:<14} {:>5} {:>8.2} {:>7.2} {:>7.1} {:>9.4} {:>8.4} {:>13.4}{}",
                    seq, d.variant, d.verdict, m.t, d.sr_annualised, m.skew, m.kurtosis, d.deflated.psr_vs_zero, d.deflated.dsr, d.same_period_dsr,
                    if d.reproduced == Reproduced::ReportOnly { "  *" } else { "" }
                );
            }
            Err(msg) => println!("{seq:>3} {msg}"),
        }
    }
    if rows.iter().any(|r| matches!(r, Ok(d) if d.reproduced == Reproduced::ReportOnly)) {
        println!("* the replay reproduces every number of the logged report, but not its logged decision fingerprint");
    }
}
