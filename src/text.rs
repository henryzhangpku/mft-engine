//! The text signal, and the rule that it may only make the engine more
//! cautious.
//!
//! A scored post (see `sidecar/`) says how likely it is to be about a coin and
//! how bullish or bearish it reads. We do not let that open or enlarge a
//! position. An LLM-style judgement on a social post is exactly the kind of
//! input that is occasionally confidently wrong, so it gets a brake pedal and
//! no accelerator.
//!
//! The rule is enforced twice:
//! 1. By construction: `Caution` is a multiplier that can only hold a value in
//!    [0, 1], and the only thing you can do with it is scale a target.
//! 2. By a check in the engine (`check_not_riskier`) that compares the target
//!    before and after. If that check ever fails, the order is blocked.

use crate::event::TextSignal;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TextParams {
    /// How long a scored post stays in force after it becomes available.
    pub ttl_ms: i64,
    /// Posts less relevant than this are ignored.
    pub min_relevance: f64,
    /// If relevance-weighted opposing probability reaches this, veto (go flat).
    pub veto_at: f64,
}

impl Default for TextParams {
    fn default() -> Self {
        Self {
            ttl_ms: 30 * 60_000,
            min_relevance: 0.5,
            veto_at: 0.6,
        }
    }
}

/// A multiplier in [0, 1] applied to a target position.
///
/// The field is private, so the only way to make one is `Caution::new`, which
/// clamps, or `Caution::NONE`. Code elsewhere cannot construct a 1.5.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Caution(f64);

impl Caution {
    /// No adjustment.
    pub const NONE: Caution = Caution(1.0);

    /// Clamp into [0, 1]. NaN becomes 0 (flat): if we cannot read the number
    /// we take the most cautious reading of it.
    pub fn new(multiplier: f64) -> Self {
        if multiplier.is_nan() {
            Caution(0.0)
        } else {
            Caution(multiplier.clamp(0.0, 1.0))
        }
    }

    pub fn value(self) -> f64 {
        self.0
    }

    /// Scale a target. With a multiplier in [0, 1] the result has the same
    /// sign as the input (or is zero) and is never larger in magnitude.
    pub fn apply(self, target_notional: f64) -> f64 {
        target_notional * self.0
    }
}

/// Returns an error if `after` is riskier than `before`: larger in magnitude,
/// or on the other side of zero. Used by the engine to block the order.
pub fn check_not_riskier(before: f64, after: f64) -> Result<(), String> {
    if !before.is_finite() || !after.is_finite() {
        return Err(format!("non-finite target (before {before}, after {after})"));
    }
    if after.abs() > before.abs() + 1e-9 {
        return Err(format!("text rule would enlarge target from {before} to {after}"));
    }
    if after != 0.0 && after.signum() != before.signum() {
        return Err(format!("text rule would flip target from {before} to {after}"));
    }
    Ok(())
}

/// The latest scored post per coin.
#[derive(Debug, Default, Clone)]
pub struct TextState {
    params: TextParams,
    latest: BTreeMap<String, TextSignal>,
}

impl TextState {
    pub fn new(params: TextParams) -> Self {
        Self {
            params,
            latest: BTreeMap::new(),
        }
    }

    pub fn on_signal(&mut self, signal: &TextSignal) {
        self.latest.insert(signal.coin.clone(), signal.clone());
    }

    /// How much to scale `target_notional` for `coin` at time `now_ms`.
    ///
    /// Only the probability that *opposes* the intended direction matters. A
    /// bullish post never adds to a long; a bearish post can shrink or veto it.
    pub fn caution_for(&self, coin: &str, target_notional: f64, now_ms: i64) -> Caution {
        if target_notional == 0.0 {
            return Caution::NONE;
        }
        let Some(sig) = self.latest.get(coin) else {
            return Caution::NONE;
        };
        let age = now_ms - sig.ts;
        if age < 0 || age > self.params.ttl_ms || sig.relevance < self.params.min_relevance {
            return Caution::NONE;
        }
        let opposing = if target_notional > 0.0 { sig.bearish } else { sig.bullish };
        let strength = opposing * sig.relevance;
        if strength >= self.params.veto_at {
            Caution::new(0.0)
        } else {
            Caution::new(1.0 - strength)
        }
    }
}
