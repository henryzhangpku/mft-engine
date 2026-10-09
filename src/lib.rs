//! mft-engine: a small mid-frequency trading engine for crypto perpetuals.
//!
//! Paper only. Live public market data in, one fixed signal, a risk layer
//! that fails closed, simulated fills, and a backtester that runs the same
//! code path as the live paper engine.
//!
//! Reading order for a newcomer: `event` (the one type), `clock`, `engine`
//! (the shared path), `risk`, `text`, `strategy`, `execution`, then
//! `event_loop` and the two drivers `backtest` and `paper`; `verify` diffs a
//! live session against its replay, and `dsr` and `trials` deflate a ledger
//! entry's Sharpe ratio by every trial on the ledger.

pub mod artifacts;
pub mod backtest;
pub mod bars;
pub mod carry;
pub mod carry_research;
pub mod clock;
pub mod demo;
pub mod dsr;
pub mod engine;
pub mod event;
pub mod event_loop;
pub mod execution;
pub mod experiment;
pub mod feed;
pub mod fetch;
pub mod gap;
pub mod hyperliquid;
pub mod kalshi;
pub mod ledger;
pub mod metrics;
pub mod paper;
pub mod polymarket;
pub mod pollers;
pub mod positioning;
pub mod prediction;
pub mod record;
pub mod risk;
pub mod strategy;
pub mod text;
pub mod trials;
pub mod universe;
pub mod verify;
