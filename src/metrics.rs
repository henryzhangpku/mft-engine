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
