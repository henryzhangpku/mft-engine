//! Pre-trade risk. Every order passes through `RiskEngine::check`, and the
//! check fails closed:
//!
//! * each rule returns `Ok(Allow)`, `Ok(Block(reason))` or `Err(error)`;
//! * a `Block` stops the order with that reason;
//! * an `Err` also stops the order. A rule that cannot decide is treated as a
//!   rule that said no. This is the property an interviewer should poke at:
//!   there is no branch where an error lets an order through;
//! * an engine with no rules blocks everything, so a misconfiguration cannot
//!   silently mean "no limits".
//!
//! The rules are trait objects (`Box<dyn RiskRule>`) so a test can insert a
//! rule that deliberately errors and prove the order is blocked.

use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RiskLimits {
    /// Largest absolute position per coin, in USD at the reference price.
    pub max_position_notional: f64,
    /// Largest single order, in USD.
    pub max_order_notional: f64,
    /// Once the day's PnL (UTC day, fees included) is at or below minus this,
    /// only orders that reduce exposure are allowed.
    pub max_daily_loss: f64,
    /// No market data for this long blocks all orders for that coin.
    pub stale_after_ms: i64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_position_notional: 1_500.0,
            max_order_notional: 2_500.0,
            max_daily_loss: 50.0,
            stale_after_ms: 90_000,
        }
    }
}

/// What the engine wants to do. `qty` is signed: positive buys, negative sells.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderIntent {
    pub coin: String,
    pub qty: f64,
    /// The price the order would be valued at (touch or last close).
    pub ref_px: f64,
}

impl OrderIntent {
    pub fn notional(&self) -> f64 {
        (self.qty * self.ref_px).abs()
    }
}

/// Everything a rule may look at. Borrowed, read-only: a rule cannot change
/// engine state, only give a verdict.
#[derive(Debug, Clone)]
pub struct RiskContext {
    pub now_ms: i64,
    /// Current position in coin units (signed).
    pub position_qty: f64,
    /// Engine-clock time of the last market data event for this coin.
    pub last_data_ms: Option<i64>,
    /// PnL since the start of the current UTC day, after fees.
    pub daily_pnl: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Allow,
    Block(String),
}

pub trait RiskRule {
    fn name(&self) -> &'static str;
    fn check(&self, order: &OrderIntent, ctx: &RiskContext) -> Result<Verdict>;
}

/// True if the order makes the absolute position smaller (and does not
/// overshoot through zero into a larger opposite position).
fn reduces_exposure(order: &OrderIntent, ctx: &RiskContext) -> bool {
    let after = ctx.position_qty + order.qty;
    after.abs() < ctx.position_qty.abs() && after * ctx.position_qty >= 0.0
}

/// Rejects NaN, infinite, zero or negative inputs before any other rule does
/// arithmetic on them. This is an `Err`, not a `Block`: bad numbers mean the
/// engine itself is confused.
pub struct SaneInputs;

impl RiskRule for SaneInputs {
    fn name(&self) -> &'static str {
        "sane_inputs"
    }
    fn check(&self, order: &OrderIntent, ctx: &RiskContext) -> Result<Verdict> {
        if !order.qty.is_finite() || order.qty == 0.0 {
            bail!("order quantity {} is not a finite non-zero number", order.qty);
        }
        if !order.ref_px.is_finite() || order.ref_px <= 0.0 {
            bail!("reference price {} is not a finite positive number", order.ref_px);
        }
        if !ctx.position_qty.is_finite() || !ctx.daily_pnl.is_finite() {
            bail!("position or PnL is not finite");
        }
        Ok(Verdict::Allow)
    }
}

pub struct StaleData {
    pub stale_after_ms: i64,
}

impl RiskRule for StaleData {
    fn name(&self) -> &'static str {
        "stale_data"
    }
    fn check(&self, _order: &OrderIntent, ctx: &RiskContext) -> Result<Verdict> {
        let Some(last) = ctx.last_data_ms else {
            return Ok(Verdict::Block("no market data received yet".into()));
        };
        let age = ctx.now_ms - last;
        if age < 0 {
            bail!("last data time {last} is after the clock {}", ctx.now_ms);
        }
        if age > self.stale_after_ms {
            return Ok(Verdict::Block(format!(
                "market data is {age} ms old, limit {} ms",
                self.stale_after_ms
            )));
        }
        Ok(Verdict::Allow)
    }
}

pub struct MaxOrderNotional {
    pub limit: f64,
}

impl RiskRule for MaxOrderNotional {
    fn name(&self) -> &'static str {
        "max_order_notional"
    }
    fn check(&self, order: &OrderIntent, _ctx: &RiskContext) -> Result<Verdict> {
        let n = order.notional();
        if n > self.limit {
            return Ok(Verdict::Block(format!("order notional {n:.2} > limit {:.2}", self.limit)));
        }
        Ok(Verdict::Allow)
    }
}

pub struct MaxPosition {
    pub limit: f64,
}

impl RiskRule for MaxPosition {
    fn name(&self) -> &'static str {
        "max_position"
    }
    fn check(&self, order: &OrderIntent, ctx: &RiskContext) -> Result<Verdict> {
        if reduces_exposure(order, ctx) {
            return Ok(Verdict::Allow);
        }
        let after = ((ctx.position_qty + order.qty) * order.ref_px).abs();
        if after > self.limit {
            return Ok(Verdict::Block(format!(
                "position after fill {after:.2} > limit {:.2}",
                self.limit
            )));
        }
        Ok(Verdict::Allow)
    }
}

pub struct MaxDailyLoss {
    pub limit: f64,
}

impl RiskRule for MaxDailyLoss {
    fn name(&self) -> &'static str {
        "max_daily_loss"
    }
    fn check(&self, order: &OrderIntent, ctx: &RiskContext) -> Result<Verdict> {
        // Once the loss limit is hit we still let the engine get out, never in.
        if ctx.daily_pnl <= -self.limit && !reduces_exposure(order, ctx) {
            return Ok(Verdict::Block(format!(
                "daily PnL {:.2} at or below -{:.2}; only reducing orders allowed",
                ctx.daily_pnl, self.limit
            )));
        }
        Ok(Verdict::Allow)
    }
}

pub struct RiskEngine {
    rules: Vec<Box<dyn RiskRule>>,
}

impl RiskEngine {
    /// The standard rule set. `SaneInputs` runs first so later rules can
    /// assume finite numbers (they still would not let an error through).
    pub fn new(limits: RiskLimits) -> Self {
        Self {
            rules: vec![
                Box::new(SaneInputs),
                Box::new(StaleData { stale_after_ms: limits.stale_after_ms }),
                Box::new(MaxOrderNotional { limit: limits.max_order_notional }),
                Box::new(MaxPosition { limit: limits.max_position_notional }),
                Box::new(MaxDailyLoss { limit: limits.max_daily_loss }),
            ],
        }
    }

    /// Build with an explicit rule list. Used by tests.
    pub fn with_rules(rules: Vec<Box<dyn RiskRule>>) -> Self {
        Self { rules }
    }

    /// `Ok(())` means every rule allowed the order. `Err(reason)` means it is
    /// blocked; the reason names the rule.
    pub fn check(&self, order: &OrderIntent, ctx: &RiskContext) -> Result<(), String> {
        if self.rules.is_empty() {
            return Err("no risk rules configured".into());
        }
        for rule in &self.rules {
            match rule.check(order, ctx) {
                Ok(Verdict::Allow) => continue,
                Ok(Verdict::Block(reason)) => return Err(format!("{}: {reason}", rule.name())),
                Err(error) => return Err(format!("{}: check errored: {error}", rule.name())),
            }
        }
        Ok(())
    }
}
