//! An append-only, hash-chained research ledger.
//!
//! Every experiment result is appended as one JSON line. Each entry stores
//! the SHA-256 of the previous entry and its own SHA-256 over all its other
//! fields, so editing or deleting a past result (a quietly dropped bad idea,
//! a rerun with friendlier data) breaks the chain and `verify` says where.
//! Killed ideas stay on the ledger next to kept ones; that is the point.
//!
//! The hash is over `serde_json::to_string` of the entry with `hash` empty.
//! Struct field order is fixed, so the serialisation is stable.

use crate::artifacts::append_line;
use crate::backtest::Evaluation;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub seq: u64,
    pub recorded_at: String,
    pub prev_hash: String,
    /// SHA-256 of this entry with `hash` set to "".
    pub hash: String,
    pub variant: String,
    pub hypothesis: String,
    pub base: String,
    /// The config overrides applied to the base, as written in the TOML.
    pub overrides: serde_json::Value,
    pub experiment_file_sha256: String,
    pub data_files: Vec<String>,
    /// SHA-256 over the input data files, in order: which data this ran on.
    pub data_sha256: String,
    pub verdict: String,
    pub verdict_rule: String,
    pub result: serde_json::Value,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The hash an entry should carry.
pub fn entry_hash(entry: &LedgerEntry) -> Result<String> {
    let mut unhashed = entry.clone();
    unhashed.hash = String::new();
    Ok(sha256_hex(serde_json::to_string(&unhashed)?.as_bytes()))
}

pub fn read(path: &Path) -> Result<Vec<LedgerEntry>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let text = std::fs::read_to_string(path)?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("ledger line {}", i + 1)))
        .collect()
}

/// Check every link and every hash. Returns the number of entries.
pub fn verify(entries: &[LedgerEntry]) -> Result<usize> {
    let mut prev = GENESIS.to_string();
    for (i, e) in entries.iter().enumerate() {
        if e.seq != i as u64 {
            bail!("entry {i}: seq is {}, expected {i}", e.seq);
        }
        if e.prev_hash != prev {
            bail!("entry {i} ({}): prev_hash does not match the entry before it", e.variant);
        }
        if entry_hash(e)? != e.hash {
            bail!("entry {i} ({}): contents do not match its hash; it was edited", e.variant);
        }
        prev = e.hash.clone();
    }
    Ok(entries.len())
}

/// Fields of a new entry, before it is linked into the chain.
pub struct NewEntry<'a> {
    pub variant: &'a str,
    pub hypothesis: &'a str,
    pub base: &'a str,
    pub overrides: serde_json::Value,
    pub experiment_file_sha256: String,
    pub data_files: Vec<String>,
    pub data_sha256: String,
    pub verdict: &'a str,
    pub verdict_rule: &'a str,
    pub eval: &'a Evaluation,
    pub recorded_at: String,
}

/// Verify the existing chain, then link and append one entry.
pub fn append(path: &Path, new: NewEntry) -> Result<LedgerEntry> {
    let existing = read(path)?;
    verify(&existing).context("refusing to append to a broken ledger")?;
    let mut entry = LedgerEntry {
        seq: existing.len() as u64,
        recorded_at: new.recorded_at,
        prev_hash: existing.last().map_or(GENESIS.to_string(), |e| e.hash.clone()),
        hash: String::new(),
        variant: new.variant.to_string(),
        hypothesis: new.hypothesis.to_string(),
        base: new.base.to_string(),
        overrides: new.overrides,
        experiment_file_sha256: new.experiment_file_sha256,
        data_files: new.data_files,
        data_sha256: new.data_sha256,
        verdict: new.verdict.to_string(),
        verdict_rule: new.verdict_rule.to_string(),
        result: serde_json::to_value(new.eval)?,
    };
    entry.hash = entry_hash(&entry)?;
    append_line(path, &serde_json::to_string(&entry)?)?;
    Ok(entry)
}
