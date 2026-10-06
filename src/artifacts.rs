//! Every file the engine writes goes through here, and nothing that looks
//! like a credential is ever written.
//!
//! The engine holds no keys, so a secret could only reach an output file by
//! accident: an environment variable echoed into a log, a header captured in
//! an error message. `write_text` checks the whole payload before touching
//! the disk and refuses (an error, nothing written) if it finds the value of
//! a known secret environment variable or a string shaped like a well-known
//! token. The error names the kind of secret, never the value. The Python
//! sidecar applies the same rule (`sidecar/secrets_guard.py`), and a test
//! scans `data/`, `results/` and `docs/` for the same patterns.

use anyhow::{bail, Result};
use serde::Serialize;
use std::path::Path;

/// Environment variables whose values must never appear in an output file.
pub const SECRET_ENV_VARS: &[&str] = &[
    "TYPESAFE_API_KEY",
    "LAYA_API_KEY",
    "IMPOSSIBL_API_KEY",
    "REDDIT_CLIENT_ID",
    "REDDIT_CLIENT_SECRET",
];

/// Token prefixes and the minimum run of token characters that must follow
/// for a match. Written as data so the list reads as a table.
const PREFIXES: &[(&str, usize, &str)] = &[
    ("sk-", 16, "openai-style key"),
    ("sk_live_", 16, "stripe-style key"),
    ("sk_test_", 16, "stripe-style key"),
    ("ghp_", 30, "github token"),
    ("gho_", 30, "github token"),
    ("xoxb-", 10, "slack token"),
    ("xoxp-", 10, "slack token"),
    ("AKIA", 16, "aws access key id"),
    ("Bearer ", 20, "bearer token"),
    ("bearer ", 20, "bearer token"),
];

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '/')
}

/// The kind of secret found in `text`, or `None`.
pub fn find_secret(text: &str) -> Option<String> {
    for name in SECRET_ENV_VARS {
        if let Ok(value) = std::env::var(name) {
            if value.len() >= 8 && text.contains(&value) {
                return Some(format!("value of ${name}"));
            }
        }
    }
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return Some("private key block".into());
    }
    for (prefix, min_len, kind) in PREFIXES {
        for (i, _) in text.match_indices(prefix) {
            // Require a word boundary before the prefix, so "task-" or a hex
            // hash containing "AKIA" mid-word does not count.
            let boundary = text[..i].chars().next_back().map_or(true, |c| !c.is_ascii_alphanumeric());
            let run = text[i + prefix.len()..].chars().take_while(|c| is_token_char(*c)).count();
            if boundary && run >= *min_len {
                return Some((*kind).to_string());
            }
        }
    }
    None
}

/// Write `text` to `path`, creating parent directories, unless it contains
/// something that looks like a secret.
pub fn write_text(path: &Path, text: &str) -> Result<()> {
    if let Some(kind) = find_secret(text) {
        bail!("refusing to write {}: it contains a {kind}", path.display());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_text(path, &(serde_json::to_string_pretty(value)? + "\n"))
}

/// One JSON object per line.
pub fn write_jsonl<T: Serialize>(path: &Path, items: &[T]) -> Result<()> {
    let mut text = String::new();
    for item in items {
        text.push_str(&serde_json::to_string(item)?);
        text.push('\n');
    }
    write_text(path, &text)
}

/// Append one line to `path` (creating it), with the same secret check.
pub fn append_line(path: &Path, line: &str) -> Result<()> {
    use std::io::Write;
    if let Some(kind) = find_secret(line) {
        bail!("refusing to append to {}: it contains a {kind}", path.display());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")?;
    Ok(())
}
