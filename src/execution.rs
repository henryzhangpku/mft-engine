//! Paper execution: a fill model and the book-keeping that follows a fill.
//!
//! There is no order router in this crate. Nothing here, or anywhere else,
//! signs a request, holds a key, or talks to an exchange's order endpoint.
//! A "fill" is a struct we compute; it never leaves the process.
//!
//! Fill model (stated in the README):
//! * Every order is a taker order and fills in full at once.
//! * Reference price: the touch (ask for a buy, bid for a sell) when we have a
//!   fresh top of book, otherwise the last bar close.
//! * Slippage: a further `slippage_bps` against us on top of the reference.
//! * Fee: `taker_fee_bps` of the filled notional (Hyperliquid's base taker
//!   tier is 4.5 bps).
//!
//! PnL is cash-based: start with zero cash, pay `qty * px + fee` on each fill,
//! and mark open positions at the latest price. Equity = cash + sum(qty * mark).

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FillModel {
    pub taker_fee_bps: f64,
    pub slippage_bps: f64,
}

impl Default for FillModel {
    fn default() -> Self {
        Self {
            taker_fee_bps: 4.5,
            slippage_bps: 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Fill {
    pub coin: String,
    pub ts: i64,
    /// Signed quantity in coin units.
    pub qty: f64,
    /// Price paid, after slippage.
    pub px: f64,
    /// The reference price before slippage, kept so slippage cost is visible.
    pub ref_px: f64,
    pub fee: f64,
}

impl FillModel {
    /// Turn an approved order into a fill. Pure arithmetic, no I/O.
    pub fn fill(&self, coin: &str, ts: i64, qty: f64, ref_px: f64) -> Fill {
        let slip = self.slippage_bps / 10_000.0;
        let px = if qty > 0.0 { ref_px * (1.0 + slip) } else { ref_px * (1.0 - slip) };
        let fee = (qty * px).abs() * self.taker_fee_bps / 10_000.0;
        Fill {
            coin: coin.to_string(),
            ts,
            qty,
            px,
            ref_px,
            fee,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct Position {
    qty: f64,
    /// Cash flow of the current round trip (flat to flat), used for hit rate.
    trip_cash: f64,
}

/// Positions, cash and round-trip results.
#[derive(Debug, Default, Clone)]
pub struct Portfolio {
    positions: BTreeMap<String, Position>,
    cash: f64,
    pub fees_paid: f64,
    pub slippage_paid: f64,
    pub traded_notional: f64,
    pub fills: u64,
    /// PnL of each completed round trip, in order.
    pub round_trips: Vec<f64>,
}

impl Portfolio {
    pub fn position(&self, coin: &str) -> f64 {
        self.positions.get(coin).map_or(0.0, |p| p.qty)
    }

    /// Cash plus open positions marked at `marks`. A position with no mark
    /// is valued at zero, which only happens before the first price arrives.
    pub fn equity(&self, marks: &BTreeMap<String, f64>) -> f64 {
        let open: f64 = self
            .positions
            .iter()
            .map(|(coin, p)| p.qty * marks.get(coin).copied().unwrap_or(0.0))
            .sum();
        self.cash + open
    }

    pub fn apply(&mut self, fill: &Fill) {
        self.cash -= fill.qty * fill.px + fill.fee;
        self.fees_paid += fill.fee;
        self.slippage_paid += (fill.px - fill.ref_px).abs() * fill.qty.abs();
        self.traded_notional += (fill.qty * fill.px).abs();
        self.fills += 1;

        let pos = self.positions.entry(fill.coin.clone()).or_default();
        let before = pos.qty;
        let after = before + fill.qty;

        // If the fill crosses zero, split it: the part that closes the old
        // trip, and the part that opens a new one. Fee is split pro rata.
        let crosses = before != 0.0 && after * before < 0.0;
        let closing_qty = if crosses { -before } else { fill.qty };
        let share = if fill.qty != 0.0 { closing_qty / fill.qty } else { 1.0 };
        pos.trip_cash -= closing_qty * fill.px + fill.fee * share;

        let closed = (before != 0.0 && after.abs() < 1e-12) || crosses;
        if closed {
            self.round_trips.push(pos.trip_cash);
            pos.trip_cash = 0.0;
        }
        if crosses {
            let opening_qty = fill.qty - closing_qty;
            pos.trip_cash -= opening_qty * fill.px + fill.fee * (1.0 - share);
        }
        // Snap float dust to exactly zero so "flat" means flat.
        pos.qty = if after.abs() < 1e-12 { 0.0 } else { after };
    }
}
