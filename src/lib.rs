//! mft-engine: a small mid-frequency trading engine for crypto perpetuals.
//!
//! Paper only. Live public market data in, one fixed signal, a risk layer
//! that fails closed, simulated fills, and a backtester that runs the same
//! code path as the live paper engine.
//!
//! Reading order for a newcomer: `event` (the one type), `clock`, `engine`
//! (the shared path), `risk`, `text`, `strategy`, `execution`, then
//! `event_loop` and the two drivers `backtest` and `paper`.

pub mod backtest;
pub mod bars;
pub mod clock;
pub mod engine;
pub mod event;
pub mod event_loop;
pub mod execution;
pub mod feed;
pub mod fetch;
pub mod gap;
pub mod hyperliquid;
pub mod metrics;
pub mod paper;
pub mod record;
pub mod risk;
pub mod strategy;
pub mod text;
