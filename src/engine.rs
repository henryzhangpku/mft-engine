//! The engine: strategy, text caution, risk and paper fills, wired together.
//!
//! This is the code path that backtest and paper share. `Engine::on_event`
//! takes one `Event` and a `Clock` and returns at most one `Decision`. It does
//! no I/O and never reads the system clock, which is why a replay is
//! deterministic: same events in, same decisions out.
//!
//! The order of operations for a bar is the whole design in six lines:
//! 1. update market state (last price, data freshness);
//! 2. the strategy proposes a target notional;
//! 3. the text signal may scale that target down (never up), and we check it;
//! 4. target minus current position becomes an order intent;
//! 5. every risk rule must allow it, or it is blocked with a reason;
//! 6. the fill model fills it and the portfolio books it.

use crate::clock::Clock;
use crate::event::{Bar, BookTop, Event};
use crate::execution::{Fill, FillModel, Portfolio};
use crate::risk::{OrderIntent, RiskContext, RiskEngine, RiskLimits};
use crate::strategy::{Momentum, MomentumParams};
use crate::text::{check_not_riskier, TextParams, TextState};
use std::collections::BTreeMap;

const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EngineConfig {
    pub strategy: MomentumParams,
    pub text: TextParams,
    pub risk: RiskLimits,
    pub fills: FillModel,
    /// Differences between target and position smaller than this (USD) are
    /// not traded, so price drift does not churn fees.
    pub min_order_notional: f64,
    /// A book top older than this is not used as the fill reference. The
    /// public feed snapshots the book about every 5.4 s, so this is roughly
    /// two snapshots.
    pub book_fresh_ms: i64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            strategy: MomentumParams::default(),
            text: TextParams::default(),
            risk: RiskLimits::default(),
            fills: FillModel::default(),
            min_order_notional: 50.0,
            book_fresh_ms: 12_000,
        }
    }
}

/// What happened to a proposed order.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum Decision {
    Filled {
        fill: Fill,
        /// Strategy target before the text rule, and after it.
        raw_target: f64,
        target: f64,
    },
    Blocked {
        coin: String,
        ts: i64,
        qty: f64,
        raw_target: f64,
        target: f64,
        reason: String,
    },
}

/// What the engine knows about one coin's market.
#[derive(Debug, Default, Clone)]
struct MarketState {
    last_px: Option<f64>,
    book: Option<BookTop>,
    /// Newest exchange timestamp seen for this coin. Freshness is judged as
    /// "engine clock minus this", so data that arrives late (a lagging feed)
    /// counts as old even though we only just received it.
    last_data_ms: Option<i64>,
}

impl MarketState {
    fn touch(&mut self, exchange_ts: i64) {
        self.last_data_ms = Some(self.last_data_ms.map_or(exchange_ts, |t| t.max(exchange_ts)));
    }
}

pub struct Engine {
    config: EngineConfig,
    strategy: Momentum,
    text: TextState,
    risk: RiskEngine,
    pub portfolio: Portfolio,
    market: BTreeMap<String, MarketState>,
    current_day: Option<i64>,
    day_start_equity: f64,
    /// How many times the text rule reduced or vetoed a target.
    pub text_reductions: u64,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self::with_risk(config, RiskEngine::new(config.risk))
    }

    /// Build with a custom risk engine. Used by tests to inject a failing rule.
    pub fn with_risk(config: EngineConfig, risk: RiskEngine) -> Self {
        Self {
            config,
            strategy: Momentum::new(config.strategy),
            text: TextState::new(config.text),
            risk,
            portfolio: Portfolio::default(),
            market: BTreeMap::new(),
            current_day: None,
            day_start_equity: 0.0,
            text_reductions: 0,
        }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// The strategy's latest z-score for `coin`, for explaining decisions.
    pub fn signal_z(&self, coin: &str) -> Option<f64> {
        self.strategy.last_z(coin)
    }

    /// Latest price per coin, for marking positions.
    pub fn marks(&self) -> BTreeMap<String, f64> {
        self.market
            .iter()
            .filter_map(|(coin, m)| m.last_px.map(|px| (coin.clone(), px)))
            .collect()
    }

    pub fn equity(&self) -> f64 {
        self.portfolio.equity(&self.marks())
    }

    /// Feed history to the strategy only. No orders can result. Paper mode
    /// uses this to fill the 60-bar volatility window from REST candles so it
    /// does not have to wait an hour before it can trade.
    pub fn warm_up(&mut self, bar: &Bar) {
        let _ = self.strategy.on_bar(bar);
    }

    /// The single entry point for both modes.
    pub fn on_event(&mut self, event: &Event, clock: &dyn Clock) -> Option<Decision> {
        let now = clock.now_ms();
        self.roll_day(now);
        match event {
            Event::Trade(t) => {
                let m = self.market.entry(t.coin.clone()).or_default();
                m.last_px = Some(t.px);
                m.touch(t.ts);
                None
            }
            Event::BookTop(b) => {
                let m = self.market.entry(b.coin.clone()).or_default();
                m.book = Some(b.clone());
                m.touch(b.ts);
                None
            }
            Event::TextSignal(s) => {
                self.text.on_signal(s);
                None
            }
            Event::Gap(g) => {
                // A hole in trades, bars or the connection means the bar
                // series may be missing prices: forget the history so no
                // signal is computed across it. A quiet book stream does not
                // touch the bars (they come from trades); its only effect is
                // that a stale book stops being used as the fill reference.
                if g.stream != "book" {
                    self.strategy.on_gap(&g.coin);
                }
                None
            }
            Event::Bar(bar) => self.on_bar(bar, now),
        }
    }

    fn on_bar(&mut self, bar: &Bar, now: i64) -> Option<Decision> {
        // 1. Market state.
        {
            let m = self.market.entry(bar.coin.clone()).or_default();
            m.last_px = Some(bar.close);
            m.touch(bar.ts);
        }

        // 2. Strategy target.
        let raw_target = self.strategy.on_bar(bar);

        // 3. Text caution, then the check that it did not add risk.
        let caution = self.text.caution_for(&bar.coin, raw_target, now);
        let target = caution.apply(raw_target);
        let position = self.portfolio.position(&bar.coin);
        if let Err(reason) = check_not_riskier(raw_target, target) {
            return Some(Decision::Blocked {
                coin: bar.coin.clone(),
                ts: now,
                qty: 0.0,
                raw_target,
                target,
                reason: format!("text_rule: {reason}"),
            });
        }
        if target != raw_target {
            self.text_reductions += 1;
        }

        // 4. Order intent from target minus position.
        let mark = bar.close;
        let delta_qty = target / mark - position;
        if (delta_qty * mark).abs() < self.config.min_order_notional {
            return None;
        }
        let ref_px = self.reference_price(&bar.coin, delta_qty, now);
        let order = OrderIntent {
            coin: bar.coin.clone(),
            qty: delta_qty,
            ref_px,
        };

        // 5. Risk.
        let ctx = RiskContext {
            now_ms: now,
            position_qty: position,
            last_data_ms: self.market.get(&bar.coin).and_then(|m| m.last_data_ms),
            daily_pnl: self.equity() - self.day_start_equity,
        };
        if let Err(reason) = self.risk.check(&order, &ctx) {
            return Some(Decision::Blocked {
                coin: bar.coin.clone(),
                ts: now,
                qty: delta_qty,
                raw_target,
                target,
                reason,
            });
        }

        // 6. Paper fill.
        let fill = self.config.fills.fill(&bar.coin, now, delta_qty, ref_px);
        self.portfolio.apply(&fill);
        Some(Decision::Filled {
            fill,
            raw_target,
            target,
        })
    }

    /// The touch if the book is fresh, otherwise the last price. A crossed or
    /// non-positive book is ignored rather than trusted.
    fn reference_price(&self, coin: &str, qty: f64, now: i64) -> f64 {
        let Some(m) = self.market.get(coin) else {
            return f64::NAN; // no market at all: risk will reject NaN
        };
        let last = m.last_px.unwrap_or(f64::NAN);
        match &m.book {
            Some(b)
                if (now - b.ts).abs() <= self.config.book_fresh_ms
                    && b.bid_px > 0.0
                    && b.ask_px > b.bid_px =>
            {
                if qty > 0.0 {
                    b.ask_px
                } else {
                    b.bid_px
                }
            }
            _ => last,
        }
    }

    /// At each new UTC day, remember the equity so the daily loss is measured
    /// from there.
    fn roll_day(&mut self, now: i64) {
        let day = now.div_euclid(DAY_MS);
        if self.current_day != Some(day) {
            self.current_day = Some(day);
            self.day_start_equity = self.equity();
        }
    }
}
