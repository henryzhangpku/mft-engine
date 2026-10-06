//! Crowd positioning from Hyperliquid's public data: what the best recent
//! traders on the leaderboard hold, aggregated per coin.
//!
//! Inputs, both public and read-only:
//! * `https://stats-data.hyperliquid.xyz/Mainnet/leaderboard`: every ranked
//!   account with its account value and PnL per window (`day`, `week`,
//!   `month`, `allTime`);
//! * `/info {"type":"clearinghouseState","user":...}`: one public address's
//!   open perp positions.
//!
//! Method (fixed before use):
//! 1. At start, choose the wallet set once: the 100 accounts with the highest
//!    30-day PnL among those with at least $100k account value. Keeping the
//!    set fixed means a change between snapshots is a change in positions,
//!    not a change in who is on the list.
//! 2. Each snapshot reads every wallet's positions and sums, per coin, the
//!    long and short position value, counts and leverage.
//! 3. `long_share` = long value / (long + short value). The crowd label uses
//!    the old analyser's rule: bullish above 60% long, bearish below 40%.
//!
//! Only aggregates leave this module. Addresses are used to make requests and
//! are never written to any file.
//!
//! There is no history: the API returns current state only, so positioning
//! exists in a replay only where it was recorded live.

use crate::event::Positioning;
use crate::hyperliquid::{get_public, info};
use crate::text::Caution;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

pub const LEADERBOARD_URL: &str = "https://stats-data.hyperliquid.xyz/Mainnet/leaderboard";

#[derive(Debug, Clone, Deserialize)]
pub struct WindowPerf {
    #[serde(default)]
    pub pnl: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaderRow {
    pub eth_address: String,
    #[serde(default)]
    pub account_value: Option<String>,
    /// e.g. `[["day", {...}], ["week", {...}], ["month", {...}], ["allTime", {...}]]`.
    #[serde(default)]
    pub window_performances: Vec<(String, WindowPerf)>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Leaderboard {
    leaderboard_rows: Vec<LeaderRow>,
}

fn parse(s: &Option<String>) -> f64 {
    s.as_deref().and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite()).unwrap_or(0.0)
}

impl LeaderRow {
    pub fn window_pnl(&self, window: &str) -> f64 {
        self.window_performances.iter().find(|(w, _)| w == window).map_or(0.0, |(_, p)| parse(&p.pnl))
    }
}

/// The wallet set: top `n` by 30-day PnL with at least `min_account_value`.
/// Ties broken by address so the choice is deterministic.
pub fn select_wallets(rows: &[LeaderRow], n: usize, min_account_value: f64) -> Vec<String> {
    let mut eligible: Vec<(&LeaderRow, f64)> = rows
        .iter()
        .filter(|r| parse(&r.account_value) >= min_account_value && !r.eth_address.is_empty())
        .map(|r| (r, r.window_pnl("month")))
        .collect();
    eligible.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.eth_address.cmp(&b.0.eth_address)));
    eligible.into_iter().take(n).map(|(r, _)| r.eth_address.to_lowercase()).collect()
}

pub fn fetch_leaderboard() -> Result<Vec<LeaderRow>> {
    let lb: Leaderboard = get_public(LEADERBOARD_URL)?;
    Ok(lb.leaderboard_rows)
}

/// One wallet's open position in one coin.
#[derive(Debug, Clone, PartialEq)]
pub struct WalletPosition {
    pub coin: String,
    /// Signed size: positive long, negative short.
    pub szi: f64,
    pub position_value: f64,
    pub leverage: f64,
}

#[derive(Deserialize)]
struct Clearinghouse {
    #[serde(rename = "assetPositions", default)]
    asset_positions: Vec<AssetPosition>,
}

#[derive(Deserialize)]
struct AssetPosition {
    position: RawPosition,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPosition {
    coin: String,
    szi: String,
    #[serde(default)]
    position_value: Option<String>,
    #[serde(default)]
    leverage: Option<Leverage>,
}

#[derive(Deserialize)]
struct Leverage {
    #[serde(default)]
    value: f64,
}

pub fn fetch_positions(address: &str) -> Result<Vec<WalletPosition>> {
    let ch: Clearinghouse = info(&serde_json::json!({ "type": "clearinghouseState", "user": address }))?;
    Ok(ch
        .asset_positions
        .into_iter()
        .filter_map(|a| {
            let szi: f64 = a.position.szi.parse().ok()?;
            (szi.abs() > 1e-12).then(|| WalletPosition {
                coin: a.position.coin,
                szi,
                position_value: parse(&a.position.position_value).abs(),
                leverage: a.position.leverage.map_or(0.0, |l| l.value),
            })
        })
        .collect())
}

/// Sum positions per coin. `wallets` holds each scanned wallet's positions.
pub fn aggregate(wallets: &[Vec<WalletPosition>], ts: i64) -> BTreeMap<String, Positioning> {
    let mut out: BTreeMap<String, Positioning> = BTreeMap::new();
    let mut lev_sums: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for positions in wallets {
        for p in positions {
            let e = out.entry(p.coin.clone()).or_insert_with(|| Positioning {
                coin: p.coin.clone(),
                ts,
                source: "hyperliquid-leaderboard".into(),
                wallets_scanned: wallets.len() as u32,
                wallets_holding: 0,
                long_count: 0,
                short_count: 0,
                long_value: 0.0,
                short_value: 0.0,
                long_share: 0.5,
                long_share_change: None,
                avg_long_leverage: 0.0,
                avg_short_leverage: 0.0,
            });
            let lev = lev_sums.entry(p.coin.clone()).or_default();
            e.wallets_holding += 1;
            if p.szi > 0.0 {
                e.long_count += 1;
                e.long_value += p.position_value;
                lev.0 += p.leverage;
            } else {
                e.short_count += 1;
                e.short_value += p.position_value;
                lev.1 += p.leverage;
            }
        }
    }
    for (coin, e) in out.iter_mut() {
        let gross = e.long_value + e.short_value;
        e.long_share = if gross > 0.0 { e.long_value / gross } else { 0.5 };
        let (ll, sl) = lev_sums[coin];
        e.avg_long_leverage = if e.long_count > 0 { ll / f64::from(e.long_count) } else { 0.0 };
        e.avg_short_leverage = if e.short_count > 0 { sl / f64::from(e.short_count) } else { 0.0 };
    }
    out
}

/// The old analyser's label.
pub fn crowd_label(long_share: f64) -> &'static str {
    if long_share > 0.6 {
        "bullish"
    } else if long_share < 0.4 {
        "bearish"
    } else {
        "neutral"
    }
}

/// Holds the wallet set and the previous snapshot, so each new snapshot can
/// report the change.
pub struct Scanner {
    wallets: Vec<String>,
    previous: BTreeMap<String, f64>,
}

impl Scanner {
    /// Download the leaderboard and fix the wallet set.
    pub fn from_leaderboard(n: usize, min_account_value: f64) -> Result<Self> {
        let rows = fetch_leaderboard()?;
        let wallets = select_wallets(&rows, n, min_account_value);
        if wallets.is_empty() {
            bail!("no leaderboard wallets passed the filter");
        }
        Ok(Self { wallets, previous: BTreeMap::new() })
    }

    pub fn wallet_count(&self) -> usize {
        self.wallets.len()
    }

    /// Read every wallet and aggregate. Fails (emits nothing) if more than a
    /// fifth of the reads fail: a partial crowd is a different crowd.
    pub fn snapshot(&mut self, now_ms: impl Fn() -> i64) -> Result<BTreeMap<String, Positioning>> {
        let mut read = Vec::with_capacity(self.wallets.len());
        let mut failed = 0usize;
        for w in &self.wallets {
            match fetch_positions(w) {
                Ok(p) => read.push(p),
                Err(_) => failed += 1,
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if failed * 5 > self.wallets.len() {
            bail!("{failed} of {} wallet reads failed; snapshot dropped", self.wallets.len());
        }
        let mut agg = aggregate(&read, now_ms());
        for (coin, p) in agg.iter_mut() {
            p.long_share_change = self.previous.get(coin).map(|prev| p.long_share - prev);
            self.previous.insert(coin.clone(), p.long_share);
        }
        Ok(agg)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PositioningParams {
    /// Strategy v3 switch.
    pub enabled: bool,
    /// Gate entries only, as v2 does with Kalshi.
    pub gate_entries_only: bool,
    /// A snapshot older than this closes the gate.
    pub max_age_ms: i64,
    /// Fewer holders than this is not a crowd; the gate closes.
    pub min_holders: u32,
    /// Long share needed for a long (and 1 minus it for a short).
    pub min_share: f64,
}

impl Default for PositioningParams {
    fn default() -> Self {
        Self {
            enabled: false,
            gate_entries_only: true,
            max_age_ms: 15 * 60_000,
            min_holders: 5,
            min_share: 0.6,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PositioningState {
    params: PositioningParams,
    latest: BTreeMap<String, Positioning>,
}

impl PositioningState {
    pub fn new(params: PositioningParams) -> Self {
        Self { params, latest: BTreeMap::new() }
    }

    pub fn on_snapshot(&mut self, p: &Positioning) {
        self.latest.insert(p.coin.clone(), p.clone());
    }

    /// The latest fresh snapshot's long share, if any.
    pub fn long_share(&self, coin: &str, now_ms: i64) -> Option<f64> {
        let p = self.latest.get(coin)?;
        let fresh = (0..=self.params.max_age_ms).contains(&(now_ms - p.ts));
        (fresh && p.wallets_holding >= self.params.min_holders).then_some(p.long_share)
    }

    /// The v3 gate: enter only with the crowd of top wallets. Fails closed.
    pub fn caution_for(&self, coin: &str, target_notional: f64, position_notional: f64, now_ms: i64) -> Caution {
        if !self.params.enabled || target_notional == 0.0 {
            return Caution::NONE;
        }
        if self.params.gate_entries_only && position_notional * target_notional > 0.0 {
            return Caution::NONE;
        }
        let Some(share) = self.long_share(coin, now_ms) else {
            return Caution::new(0.0);
        };
        let agree = if target_notional > 0.0 { share } else { 1.0 - share };
        if agree > self.params.min_share {
            Caution::NONE
        } else {
            Caution::new(0.0)
        }
    }
}

/// `positioning` subcommand: one snapshot, printed as a contract-sentiment
/// table, and saved (aggregates only).
pub fn run(coins: &[String], top: usize, min_account_value: f64, out: &std::path::Path) -> Result<()> {
    let mut scanner = Scanner::from_leaderboard(top, min_account_value)?;
    println!("wallet set: top {} by 30-day PnL with account value >= ${min_account_value:.0}", scanner.wallet_count());
    let snap = scanner.snapshot(crate::clock::wall_now_ms)?;
    let mut rows: Vec<&Positioning> = snap.values().collect();
    rows.sort_by(|a, b| (b.long_value + b.short_value).total_cmp(&(a.long_value + a.short_value)));
    println!(
        "\n{:<14} {:>7} {:>6} {:>6} {:>12} {:>12} {:>7} {:>7} {:>7}  crowd",
        "coin", "holders", "longs", "shorts", "long $M", "short $M", "long %", "lev L", "lev S"
    );
    let wanted = |c: &str| coins.is_empty() || coins.iter().any(|w| w == c);
    for p in rows.iter().filter(|p| wanted(&p.coin)).take(25) {
        println!(
            "{:<14} {:>7} {:>6} {:>6} {:>12.1} {:>12.1} {:>6.1}% {:>7.1} {:>7.1}  {}",
            p.coin,
            p.wallets_holding,
            p.long_count,
            p.short_count,
            p.long_value / 1e6,
            p.short_value / 1e6,
            p.long_share * 100.0,
            p.avg_long_leverage,
            p.avg_short_leverage,
            crowd_label(p.long_share)
        );
    }
    let kept: Vec<&Positioning> = rows.into_iter().filter(|p| wanted(&p.coin)).collect();
    crate::artifacts::write_jsonl(out, &kept)?;
    println!("\nwrote {} coin aggregates to {} (no addresses)", kept.len(), out.display());
    Ok(())
}
