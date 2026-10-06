//! The experiment ledger's hash chain, config overrides, and the rule that no
//! secret is ever written to an output file.

use mft_engine::artifacts::{find_secret, write_text};
use mft_engine::backtest::{evaluate, load_events};
use mft_engine::engine::EngineConfig;
use mft_engine::experiment::build_config;
use mft_engine::ledger::{self, NewEntry};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("mft-engine-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[tokio::test]
async fn ledger_chain_verifies_and_detects_tampering() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let events = load_events(&[root.join("data/bars_1m.jsonl")]).unwrap();
    let events = &events[..3_000];
    let path = scratch("ledger.jsonl");
    for (name, config) in [("a", EngineConfig::v1()), ("b", EngineConfig::v2()), ("c", EngineConfig::v1())] {
        let (eval, _) = evaluate(name, events, config).await;
        ledger::append(
            &path,
            NewEntry {
                variant: name,
                hypothesis: "test",
                base: "v1",
                overrides: serde_json::json!({}),
                experiment_file_sha256: "x".into(),
                data_files: vec![],
                data_sha256: "y".into(),
                verdict: "killed",
                verdict_rule: "test",
                eval: &eval,
                recorded_at: "now".into(),
            },
        )
        .unwrap();
    }
    let entries = ledger::read(&path).unwrap();
    assert_eq!(ledger::verify(&entries).unwrap(), 3);
    assert_eq!(entries[1].prev_hash, entries[0].hash);

    // Rewrite history: flip a verdict.
    let mut edited = entries.clone();
    edited[1].verdict = "kept".into();
    assert!(ledger::verify(&edited).unwrap_err().to_string().contains("edited"));
    // Quietly drop a bad idea.
    let mut dropped = entries.clone();
    dropped.remove(1);
    assert!(ledger::verify(&dropped).is_err());
    // Recompute the edited entry's own hash: the next link still breaks.
    let mut rehashed = entries.clone();
    rehashed[1].verdict = "kept".into();
    rehashed[1].hash = ledger::entry_hash(&rehashed[1]).unwrap();
    assert!(ledger::verify(&rehashed).unwrap_err().to_string().contains("prev_hash"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn config_overrides_apply_and_reject_mistakes() {
    let mut set = BTreeMap::new();
    set.insert("strategy.entry_z".to_string(), toml::Value::Float(3.0));
    set.insert("prediction.enabled".to_string(), toml::Value::Boolean(false));
    let c = build_config("v2", &set).unwrap();
    assert_eq!(c.strategy.entry_z, 3.0);
    assert!(!c.prediction.enabled);
    assert!(c.text.enabled, "untouched fields keep the base's value");

    let mut typo = BTreeMap::new();
    typo.insert("strategy.entry_zz".to_string(), toml::Value::Float(3.0));
    assert!(build_config("v1", &typo).is_err(), "unknown field");
    let mut wrong = BTreeMap::new();
    wrong.insert("prediction.enabled".to_string(), toml::Value::String("yes".into()));
    assert!(build_config("v1", &wrong).is_err(), "wrong type");
    assert!(build_config("v3", &BTreeMap::new()).is_err(), "unknown base");
}

#[test]
fn secret_detector_catches_tokens_but_not_hashes() {
    let fake_openai = format!("key={}{}", "sk-", "abcdefghijklmnopqrstuvwx");
    assert!(find_secret(&fake_openai).is_some());
    assert!(find_secret(&format!("Authorization: {}{}", "Bearer ", "a".repeat(40))).is_some());
    assert!(find_secret(&format!("{}{}", "AKIA", "ABCDEFGHIJKLMNOP")).is_some());
    assert!(find_secret(&format!("-----BEGIN RSA {}-----", "PRIVATE KEY")).is_some());
    // Things that legitimately appear in outputs.
    let sha = "dc437e468287f0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f6071829";
    assert!(find_secret(sha).is_none());
    assert!(find_secret("task-runner and desk-level risk").is_none());
    assert!(find_secret(r#"{"type":"Bar","coin":"BTC"}"#).is_none());
}

#[test]
fn write_text_refuses_a_secret() {
    let path = scratch("secret.txt");
    let bad = format!("leaked {}{}", "ghp_", "x".repeat(36));
    assert!(write_text(&path, &bad).is_err());
    assert!(!path.exists(), "nothing written");
    write_text(&path, "fine").unwrap();
    let _ = std::fs::remove_file(&path);
}

fn scan(dir: &Path, found: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan(&path, found);
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(kind) = find_secret(&text) {
                found.push(format!("{}: {kind}", path.display()));
            }
        }
    }
}

#[test]
fn no_secret_in_any_committed_output() {
    // data/, results/, docs/ and experiments/ hold everything the programs
    // write. Also checks for the live values of TYPESAFE_API_KEY etc. when
    // they are set in this shell. (Source files are not scanned: the guard's
    // own patterns would match themselves.)
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut found = Vec::new();
    for dir in ["data", "results", "docs", "experiments"] {
        scan(&root.join(dir), &mut found);
    }
    assert!(found.is_empty(), "secret-looking content: {found:?}");
}
