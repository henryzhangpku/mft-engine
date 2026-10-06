//! `universe`: every Hyperliquid perpetual across every dex, from the public
//! info API.
//!
//! Hyperliquid has the main dex (native crypto perps) and builder-deployed
//! dexes (HIP-3), such as `xyz` for equity and index perps. `perpDexs` lists
//! the dexes; `metaAndAssetCtxs` with a `dex` field returns each dex's
//! contracts (name, size decimals, max leverage) and live context (funding,
//! open interest, mark price, volume).
//!
//! Funding: the `funding` field is the rate paid per hour, so the annualised
//! rate is `funding * 24 * 365`. (The older Python scanner multiplied by
//! `3 * 365`, as if funding were paid every 8 hours, which understated it by
//! a factor of 8.)

use crate::artifacts::write_json;
use crate::clock::{format_utc, wall_now_ms};
use crate::hyperliquid::info;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contract {
    /// "" for the main dex, otherwise the builder dex name, e.g. "xyz".
    pub dex: String,
    /// As the API names it; builder coins carry the dex prefix ("xyz:NVDA").
    pub coin: String,
    pub sz_decimals: u32,
    pub max_leverage: u32,
    pub delisted: bool,
    pub mark_px: Option<f64>,
    /// Hourly funding rate (a fraction, not percent).
    pub funding_hourly: Option<f64>,
    pub funding_annualized_pct: Option<f64>,
    /// Open interest in coin units, and valued at the mark price.
    pub open_interest: Option<f64>,
    pub open_interest_usd: Option<f64>,
    pub day_volume_usd: Option<f64>,
}

/// The funding zones from the old scanner, kept as a readable summary of how
/// crowded one side is, on correctly annualised funding (percent per year).
pub fn funding_zone(annualized_pct: f64) -> &'static str {
    match annualized_pct {
        f if f > 100.0 => "extreme long",
        f if f > 50.0 => "hot long",
        f if f > 5.0 => "mild long",
        f if f > -5.0 => "neutral",
        f if f > -50.0 => "mild short",
        f if f > -100.0 => "hot short",
        _ => "extreme short",
    }
}

#[derive(Deserialize)]
struct Meta {
    universe: Vec<AssetMeta>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssetMeta {
    name: String,
    sz_decimals: u32,
    max_leverage: u32,
    #[serde(default)]
    is_delisted: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssetCtx {
    #[serde(default)]
    funding: Option<String>,
    #[serde(default)]
    open_interest: Option<String>,
    #[serde(default)]
    mark_px: Option<String>,
    #[serde(default)]
    day_ntl_vlm: Option<String>,
}

#[derive(Deserialize)]
struct DexInfo {
    name: String,
}

fn num(s: &Option<String>) -> Option<f64> {
    s.as_deref().and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite())
}

/// Join one dex's metadata with its contexts (they are parallel arrays).
pub fn contracts_for_dex(dex: &str) -> Result<Vec<Contract>> {
    let mut body = serde_json::json!({ "type": "metaAndAssetCtxs" });
    if !dex.is_empty() {
        body["dex"] = dex.into();
    }
    let (meta, ctxs): (Meta, Vec<AssetCtx>) = info(&body).with_context(|| format!("metaAndAssetCtxs for dex {dex:?}"))?;
    Ok(meta
        .universe
        .into_iter()
        .zip(ctxs)
        .map(|(m, c)| {
            let mark = num(&c.mark_px);
            let funding = num(&c.funding);
            let oi = num(&c.open_interest);
            Contract {
                dex: dex.to_string(),
                coin: m.name,
                sz_decimals: m.sz_decimals,
                max_leverage: m.max_leverage,
                delisted: m.is_delisted,
                mark_px: mark,
                funding_hourly: funding,
                funding_annualized_pct: funding.map(|f| f * 24.0 * 365.0 * 100.0),
                open_interest: oi,
                open_interest_usd: oi.zip(mark).map(|(o, p)| o * p),
                day_volume_usd: num(&c.day_ntl_vlm),
            }
        })
        .collect())
}

/// All dexes: the main dex first (`perpDexs` lists it as `null`), then the
/// builder dexes in the API's order.
/// Returns the dex names (main dex as "") and all their contracts.
pub fn fetch_all() -> Result<(Vec<String>, Vec<Contract>)> {
    let dexes: Vec<Option<DexInfo>> = info(&serde_json::json!({ "type": "perpDexs" }))?;
    let names: Vec<String> = dexes.into_iter().map(|d| d.map(|d| d.name).unwrap_or_default()).collect();
    let mut out = Vec::new();
    for name in &names {
        out.extend(contracts_for_dex(name)?);
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    Ok((names, out))
}

pub fn run(out: &Path, top: usize) -> Result<()> {
    let (dex_names, contracts) = fetch_all()?;
    let live: Vec<&Contract> = contracts.iter().filter(|c| !c.delisted).collect();

    let mut dexes: Vec<(&str, usize, f64)> = Vec::new();
    for c in &live {
        let vol = c.day_volume_usd.unwrap_or(0.0);
        match dexes.iter_mut().find(|(d, _, _)| *d == c.dex) {
            Some(e) => {
                e.1 += 1;
                e.2 += vol;
            }
            None => dexes.push((&c.dex, 1, vol)),
        }
    }
    println!(
        "{} dexes listed; {} live perps on the {} with live contracts ({} delisted not shown)",
        dex_names.len(),
        live.len(),
        dexes.len(),
        contracts.len() - live.len()
    );
    for (d, n, vol) in &dexes {
        let label = if d.is_empty() { "main" } else { d };
        println!("  {label:<8} {n:>4} contracts, 24h volume ${:>8.1}M", vol / 1e6);
    }

    let mut by_volume = live.clone();
    by_volume.sort_by(|a, b| b.day_volume_usd.unwrap_or(0.0).total_cmp(&a.day_volume_usd.unwrap_or(0.0)));
    println!(
        "\n{:<16} {:>6} {:>12} {:>10} {:>12} {:>9} {:>4} {:>4}  zone",
        "coin", "dex", "mark", "vol $M", "OI $M", "fund %/y", "lev", "szd"
    );
    let show = |v: Option<f64>, d: usize| v.map_or("n/a".to_string(), |x| format!("{x:.d$}"));
    for c in by_volume.iter().take(top) {
        let zone = c.funding_annualized_pct.map_or("n/a", funding_zone);
        println!(
            "{:<16} {:>6} {:>12} {:>10} {:>12} {:>9} {:>4} {:>4}  {zone}",
            c.coin,
            if c.dex.is_empty() { "main" } else { &c.dex },
            show(c.mark_px, 4),
            show(c.day_volume_usd.map(|v| v / 1e6), 1),
            show(c.open_interest_usd.map(|v| v / 1e6), 1),
            show(c.funding_annualized_pct, 1),
            c.max_leverage,
            c.sz_decimals,
        );
    }

    let doc = serde_json::json!({
        "fetched_at": format_utc(wall_now_ms()),
        "source": "https://api.hyperliquid.xyz/info perpDexs + metaAndAssetCtxs",
        "dexes": dex_names,
        "funding_note": "funding_hourly is per hour; funding_annualized_pct = funding_hourly * 24 * 365 * 100",
        "contracts": contracts,
    });
    write_json(out, &doc)?;
    println!("\nwrote {} contracts to {}", contracts.len(), out.display());
    Ok(())
}
