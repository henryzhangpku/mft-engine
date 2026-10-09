//! Prediction-market features: from a strike ladder (Kalshi hourly, or
//! Polymarket daily) to an implied distribution, and the gates strategies v2
//! and v2b use.
//!
//! A ladder of "above strike K at close" contracts gives, after cleaning, a
//! non-increasing function `P(close > K)`. From it:
//! * `prob_above(spot)` is the market's probability that the price at the
//!   close is above where it is now: the implied direction;
//! * `quantile(q)` is the price the market thinks has probability q of not
//!   being exceeded; q = 0.5 is the implied median.
//!
//! Between strikes we interpolate linearly. Outside the ladder we return
//! `None`: we do not extrapolate a distribution we were not shown.
//!
//! The gate (`PredictionState::caution_for`) is a `Caution`, the same [0, 1]
//! multiplier the text rule uses, so it can only remove exposure. It fails
//! closed: when enabled, a missing, stale or unreadable ladder means flat.
//! Ladders are kept per venue and coin, so a Polymarket snapshot never
//! replaces a Kalshi one: v2 reads Kalshi only; v2b reads both and needs both
//! to agree.

use crate::event::PredictionMarket;
use crate::text::Caution;
use std::collections::BTreeMap;

/// Turn raw (strike, probability) pairs into a clean ladder: sorted by
/// strike, clamped to [0, 1], non-finite points dropped, and forced to be
/// non-increasing. Quotes are noisy and a higher strike can briefly be priced
/// above a lower one; a probability of "above K" cannot rise with K, so we
/// take the running minimum from the low strikes up.
pub fn clean_ladder(mut raw: Vec<(f64, f64)>) -> (Vec<f64>, Vec<f64>) {
    raw.retain(|(k, p)| k.is_finite() && p.is_finite());
    raw.sort_by(|a, b| a.0.total_cmp(&b.0));
    raw.dedup_by(|a, b| a.0 == b.0);
    let mut strikes = Vec::with_capacity(raw.len());
    let mut probs = Vec::with_capacity(raw.len());
    let mut running = 1.0_f64;
    for (k, p) in raw {
        running = running.min(p.clamp(0.0, 1.0));
        strikes.push(k);
        probs.push(running);
    }
    (strikes, probs)
}

/// Build a `PredictionMarket` from raw (strike, probability) points, keeping
/// only the informative part of the ladder: from the last strike the market
/// prices at 97% or more up to the first it prices at 3% or less. Fewer than
/// three usable strikes is not a distribution, so `None`. Shared by the
/// Kalshi and Polymarket readers, so both venues are cleaned the same way.
pub fn ladder_snapshot(
    venue: &str,
    coin: &str,
    event: &str,
    ts: i64,
    close_ts: i64,
    raw: Vec<(f64, f64)>,
) -> Option<PredictionMarket> {
    let (strikes, probs) = clean_ladder(raw);
    let lo = probs.iter().rposition(|p| *p >= 0.97).unwrap_or(0);
    let hi = probs.iter().position(|p| *p <= 0.03).unwrap_or(probs.len().saturating_sub(1));
    if probs.is_empty() || hi < lo || hi - lo + 1 < 3 {
        return None;
    }
    Some(PredictionMarket {
        coin: coin.to_string(),
        ts,
        venue: venue.to_string(),
        event: event.to_string(),
        close_ts,
        strikes: strikes[lo..=hi].to_vec(),
        prob_above: probs[lo..=hi].iter().map(|p| (p * 1000.0).round() / 1000.0).collect(),
    })
}

/// P(close > x) by linear interpolation between strikes. `None` outside the
/// ladder or for a malformed ladder.
pub fn prob_above(strikes: &[f64], probs: &[f64], x: f64) -> Option<f64> {
    if strikes.len() < 2 || strikes.len() != probs.len() || !x.is_finite() {
        return None;
    }
    if x < strikes[0] || x > strikes[strikes.len() - 1] {
        return None;
    }
    for i in 1..strikes.len() {
        let (k0, k1) = (strikes[i - 1], strikes[i]);
        if x <= k1 {
            let w = if k1 > k0 { (x - k0) / (k1 - k0) } else { 0.0 };
            return Some(probs[i - 1] + w * (probs[i] - probs[i - 1]));
        }
    }
    None
}

/// The price with probability `q` of not being exceeded at the close, i.e.
/// where P(close > K) = 1 - q. `None` if the ladder does not span that level.
pub fn quantile(strikes: &[f64], probs: &[f64], q: f64) -> Option<f64> {
    if strikes.len() < 2 || strikes.len() != probs.len() {
        return None;
    }
    let target = 1.0 - q;
    for i in 1..strikes.len() {
        let (p0, p1) = (probs[i - 1], probs[i]);
        if p0 >= target && p1 <= target {
            if p0 == p1 {
                return Some(strikes[i - 1]);
            }
            let w = (p0 - target) / (p0 - p1);
            return Some(strikes[i - 1] + w * (strikes[i] - strikes[i - 1]));
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PredictionParams {
    /// Strategy v2 switch. Off in v1.
    pub enabled: bool,
    /// A ladder older than this is stale and closes the gate.
    pub max_age_ms: i64,
    /// Apply the gate only when a position would be opened or flipped, not
    /// while one is held. Gating every bar made the target flicker between
    /// full size and flat as P(up) wobbled around 0.5, which churned fees
    /// (found on the first v2 run; that variant is kept on the ledger).
    pub gate_entries_only: bool,
    /// The market must give more than this probability to our direction:
    /// P(close > spot) for a long, P(close < spot) for a short.
    pub min_agree: f64,
    /// Strategy v2b: a new entry also needs Polymarket's daily ladder to
    /// lean our way (same `min_agree`, same entries-only rule). Off in v1
    /// and v2. A missing, stale or out-of-range Polymarket ladder means no
    /// entry.
    #[serde(default)]
    pub require_polymarket: bool,
}

impl Default for PredictionParams {
    fn default() -> Self {
        Self {
            enabled: false,
            gate_entries_only: true,
            max_age_ms: 180_000,
            min_agree: 0.5,
            require_polymarket: false,
        }
    }
}

/// What the gate saw, for logs and the demo.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PmView {
    pub p_up: Option<f64>,
    pub median: Option<f64>,
}

#[derive(Debug, Default, Clone)]
pub struct PredictionState {
    params: PredictionParams,
    /// Latest ladder per (venue, coin).
    latest: BTreeMap<(String, String), PredictionMarket>,
}

/// The venue v2's gate reads, and the one v2b adds.
pub const KALSHI: &str = "kalshi";
pub const POLYMARKET: &str = "polymarket";

impl PredictionState {
    pub fn new(params: PredictionParams) -> Self {
        Self {
            params,
            latest: BTreeMap::new(),
        }
    }

    pub fn on_snapshot(&mut self, pm: &PredictionMarket) {
        self.latest.insert((pm.venue.clone(), pm.coin.clone()), pm.clone());
    }

    /// The latest Kalshi ladder's view at `spot`, if there is a usable one.
    pub fn view(&self, coin: &str, spot: f64, now_ms: i64) -> PmView {
        self.venue_view(KALSHI, coin, spot, now_ms)
    }

    /// The latest ladder of `venue` at `spot`, if there is a usable one.
    pub fn venue_view(&self, venue: &str, coin: &str, spot: f64, now_ms: i64) -> PmView {
        match self.fresh(venue, coin, now_ms) {
            Some(pm) => PmView {
                p_up: prob_above(&pm.strikes, &pm.prob_above, spot),
                median: quantile(&pm.strikes, &pm.prob_above, 0.5),
            },
            None => PmView { p_up: None, median: None },
        }
    }

    fn fresh(&self, venue: &str, coin: &str, now_ms: i64) -> Option<&PredictionMarket> {
        let pm = self.latest.get(&(venue.to_string(), coin.to_string()))?;
        let age = now_ms - pm.ts;
        let usable = (0..=self.params.max_age_ms).contains(&age) && pm.close_ts > now_ms;
        usable.then_some(pm)
    }

    /// The v2 gate (and v2b's, with `require_polymarket`): keep the target
    /// only if the market, or both markets, lean our way.
    /// `position_notional` is the current position, signed, in USD.
    pub fn caution_for(&self, coin: &str, target_notional: f64, position_notional: f64, spot: f64, now_ms: i64) -> Caution {
        if !self.params.enabled || target_notional == 0.0 {
            return Caution::NONE;
        }
        let holding_same_side = position_notional * target_notional > 0.0;
        if self.params.gate_entries_only && holding_same_side {
            return Caution::NONE;
        }
        let venues: &[&str] = if self.params.require_polymarket { &[KALSHI, POLYMARKET] } else { &[KALSHI] };
        for venue in venues {
            let Some(p_up) = self.venue_view(venue, coin, spot, now_ms).p_up else {
                return Caution::new(0.0); // required input missing: no trade
            };
            let agree = if target_notional > 0.0 { p_up } else { 1.0 - p_up };
            if agree <= self.params.min_agree {
                return Caution::new(0.0);
            }
        }
        Caution::NONE
    }
}
