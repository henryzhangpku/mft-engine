//! The signal: volatility-normalised short-horizon momentum on 1-minute bars.
//!
//! Fixed in advance, before any backtest was run, and not tuned afterwards:
//!
//! * `r5`    = ln(close_t / close_{t-5}), the 5-minute log return.
//! * `sigma` = standard deviation of the last 60 one-minute log returns.
//! * `z`     = r5 / (sigma * sqrt(5)), the 5-minute move in units of its own
//!   recent noise.
//! * Flat: go long when z >= 2.0, short when z <= -2.0.
//! * In a position: exit when z changes sign against us, or after 15 bars.
//!   Flip directly if z crosses the entry threshold the other way.
//! * Size: a fixed target notional (default $1,000) per coin.
//!
//! The strategy outputs a *target* notional, not an order. The engine turns
//! the difference between target and actual position into an order. If risk
//! blocks that order, the next bar simply asks for the same target again;
//! nothing in the strategy assumes its last wish was granted.
//!
//! Gap handling: if bars are not contiguous minutes, or a `Gap` event arrives,
//! the price history is discarded and the target is flat until the window has
//! refilled. A return computed across a hole is not a 5-minute return.

use crate::event::Bar;
use std::collections::{BTreeMap, VecDeque};

const BAR_MS: i64 = 60_000;

/// One hour, the bar length of strategy v5.
pub const HOUR_MS: i64 = 3_600_000;

fn default_bar_ms() -> i64 {
    BAR_MS
}

fn is_minute(ms: &i64) -> bool {
    *ms == BAR_MS
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MomentumParams {
    pub lookback_bars: usize,
    pub vol_window_bars: usize,
    pub entry_z: f64,
    pub max_hold_bars: u32,
    pub target_notional: f64,
    /// Bar length the strategy expects, in ms; a bar that does not follow
    /// the previous one by exactly this much is a gap. One minute unless set
    /// (and then left out of the serialised config, so every config already
    /// on the ledger serialises, and hashes, exactly as before).
    #[serde(default = "default_bar_ms", skip_serializing_if = "is_minute")]
    pub bar_ms: i64,
}

impl Default for MomentumParams {
    fn default() -> Self {
        Self {
            lookback_bars: 5,
            vol_window_bars: 60,
            entry_z: 2.0,
            max_hold_bars: 15,
            target_notional: 1_000.0,
            bar_ms: BAR_MS,
        }
    }
}

impl MomentumParams {
    /// Strategy v5's signal: the same rules on 1-hour bars at a 24-hour
    /// horizon. r24 = ln(close_t / close_{t-24}), sigma = std of the last 168
    /// hourly log returns, z = r24 / (sigma * sqrt(24)); enter at |z| >= 1,
    /// exit when z crosses 0 against the position or after 72 bars.
    pub fn v5_hourly() -> Self {
        Self {
            lookback_bars: 24,
            vol_window_bars: 168,
            entry_z: 1.0,
            max_hold_bars: 72,
            target_notional: 1_000.0,
            bar_ms: HOUR_MS,
        }
    }
}

/// Per-coin memory. Kept small on purpose: recent closes and the current
/// intended direction.
#[derive(Debug, Default, Clone)]
struct CoinState {
    closes: VecDeque<f64>,
    last_open_ts: Option<i64>,
    /// -1 short, 0 flat, +1 long.
    direction: i8,
    bars_held: u32,
    /// The z-score from the latest bar, kept only so logs can explain a decision.
    last_z: Option<f64>,
}

impl CoinState {
    fn reset(&mut self) {
        *self = CoinState::default();
    }
}

#[derive(Debug, Clone)]
pub struct Momentum {
    params: MomentumParams,
    // BTreeMap rather than HashMap: iteration order is fixed, so nothing that
    // ever walks this map can make two replays differ.
    coins: BTreeMap<String, CoinState>,
}

impl Momentum {
    pub fn new(params: MomentumParams) -> Self {
        Self {
            params,
            coins: BTreeMap::new(),
        }
    }

    pub fn params(&self) -> &MomentumParams {
        &self.params
    }

    /// The z-score computed on the latest bar for `coin`, if there was one.
    pub fn last_z(&self, coin: &str) -> Option<f64> {
        self.coins.get(coin).and_then(|s| s.last_z)
    }

    /// Forget all history for a coin. Called on a `Gap` event.
    pub fn on_gap(&mut self, coin: &str) {
        if let Some(state) = self.coins.get_mut(coin) {
            state.reset();
        }
    }

    /// Feed one completed bar; returns the target notional for this coin
    /// (positive long, negative short, zero flat).
    pub fn on_bar(&mut self, bar: &Bar) -> f64 {
        let p = self.params;
        let state = self.coins.entry(bar.coin.clone()).or_default();

        // A missing minute is a gap even if nobody sent a Gap event.
        if let Some(prev) = state.last_open_ts {
            if bar.open_ts != prev + p.bar_ms {
                state.reset();
            }
        }
        state.last_open_ts = Some(bar.open_ts);

        state.closes.push_back(bar.close);
        // We need vol_window returns, which takes vol_window + 1 closes.
        while state.closes.len() > p.vol_window_bars + 1 {
            state.closes.pop_front();
        }

        state.last_z = zscore(&state.closes, p.lookback_bars, p.vol_window_bars);
        let z = match state.last_z {
            Some(z) => z,
            None => {
                // Warming up, or bad data: no opinion means flat.
                state.direction = 0;
                state.bars_held = 0;
                return 0.0;
            }
        };

        next_direction(state, z, &p);
        f64::from(state.direction) * p.target_notional
    }
}

/// The position state machine, separated out so it reads as the rules above.
fn next_direction(state: &mut CoinState, z: f64, p: &MomentumParams) {
    let dir = f64::from(state.direction);
    if state.direction == 0 {
        if z >= p.entry_z {
            state.direction = 1;
            state.bars_held = 0;
        } else if z <= -p.entry_z {
            state.direction = -1;
            state.bars_held = 0;
        }
        return;
    }

    state.bars_held += 1;
    if z * dir <= -p.entry_z {
        // Strong move the other way: flip.
        state.direction = -state.direction;
        state.bars_held = 0;
    } else if z * dir < 0.0 || state.bars_held >= p.max_hold_bars {
        state.direction = 0;
        state.bars_held = 0;
    }
}

/// z-score of the `lookback`-bar log return against the volatility of the
/// last `vol_window` one-bar log returns. `None` if there is not enough
/// history or the inputs are not usable (non-positive prices, zero variance).
pub fn zscore(closes: &VecDeque<f64>, lookback: usize, vol_window: usize) -> Option<f64> {
    if lookback == 0 || vol_window < 2 || closes.len() < vol_window + 1 || lookback > vol_window {
        return None;
    }
    if closes.iter().any(|c| !c.is_finite() || *c <= 0.0) {
        return None;
    }
    let n = closes.len();
    let returns: Vec<f64> = (n - vol_window..n)
        .map(|i| (closes[i] / closes[i - 1]).ln())
        .collect();
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (returns.len() - 1) as f64;
    let sigma = var.sqrt();
    if sigma <= 0.0 || !sigma.is_finite() {
        return None;
    }
    let r = (closes[n - 1] / closes[n - 1 - lookback]).ln();
    Some(r / (sigma * (lookback as f64).sqrt()))
}
