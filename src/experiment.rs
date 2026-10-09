//! `experiment`: from an idea to a recorded result in one command.
//!
//! A small TOML file lists variants. Each names a base strategy (`v1` or
//! `v2`), optional config overrides by dotted path, and a hypothesis written
//! before it is run. Every variant goes through the same backtester as
//! everything else (full sample and both halves), the comparison table is
//! printed, and each result is appended to the hash-chained ledger with a
//! verdict decided by a fixed rule, so kept and killed ideas are recorded the
//! same way.
//!
//! ```toml
//! data = ["data/bars_1m.jsonl", "data/kalshi_ladders.jsonl"]
//!
//! [[variant]]
//! name = "v1_momentum"
//! base = "v1"
//! hypothesis = "Baseline."
//!
//! [[variant]]
//! name = "v1_entry_z_3"
//! base = "v1"
//! hypothesis = "Fewer, stronger entries survive costs."
//! set = { "strategy.entry_z" = 3.0 }
//! ```
//!
//! An optional top-level `time_key = "arrival"` replays a session recorded
//! live by arrival time. Without it a file replays by exchange time, which is
//! what every experiment already on the ledger ran on; new entries record
//! the key they used in their result.

use crate::backtest::{evaluate_keyed, load_session, print_table, Evaluation};
use crate::clock::{format_utc, wall_now_ms, TimeKey};
use crate::engine::EngineConfig;
use crate::ledger::{self, sha256_hex, NewRecord};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The verdict rule, fixed in code and copied into every ledger entry.
pub const VERDICT_RULE: &str =
    "kept if second-half (holdout) PnL after costs > 0 and > the baseline's second-half PnL; else killed. The first variant is the baseline.";

#[derive(Debug, Deserialize)]
pub struct ExperimentFile {
    pub data: Vec<PathBuf>,
    /// Replay time key; absent means exchange (the ledger's existing key).
    #[serde(default)]
    pub time_key: TimeKey,
    /// If true, `run` refuses unless every variant was first written to the
    /// ledger by `--preregister` from this exact file (same SHA-256), so the
    /// rule provably existed before any result.
    #[serde(default)]
    pub require_preregistration: bool,
    #[serde(rename = "variant")]
    pub variants: Vec<Variant>,
}

#[derive(Debug, Deserialize)]
pub struct Variant {
    pub name: String,
    pub base: String,
    pub hypothesis: String,
    #[serde(default)]
    pub set: BTreeMap<String, toml::Value>,
}

/// Build a config from a base name and dotted-path overrides, by going
/// through JSON: serialise the base, replace the named fields, deserialise.
/// An unknown path or a wrong type is an error, never silently ignored.
pub fn build_config(base: &str, set: &BTreeMap<String, toml::Value>) -> Result<EngineConfig> {
    let base_config = match base {
        "v1" => EngineConfig::v1(),
        "v2" => EngineConfig::v2(),
        "v2b" => EngineConfig::v2b(),
        "v3" => EngineConfig::v3(),
        other => bail!("unknown base {other:?}; use \"v1\", \"v2\", \"v2b\" or \"v3\""),
    };
    let mut json = serde_json::to_value(base_config)?;
    for (path, value) in set {
        let mut node = &mut json;
        for part in path.split('.') {
            node = node
                .get_mut(part)
                .ok_or_else(|| anyhow!("unknown config field {path:?}"))?;
        }
        let new = serde_json::to_value(value)?;
        if std::mem::discriminant(node) != std::mem::discriminant(&new) && !(node.is_number() && new.is_number()) {
            bail!("config field {path:?} expects {node}, got {new}");
        }
        *node = new;
    }
    serde_json::from_value(json).context("overrides produced an invalid config")
}

fn verdict(eval: &Evaluation, baseline_holdout: Option<f64>) -> &'static str {
    let holdout = eval.second_half.pnl_after_costs;
    match baseline_holdout {
        None => "baseline",
        Some(b) if holdout > 0.0 && holdout > b => "kept",
        Some(_) => "killed",
    }
}

/// `experiment --preregister`: write every variant's specification (base,
/// overrides, the full resolved config, hypothesis, data hash, verdict rule)
/// to the ledger with verdict "preregistered", without running anything.
pub fn preregister(file: &Path, ledger_path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let spec: ExperimentFile = toml::from_str(&text).context("parsing experiment TOML")?;
    let existing = ledger::read(ledger_path)?;
    ledger::verify(&existing)?;
    let file_sha = sha256_hex(text.as_bytes());
    if existing.iter().any(|e| e.verdict == "preregistered" && e.experiment_file_sha256 == file_sha) {
        bail!("{} is already pre-registered on the ledger", file.display());
    }
    let mut data_bytes = Vec::new();
    for p in &spec.data {
        data_bytes.extend(std::fs::read(p).with_context(|| format!("reading {}", p.display()))?);
    }
    let data_sha = sha256_hex(&data_bytes);
    for v in &spec.variants {
        let config = build_config(&v.base, &v.set).with_context(|| format!("variant {}", v.name))?;
        let entry = ledger::append_record(
            ledger_path,
            NewRecord {
                variant: &v.name,
                hypothesis: &v.hypothesis,
                base: &v.base,
                overrides: serde_json::to_value(&v.set)?,
                experiment_file_sha256: file_sha.clone(),
                data_files: spec.data.iter().map(|p| p.display().to_string()).collect(),
                data_sha256: data_sha.clone(),
                verdict: "preregistered",
                verdict_rule: VERDICT_RULE,
                result: serde_json::json!({ "phase": "preregistered", "config": config, "time_key": spec.time_key }),
                recorded_at: format_utc(wall_now_ms()),
            },
        )?;
        println!("ledger #{} {} -> preregistered ({})", entry.seq, v.name, &entry.hash[..12]);
    }
    Ok(())
}

pub async fn run(file: &Path, ledger_path: &Path) -> Result<Vec<Evaluation>> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let spec: ExperimentFile = toml::from_str(&text).context("parsing experiment TOML")?;
    if spec.variants.is_empty() {
        bail!("no [[variant]] entries in {}", file.display());
    }
    if spec.require_preregistration {
        let file_sha = sha256_hex(text.as_bytes());
        let entries = ledger::read(ledger_path)?;
        for v in &spec.variants {
            let found = entries
                .iter()
                .any(|e| e.verdict == "preregistered" && e.variant == v.name && e.experiment_file_sha256 == file_sha);
            if !found {
                bail!("variant {} is not pre-registered from this exact file; run --preregister first", v.name);
            }
        }
    }

    let mut data_bytes = Vec::new();
    for p in &spec.data {
        data_bytes.extend(std::fs::read(p).with_context(|| format!("reading {}", p.display()))?);
    }
    let data_sha = sha256_hex(&data_bytes);
    let (events, key) = load_session(&spec.data, Some(spec.time_key))?;

    let mut evals = Vec::new();
    let mut baseline_holdout = None;
    for v in &spec.variants {
        let config = build_config(&v.base, &v.set).with_context(|| format!("variant {}", v.name))?;
        let (eval, _) = evaluate_keyed(&v.name, &events, config, key).await;
        let verdict = verdict(&eval, baseline_holdout);
        if baseline_holdout.is_none() {
            baseline_holdout = Some(eval.second_half.pnl_after_costs);
        }
        let mut result = serde_json::to_value(&eval)?;
        result["time_key"] = serde_json::to_value(key)?;
        let entry = ledger::append_record(
            ledger_path,
            NewRecord {
                variant: &v.name,
                hypothesis: &v.hypothesis,
                base: &v.base,
                overrides: serde_json::to_value(&v.set)?,
                experiment_file_sha256: sha256_hex(text.as_bytes()),
                data_files: spec.data.iter().map(|p| p.display().to_string()).collect(),
                data_sha256: data_sha.clone(),
                verdict,
                verdict_rule: VERDICT_RULE,
                result,
                recorded_at: format_utc(wall_now_ms()),
            },
        )?;
        println!("ledger #{} {} -> {} ({})", entry.seq, v.name, verdict, &entry.hash[..12]);
        evals.push(eval);
    }
    println!();
    print_table(&evals);
    let all = ledger::read(ledger_path)?;
    println!("\nledger {}: {} entries, chain verified", ledger_path.display(), ledger::verify(&all)?);
    Ok(evals)
}
