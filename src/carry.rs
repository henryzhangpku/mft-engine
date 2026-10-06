//! Strategy v4: cross-sectional funding-rate carry on Hyperliquid perpetuals.
//!
//! The mechanism. A perpetual has no expiry, so the exchange keeps its price
//! near the index with a funding payment between longs and shorts. On
//! Hyperliquid it is settled **every hour**: a positive rate means longs pay
//! shorts that fraction of position value for the hour. When one side is
//! crowded (many leveraged longs chasing a coin) funding stays high, and
//! whoever takes the other side is paid to do so. v4 collects that payment
//! across the cross-section without taking a view on the market:
//!
//! * every `rebalance_every_hours`, rank the eligible perps by their mean
//!   hourly funding over the last `lookback_hours`;
//! * short the top `bucket_size` (longs pay the most), long the bottom
//!   `bucket_size` (longs pay the least, or are paid);
//! * dollar-neutral: each side carries half of `gross_notional`, split
//!   equally (or by inverse volatility) inside the side;
//! * accrue funding hourly on every open position; pay the taker fee plus
//!   the slippage assumption on every rebalance trade (`FillModel`).
//!
//! The risk is the price leg: coins with crowded longs are often the ones
//! going up, and a short that earns 30% a year in funding can lose that in a
//! day. Whether the carry survives the price leg and the costs is what the
//! pre-registered test measures.
//!
//! This is a cross-sectional, hourly, portfolio strategy, so it does not run
//! through `Engine::on_event` (one coin, one minute bar, one decision). It is
//! a pure function of an hourly panel, deterministic and fingerprinted in the
//! same way: same panel and parameters, same trades, same fingerprint.
//!
//! Point in time. A rebalance at hour `h` uses only closes stamped at or
//! before `h` (a bar is stamped at its close) and funding settled at or
//! before `h`. Funding settled at `h` pays positions held over `(h-1h, h]`,
//! so a position opened at `h` first earns the payment settled at `h+1h`.

use crate::event::{Bar, Event};
use crate::execution::FillModel;
use crate::hyperliquid::FundingRecord;
use crate::metrics::{fnv1a, max_drawdown};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const HOUR_MS: i64 = 3_600_000;
pub const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Weighting {
    /// Every name in a side gets the same notional.
    Equal,
    /// Inside a side, notional proportional to 1 / hourly return volatility.
    InverseVol,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CarryParams {
    /// Hours of settled funding averaged to rank coins.
    pub lookback_hours: usize,
    /// Rebalance every this many hours from the window start, which is a
    /// UTC midnight (24 = daily at 00:00 UTC).
    pub rebalance_every_hours: i64,
    /// Names short (highest funding) and names long (lowest funding).
    pub bucket_size: usize,
    pub weighting: Weighting,
    /// Hours of returns for the inverse-volatility weights.
    pub vol_lookback_hours: usize,
    /// Long notional plus short notional, USD. Each side carries half.
    pub gross_notional: f64,
    /// Rebalance trades smaller than this (USD) are skipped, unless they
    /// close a position completely.
    pub min_trade_notional: f64,
    /// A coin is eligible only if at least this share of the lookback hours
    /// have a settled funding rate.
    pub min_funding_coverage: f64,
    /// A coin whose last close is older than this is not traded into, and a
    /// held position in it is closed at its last close.
    pub stale_price_hours: i64,
    /// Taker fee plus slippage, the same model as every other strategy here.
    pub fills: FillModel,
}

impl Default for CarryParams {
    fn default() -> Self {
        Self {
            lookback_hours: 72,
            rebalance_every_hours: 24,
            bucket_size: 6,
            weighting: Weighting::Equal,
            vol_lookback_hours: 168,
            gross_notional: 10_000.0,
            min_trade_notional: 50.0,
            min_funding_coverage: 0.9,
            stale_price_hours: 2,
            fills: FillModel { taker_fee_bps: 4.5, slippage_bps: 3.0 },
        }
    }
}

/// Hourly closes and settled funding, per coin, on one contiguous hour grid.
#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    /// Hour timestamps (Unix ms, on the hour), ascending, contiguous.
    pub hours: Vec<i64>,
    pub coins: Vec<String>,
    /// `close[c][i]`: close of the bar stamped `hours[i]` (its close time).
    pub close: Vec<Vec<Option<f64>>>,
    /// `funding[c][i]`: the rate settled at `hours[i]`.
    pub funding: Vec<Vec<Option<f64>>>,
}

/// Funding settles a few milliseconds after the hour; snap it to the hour.
pub fn funding_hour(ts: i64) -> i64 {
    (ts + HOUR_MS / 2).div_euclid(HOUR_MS) * HOUR_MS
}

impl Panel {
    /// Build from 1-hour bars and funding records. Rows outside the bar
    /// grid's span are dropped; non-finite values are an error.
    pub fn build(bars: &[Bar], funding: &[FundingRecord]) -> Result<Panel> {
        let coins: BTreeSet<&str> = bars.iter().map(|b| b.coin.as_str()).collect();
        let (Some(first), Some(last)) = (bars.iter().map(|b| b.ts).min(), bars.iter().map(|b| b.ts).max()) else {
            bail!("no bars");
        };
        if first % HOUR_MS != 0 || last % HOUR_MS != 0 {
            bail!("bars are not stamped on the hour");
        }
        let hours: Vec<i64> = (0..=(last - first) / HOUR_MS).map(|k| first + k * HOUR_MS).collect();
        let coins: Vec<String> = coins.into_iter().map(String::from).collect();
        let index: BTreeMap<&str, usize> = coins.iter().enumerate().map(|(i, c)| (c.as_str(), i)).collect();
        let n = hours.len();
        let mut close = vec![vec![None; n]; coins.len()];
        let mut rates = vec![vec![None; n]; coins.len()];
        for b in bars {
            if !b.close.is_finite() || b.close <= 0.0 {
                bail!("{} bar at {} has close {}", b.coin, b.ts, b.close);
            }
            close[index[b.coin.as_str()]][((b.ts - first) / HOUR_MS) as usize] = Some(b.close);
        }
        for f in funding {
            let Some(&c) = index.get(f.coin.as_str()) else { continue };
            let h = funding_hour(f.ts);
            if h < first || h > last {
                continue;
            }
            if !f.rate.is_finite() {
                bail!("{} funding at {} is not finite", f.coin, f.ts);
            }
            rates[c][((h - first) / HOUR_MS) as usize] = Some(f.rate);
        }
        Ok(Panel { hours, coins, close, funding: rates })
    }

    /// Load the committed files: 1-hour `Bar` events and funding JSONL.
    pub fn load(bars_path: &Path, funding_path: &Path) -> Result<Panel> {
        let bars: Vec<Bar> = crate::bars::read_events(bars_path)?
            .into_iter()
            .filter_map(|e| match e {
                Event::Bar(b) => Some(b),
                _ => None,
            })
            .collect();
        let funding = read_funding(funding_path)?;
        Panel::build(&bars, &funding)
    }

    /// Index of hour `ts` on the grid.
    pub fn index_of(&self, ts: i64) -> Option<usize> {
        let first = *self.hours.first()?;
        let i = (ts - first).div_euclid(HOUR_MS);
        (ts % HOUR_MS == 0 && i >= 0 && (i as usize) < self.hours.len()).then_some(i as usize)
    }

    /// The same panel with every hour after `ts` removed. The in-sample run
    /// is given this, so no later number can reach it, even by mistake.
    pub fn truncated_after(&self, ts: i64) -> Panel {
        let keep = self.hours.iter().take_while(|h| **h <= ts).count();
        Panel {
            hours: self.hours[..keep].to_vec(),
            coins: self.coins.clone(),
            close: self.close.iter().map(|v| v[..keep].to_vec()).collect(),
            funding: self.funding.iter().map(|v| v[..keep].to_vec()).collect(),
        }
    }

    /// Only these coins (in panel order), e.g. the pre-chosen universe.
    pub fn restricted_to(&self, keep: &[String]) -> Panel {
        let idx: Vec<usize> = (0..self.coins.len()).filter(|i| keep.contains(&self.coins[*i])).collect();
        Panel {
            hours: self.hours.clone(),
            coins: idx.iter().map(|i| self.coins[*i].clone()).collect(),
            close: idx.iter().map(|i| self.close[*i].clone()).collect(),
            funding: idx.iter().map(|i| self.funding[*i].clone()).collect(),
        }
    }
}

pub fn read_funding(path: &Path) -> Result<Vec<FundingRecord>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("{}:{}: bad funding line", path.display(), i + 1)))
        .collect()
}

/// One trade at a rebalance.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CarryTrade {
    pub coin: String,
    pub qty: f64,
    pub px: f64,
    pub fee: f64,
}

/// What one rebalance decided and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rebalance {
    pub ts: i64,
    pub eligible: usize,
    /// (coin, mean hourly funding over the lookback), highest first.
    pub shorts: Vec<(String, f64)>,
    /// (coin, mean hourly funding), lowest first.
    pub longs: Vec<(String, f64)>,
    pub trades: Vec<CarryTrade>,
    /// Long and short notional after the trades, at the rebalance price.
    pub long_notional: f64,
    pub short_notional: f64,
}

/// Everything one run produced.
#[derive(Debug, Clone, PartialEq)]
pub struct CarryRun {
    pub rebalances: Vec<Rebalance>,
    /// (hour, equity) after every hour, starting at 0.
    pub equity: Vec<(i64, f64)>,
    pub funding_pnl: f64,
    pub fees: f64,
    pub slippage: f64,
    pub traded_notional: f64,
    /// Held-position hours with no settled funding rate (accrued as zero).
    pub missing_funding_hours: u64,
}

impl CarryRun {
    pub fn pnl(&self) -> f64 {
        self.equity.last().map_or(0.0, |(_, e)| *e)
    }
    /// FNV-1a over every rebalance (signals, trades, prices) in order.
    pub fn fingerprint(&self) -> String {
        format!("{:016x}", fnv1a(serde_json::to_string(&self.rebalances).unwrap_or_default().as_bytes()))
    }
}

/// Mean settled funding over the `lookback` hours ending at `i`, if enough
/// of them settled.
fn mean_funding(rates: &[Option<f64>], i: usize, lookback: usize, min_coverage: f64) -> Option<f64> {
    if lookback == 0 || i + 1 < lookback {
        return None;
    }
    let window: Vec<f64> = rates[i + 1 - lookback..=i].iter().flatten().copied().collect();
    if (window.len() as f64) < min_coverage * lookback as f64 || window.is_empty() {
        return None;
    }
    Some(window.iter().sum::<f64>() / window.len() as f64)
}

/// Standard deviation of hourly log returns over the `lookback` hours ending
/// at `i`, using only hours where both closes exist.
fn hourly_vol(closes: &[Option<f64>], i: usize, lookback: usize) -> Option<f64> {
    if i < lookback || lookback < 2 {
        return None;
    }
    let r: Vec<f64> = (i + 1 - lookback..=i)
        .filter_map(|k| match (closes[k - 1], closes[k]) {
            (Some(a), Some(b)) => Some((b / a).ln()),
            _ => None,
        })
        .collect();
    if r.len() < lookback / 2 {
        return None;
    }
    let m = r.iter().sum::<f64>() / r.len() as f64;
    let v = r.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (r.len() - 1) as f64;
    (v > 0.0 && v.is_finite()).then(|| v.sqrt())
}

/// Target notional per coin (positive long, negative short) at hour `i`.
/// Returns the targets, the eligible count and the two buckets.
pub fn targets(panel: &Panel, i: usize, last_seen: &[Option<usize>], p: &CarryParams) -> (Vec<f64>, usize, Vec<(String, f64)>, Vec<(String, f64)>) {
    let mut ranked: Vec<(usize, f64)> = (0..panel.coins.len())
        .filter(|&c| last_seen[c].is_some_and(|k| (i - k) as i64 <= p.stale_price_hours))
        .filter_map(|c| mean_funding(&panel.funding[c], i, p.lookback_hours, p.min_funding_coverage).map(|f| (c, f)))
        .collect();
    let eligible = ranked.len();
    let mut out = vec![0.0; panel.coins.len()];
    if eligible < 2 * p.bucket_size || p.bucket_size == 0 {
        return (out, eligible, vec![], vec![]);
    }
    // Highest funding first; ties broken by coin name, so the order is fixed.
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| panel.coins[a.0].cmp(&panel.coins[b.0])));
    let shorts: Vec<(usize, f64)> = ranked[..p.bucket_size].to_vec();
    let mut longs: Vec<(usize, f64)> = ranked[eligible - p.bucket_size..].to_vec();
    longs.reverse();
    let side = p.gross_notional / 2.0;
    for (bucket, sign) in [(&shorts, -1.0), (&longs, 1.0)] {
        let raw: Vec<f64> = bucket
            .iter()
            .map(|(c, _)| match p.weighting {
                Weighting::Equal => 1.0,
                Weighting::InverseVol => hourly_vol(&panel.close[*c], i, p.vol_lookback_hours).map_or(0.0, |v| 1.0 / v),
            })
            .collect();
        let total: f64 = raw.iter().sum();
        for ((c, _), w) in bucket.iter().zip(&raw) {
            out[*c] = if total > 0.0 { sign * side * w / total } else { 0.0 };
        }
    }
    let named = |b: &[(usize, f64)]| b.iter().map(|(c, f)| (panel.coins[*c].clone(), *f)).collect();
    (out, eligible, named(&shorts), named(&longs))
}

/// Run v4 over hours `[from_ts, to_ts]` of `panel`, starting flat with zero
/// cash. Earlier hours are history the signal may read; later hours are
/// never touched. Every position is closed at `to_ts`, with costs.
pub fn run(panel: &Panel, p: &CarryParams, from_ts: i64, to_ts: i64) -> Result<CarryRun> {
    let start = panel.index_of(from_ts).context("window start is not on the panel's hour grid")?;
    let end = panel.index_of(to_ts).context("window end is not on the panel's hour grid")?;
    if end <= start {
        bail!("empty window");
    }
    let nc = panel.coins.len();
    let mut qty = vec![0.0_f64; nc];
    let mut last_px: Vec<Option<f64>> = vec![None; nc];
    let mut last_seen: Vec<Option<usize>> = vec![None; nc];
    // Prices before the window are history: the first rebalance may need them.
    for c in 0..nc {
        if let Some(k) = (0..=start).rev().find(|k| panel.close[c][*k].is_some()) {
            last_px[c] = panel.close[c][k];
            last_seen[c] = Some(k);
        }
    }
    let mut run = CarryRun {
        rebalances: vec![],
        equity: vec![],
        funding_pnl: 0.0,
        fees: 0.0,
        slippage: 0.0,
        traded_notional: 0.0,
        missing_funding_hours: 0,
    };
    let mut cash = 0.0_f64;

    for i in start..=end {
        let h = panel.hours[i];
        for c in 0..nc {
            if let Some(px) = panel.close[c][i] {
                last_px[c] = Some(px);
                last_seen[c] = Some(i);
            }
        }
        // 1. Funding settled at h, on positions held over the past hour.
        //    Positive rate: longs pay shorts, so a long's cash goes down.
        if i > start {
            for c in 0..nc {
                if qty[c] == 0.0 {
                    continue;
                }
                match (panel.funding[c][i], last_px[c]) {
                    (Some(rate), Some(px)) => {
                        let payment = funding_payment(qty[c], px, rate);
                        cash += payment;
                        run.funding_pnl += payment;
                    }
                    _ => run.missing_funding_hours += 1,
                }
            }
        }
        // 2. Rebalance (or close everything at the last hour).
        // The schedule is anchored at the window start (a UTC midnight), so
        // every window opens its book at its first hour.
        let rebalance_hour = (h - from_ts).rem_euclid(p.rebalance_every_hours * HOUR_MS) == 0;
        if i == end || rebalance_hour {
            let (target, eligible, shorts, longs) = if i == end {
                (vec![0.0; nc], 0, vec![], vec![])
            } else {
                targets(panel, i, &last_seen, p)
            };
            let mut trades = Vec::new();
            for c in 0..nc {
                // A stale coin is closed at its last close; never entered.
                let Some(px) = last_px[c] else { continue };
                let stale = last_seen[c].map_or(true, |k| (i - k) as i64 > p.stale_price_hours);
                let want = if stale { 0.0 } else { target[c] / px };
                let delta = want - qty[c];
                let closes_out = want == 0.0 && qty[c] != 0.0;
                if delta == 0.0 || (!closes_out && (delta * px).abs() < p.min_trade_notional) {
                    continue;
                }
                let fill = p.fills.fill(&panel.coins[c], h, delta, px);
                cash -= fill.qty * fill.px + fill.fee;
                run.fees += fill.fee;
                run.slippage += (fill.px - px).abs() * delta.abs();
                run.traded_notional += (fill.qty * fill.px).abs();
                qty[c] = if closes_out { 0.0 } else { want };
                trades.push(CarryTrade { coin: panel.coins[c].clone(), qty: delta, px: fill.px, fee: fill.fee });
            }
            let notional = |sign: f64| -> f64 {
                (0..nc).filter(|c| qty[*c] * sign > 0.0).map(|c| (qty[c] * last_px[c].unwrap_or(0.0)).abs()).sum()
            };
            run.rebalances.push(Rebalance { ts: h, eligible, shorts, longs, trades, long_notional: notional(1.0), short_notional: notional(-1.0) });
        }
        let open: f64 = (0..nc).map(|c| qty[c] * last_px[c].unwrap_or(0.0)).sum();
        run.equity.push((h, cash + open));
    }
    Ok(run)
}

/// Cash received by a position of `qty` coins (signed) for one hour of
/// funding at `rate`, valued at `px`. Positive rate: longs pay, shorts
/// receive. Hyperliquid: payment = size * oracle price * rate, every hour.
pub fn funding_payment(qty: f64, px: f64, rate: f64) -> f64 {
    -qty * px * rate
}

/// Buy and hold `notional` of one coin over the window, paying funding and
/// one entry and one exit trade: the simple benchmark.
pub fn buy_and_hold(panel: &Panel, coin: &str, notional: f64, fills: &FillModel, from_ts: i64, to_ts: i64) -> Result<CarryRun> {
    let c = panel.coins.iter().position(|x| x == coin).with_context(|| format!("{coin} not in the panel"))?;
    let start = panel.index_of(from_ts).context("start off grid")?;
    let end = panel.index_of(to_ts).context("end off grid")?;
    let mut run = CarryRun { rebalances: vec![], equity: vec![], funding_pnl: 0.0, fees: 0.0, slippage: 0.0, traded_notional: 0.0, missing_funding_hours: 0 };
    let (mut cash, mut qty, mut px) = (0.0, 0.0, panel.close[c][start].context("no benchmark price at start")?);
    for i in start..=end {
        px = panel.close[c][i].unwrap_or(px);
        if i > start {
            match panel.funding[c][i] {
                Some(rate) => {
                    let pay = funding_payment(qty, px, rate);
                    cash += pay;
                    run.funding_pnl += pay;
                }
                None => run.missing_funding_hours += 1,
            }
        }
        let delta = if i == start { notional / px } else if i == end { -qty } else { 0.0 };
        if delta != 0.0 {
            let fill = fills.fill(coin, panel.hours[i], delta, px);
            cash -= fill.qty * fill.px + fill.fee;
            run.fees += fill.fee;
            run.slippage += (fill.px - px).abs() * delta.abs();
            run.traded_notional += (fill.qty * fill.px).abs();
            qty += delta;
        }
        run.equity.push((panel.hours[i], cash + qty * px));
    }
    Ok(run)
}

/// SplitMix64: a tiny fixed-output generator, so the bootstrap is the same
/// on every machine and every run.
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Moving-block bootstrap (circular) of the mean: `resamples` series of the
/// same length built from blocks of `block` consecutive values. Returns the
/// 2.5% and 97.5% quantiles of the resampled means and the share of
/// resampled means at or below zero.
pub fn block_bootstrap_mean(values: &[f64], block: usize, resamples: usize, seed: u64) -> Option<(f64, f64, f64)> {
    let n = values.len();
    if n < 2 || block == 0 || resamples == 0 {
        return None;
    }
    let mut rng = SplitMix64(seed);
    let mut means = Vec::with_capacity(resamples);
    for _ in 0..resamples {
        let mut sum = 0.0;
        let mut taken = 0;
        while taken < n {
            let s = rng.below(n);
            for k in 0..block.min(n - taken) {
                sum += values[(s + k) % n];
            }
            taken += block.min(n - taken);
        }
        means.push(sum / n as f64);
    }
    means.sort_by(f64::total_cmp);
    let q = |p: f64| means[((p * resamples as f64) as usize).min(resamples - 1)];
    let at_or_below_zero = means.iter().filter(|m| **m <= 0.0).count() as f64 / resamples as f64;
    Some((q(0.025), q(0.975), at_or_below_zero))
}

/// Daily P&L: equity differences between successive 00:00 UTC marks (the
/// window starts and ends on a midnight, so every day is whole).
pub fn daily_pnl(equity: &[(i64, f64)]) -> Vec<f64> {
    let marks: Vec<f64> = equity.iter().filter(|(t, _)| t.rem_euclid(DAY_MS) == 0).map(|(_, e)| *e).collect();
    marks.windows(2).map(|w| w[1] - w[0]).collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CarryReport {
    pub window: String,
    pub start: String,
    pub end: String,
    pub days: usize,
    pub rebalances: usize,
    pub trades: usize,
    /// Net P&L after fees, slippage and funding, USD (positions closed at the end).
    pub pnl_net: f64,
    pub funding_pnl: f64,
    pub fees: f64,
    pub slippage: f64,
    /// pnl_net - funding + fees + slippage: what prices did.
    pub price_pnl: f64,
    pub traded_notional: f64,
    /// Traded notional / gross notional, per day.
    pub turnover_per_day: f64,
    pub mean_daily_pnl: f64,
    pub std_daily_pnl: f64,
    /// Daily, annualised with sqrt(365): crypto trades every day.
    pub sharpe_annualised: Option<f64>,
    /// mean daily P&L * 365 / gross notional, percent.
    pub annualised_return_pct: f64,
    pub max_drawdown: f64,
    pub positive_days: usize,
    /// 95% moving-block bootstrap interval for the mean daily P&L.
    pub mean_daily_ci95: Option<(f64, f64)>,
    /// Share of bootstrap means at or below zero.
    pub bootstrap_share_at_or_below_zero: Option<f64>,
    pub missing_funding_hours: u64,
    pub fingerprint: String,
}

pub fn report(window: &str, run: &CarryRun, gross: f64, block_days: usize, resamples: usize, seed: u64) -> CarryReport {
    let daily = daily_pnl(&run.equity);
    let n = daily.len();
    let mean = if n > 0 { daily.iter().sum::<f64>() / n as f64 } else { 0.0 };
    let std = if n > 1 { (daily.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt() } else { 0.0 };
    let boot = block_bootstrap_mean(&daily, block_days, resamples, seed);
    let pnl = run.pnl();
    let fmt = |t: i64| crate::clock::format_utc(t);
    CarryReport {
        window: window.to_string(),
        start: run.equity.first().map_or(String::new(), |(t, _)| fmt(*t)),
        end: run.equity.last().map_or(String::new(), |(t, _)| fmt(*t)),
        days: n,
        rebalances: run.rebalances.len(),
        trades: run.rebalances.iter().map(|r| r.trades.len()).sum(),
        pnl_net: pnl,
        funding_pnl: run.funding_pnl,
        fees: run.fees,
        slippage: run.slippage,
        price_pnl: pnl - run.funding_pnl + run.fees + run.slippage,
        traded_notional: run.traded_notional,
        turnover_per_day: if n > 0 { run.traded_notional / gross / n as f64 } else { 0.0 },
        mean_daily_pnl: mean,
        std_daily_pnl: std,
        sharpe_annualised: (std > 0.0).then(|| mean / std * 365f64.sqrt()),
        annualised_return_pct: mean * 365.0 / gross * 100.0,
        max_drawdown: max_drawdown(&run.equity.iter().map(|(_, e)| *e).collect::<Vec<_>>()),
        positive_days: daily.iter().filter(|d| **d > 0.0).count(),
        mean_daily_ci95: boot.map(|(lo, hi, _)| (lo, hi)),
        bootstrap_share_at_or_below_zero: boot.map(|(_, _, z)| z),
        missing_funding_hours: run.missing_funding_hours,
        fingerprint: run.fingerprint(),
    }
}
