//! Small, dependency-free statistics used in reports.

/// Nearest-rank percentile (p in 0..=100) of an unsorted slice. Returns
/// `None` for an empty slice rather than inventing a number.
pub fn percentile<T: Copy + Ord>(values: &[T], p: f64) -> Option<T> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    Some(v[rank.clamp(1, v.len()) - 1])
}

/// Largest peak-to-trough fall of an equity curve, as a positive number.
pub fn max_drawdown(equity: &[f64]) -> f64 {
    let mut peak = f64::NEG_INFINITY;
    let mut worst = 0.0_f64;
    for &e in equity {
        peak = peak.max(e);
        worst = worst.max(peak - e);
    }
    worst
}

/// Share of values strictly above zero. `None` if there are none.
pub fn hit_rate(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().filter(|v| **v > 0.0).count() as f64 / values.len() as f64)
}

/// FNV-1a, 64-bit. Used to fingerprint a run's decisions so two runs can be
/// compared at a glance. Chosen over `DefaultHasher` because its output is
/// fixed by definition, not by the Rust version.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

const DAY_MS: i64 = 86_400_000;

/// P&L per UTC day from (time, equity) samples: the last equity of each day,
/// differenced from a flat start (equity 0 before the first sample). A sample
/// stamped exactly at midnight (an hourly bar closing then) belongs to the day
/// that just ended. Days with no sample carry the equity, so give zero P&L.
pub fn daily_pnl(curve: &[(i64, f64)]) -> Vec<f64> {
    let mut last: std::collections::BTreeMap<i64, f64> = std::collections::BTreeMap::new();
    for (ts, eq) in curve {
        last.insert((ts - 1).div_euclid(DAY_MS), *eq);
    }
    let (Some(&first), Some(&end)) = (last.keys().next(), last.keys().next_back()) else {
        return vec![];
    };
    let (mut prev, mut held, mut out) = (0.0, 0.0, Vec::new());
    for d in first..=end {
        if let Some(eq) = last.get(&d) {
            held = *eq;
        }
        out.push(held - prev);
        prev = held;
    }
    out
}

/// Sharpe ratio of daily P&L, annualised with sqrt(365) (crypto trades every
/// day). `None` with fewer than 20 days or no variation: too short to mean
/// anything, so no number is invented.
pub fn sharpe_daily(daily: &[f64]) -> Option<f64> {
    if daily.len() < 20 {
        return None;
    }
    let n = daily.len() as f64;
    let mean = daily.iter().sum::<f64>() / n;
    let var = daily.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let sd = var.sqrt();
    (sd > 0.0 && sd.is_finite()).then(|| mean / sd * 365f64.sqrt())
}
