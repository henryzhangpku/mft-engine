//! The deflated Sharpe ratio (Bailey and López de Prado, 2014).
//!
//! A Sharpe ratio measured on one backtest says little once many strategies
//! have been tried: the best of N noise strategies has a positive Sharpe by
//! construction. The DSR asks how likely it is that the true Sharpe ratio
//! beats the Sharpe ratio the best of N worthless trials would be expected to
//! show, given the sample length and the shape of the returns.
//!
//! With per-period returns r_1..r_T (here: P&L per minute or per day):
//!
//! * `SR = mean / std`, std with T - 1 (not annualised);
//! * `skew = m3 / m2^1.5` and `kurt = m4 / m2^2` from the central moments
//!   m_k = mean((r - mean)^k) (kurt is not excess: 3 for a normal);
//! * the probabilistic Sharpe ratio against a benchmark SR0,
//!   `PSR(SR0) = Phi( (SR - SR0) sqrt(T - 1) / sqrt(1 - skew SR + (kurt - 1)/4 SR^2) )`;
//! * the benchmark is the expected maximum Sharpe of N independent trials
//!   whose Sharpe ratios have variance V and true mean zero,
//!   `SR0 = sqrt(V) ((1 - g) PhiInv(1 - 1/N) + g PhiInv(1 - 1/(N e)))`,
//!   g the Euler-Mascheroni constant;
//! * `DSR = PSR(SR0)`.
//!
//! With N = 1 there is nothing to deflate: SR0 = 0 and the DSR is the PSR
//! against zero. (The formula itself diverges at N = 1, since
//! PhiInv(0) = -infinity; the expected maximum of one trial is its mean, 0.)
//!
//! No statistics crate: the normal CDF is computed from `erfc` (a series
//! below 3, a continued fraction above) and its inverse by Acklam's rational
//! approximation polished with Halley steps, both to near double precision.

use anyhow::{bail, Result};
use serde::Serialize;
use std::f64::consts::{E, PI};

pub const EULER_GAMMA: f64 = 0.577_215_664_901_532_9;

/// Fewer observations than this is an error: skewness and kurtosis are not
/// estimable and `sqrt(T - 1)` means nothing.
pub const MIN_OBSERVATIONS: usize = 5;

/// Complementary error function, to about 1e-15 relative.
pub fn erfc(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x < 0.0 {
        return 2.0 - erfc(-x);
    }
    if x < 3.0 {
        // erf(x) = 2/sqrt(pi) e^{-x^2} sum_n 2^n x^{2n+1} / (1 3 5 ... (2n+1)):
        // every term positive, so no cancellation inside the sum.
        let x2 = x * x;
        let (mut term, mut sum, mut n) = (x, x, 0.0);
        loop {
            n += 1.0;
            term *= 2.0 * x2 / (2.0 * n + 1.0);
            sum += term;
            if term <= sum * 1e-17 {
                break;
            }
        }
        1.0 - 2.0 / PI.sqrt() * (-x2).exp() * sum
    } else {
        // erfc(x) = e^{-x^2}/sqrt(pi) / (x + (1/2)/(x + 1/(x + (3/2)/(x + ...)))),
        // evaluated from the tail.
        let mut f = x;
        for k in (1..=80).rev() {
            f = x + (k as f64 / 2.0) / f;
        }
        (-x * x).exp() / (PI.sqrt() * f)
    }
}

/// Standard normal CDF.
pub fn norm_cdf(z: f64) -> f64 {
    0.5 * erfc(-z / std::f64::consts::SQRT_2)
}

/// Inverse standard normal CDF, for p in (0, 1). 0 and 1 give infinities.
#[allow(clippy::excessive_precision)] // Acklam's published coefficients, verbatim
pub fn norm_inv(p: f64) -> f64 {
    if p.is_nan() || !(0.0..=1.0).contains(&p) {
        return f64::NAN;
    }
    if p == 0.0 {
        return f64::NEG_INFINITY;
    }
    if p == 1.0 {
        return f64::INFINITY;
    }
    // Acklam's rational approximation (relative error about 1e-9) ...
    const A: [f64; 6] = [-3.969683028665376e+01, 2.209460984245205e+02, -2.759285104469687e+02, 1.383577518672690e+02, -3.066479806614716e+01, 2.506628277459239e+00];
    const B: [f64; 5] = [-5.447609879822406e+01, 1.615858368580409e+02, -1.556989798598866e+02, 6.680131188771972e+01, -1.328068155288572e+01];
    const C: [f64; 6] = [-7.784894002430293e-03, -3.223964580411365e-01, -2.400758277161838e+00, -2.549732539343734e+00, 4.374664141464968e+00, 2.938163982698783e+00];
    const D: [f64; 4] = [7.784695709041462e-03, 3.224671290700398e-01, 2.445134137142996e+00, 3.754408661907416e+00];
    const P_LOW: f64 = 0.02425;
    let tail = |q: f64| {
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5]) / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    };
    let mut x = if p < P_LOW {
        tail((-2.0 * p.ln()).sqrt())
    } else if p <= 1.0 - P_LOW {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        -tail((-2.0 * (1.0 - p).ln()).sqrt())
    };
    // ... then Halley steps on Phi(x) = p, to full precision.
    for _ in 0..2 {
        let e = norm_cdf(x) - p;
        let u = e * (2.0 * PI).sqrt() * (x * x / 2.0).exp();
        x -= u / (1.0 + x * u / 2.0);
    }
    x
}

/// The sample statistics the PSR needs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Moments {
    pub t: usize,
    pub mean: f64,
    /// Standard deviation with T - 1.
    pub std: f64,
    pub skew: f64,
    /// Not excess: 3 for a normal distribution.
    pub kurtosis: f64,
}

impl Moments {
    pub fn of(returns: &[f64]) -> Result<Moments> {
        let t = returns.len();
        if t < MIN_OBSERVATIONS {
            bail!("{t} observations; the PSR needs at least {MIN_OBSERVATIONS}");
        }
        if returns.iter().any(|r| !r.is_finite()) {
            bail!("non-finite return");
        }
        let n = t as f64;
        let mean = returns.iter().sum::<f64>() / n;
        let central = |k: i32| returns.iter().map(|r| (r - mean).powi(k)).sum::<f64>() / n;
        let (m2, m3, m4) = (central(2), central(3), central(4));
        if m2 <= 0.0 {
            bail!("returns do not vary; the Sharpe ratio is undefined");
        }
        Ok(Moments {
            t,
            mean,
            std: (m2 * n / (n - 1.0)).sqrt(),
            skew: m3 / m2.powf(1.5),
            kurtosis: m4 / (m2 * m2),
        })
    }

    /// Per-period Sharpe ratio, not annualised.
    pub fn sharpe(&self) -> f64 {
        self.mean / self.std
    }
}

/// The probabilistic Sharpe ratio: P(true SR > `sr0`), with `sr` and `sr0`
/// in the same per-period units. Returns (z, PSR).
pub fn psr(sr: f64, sr0: f64, t: usize, skew: f64, kurtosis: f64) -> Result<(f64, f64)> {
    if t < MIN_OBSERVATIONS {
        bail!("{t} observations; the PSR needs at least {MIN_OBSERVATIONS}");
    }
    let var = 1.0 - skew * sr + (kurtosis - 1.0) / 4.0 * sr * sr;
    if var.is_nan() || var <= 0.0 || !var.is_finite() {
        bail!("the Sharpe ratio's variance term is not positive ({var})");
    }
    let z = (sr - sr0) * ((t - 1) as f64).sqrt() / var.sqrt();
    Ok((z, norm_cdf(z)))
}

/// Expected maximum Sharpe ratio of `n_trials` independent trials with true
/// Sharpe zero and cross-trial variance `var_trial_sr` (False Strategy
/// Theorem approximation). Zero for a single trial.
pub fn expected_max_sharpe(n_trials: usize, var_trial_sr: f64) -> Result<f64> {
    if n_trials == 0 {
        bail!("no trials");
    }
    if var_trial_sr.is_nan() || var_trial_sr < 0.0 {
        bail!("trial Sharpe variance must be non-negative");
    }
    if n_trials == 1 {
        return Ok(0.0);
    }
    let n = n_trials as f64;
    let g = EULER_GAMMA;
    Ok(var_trial_sr.sqrt() * ((1.0 - g) * norm_inv(1.0 - 1.0 / n) + g * norm_inv(1.0 - 1.0 / (n * E))))
}

/// Sample variance (n - 1) of trial Sharpe ratios; 0 for a single trial.
pub fn variance(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Deflated {
    pub moments: Moments,
    /// Per-period Sharpe ratio of the returns.
    pub sr: f64,
    /// The benchmark, per period.
    pub sr0: f64,
    pub n_trials: usize,
    pub z: f64,
    pub psr_vs_zero: f64,
    pub dsr: f64,
}

/// The DSR of `returns` against `sr0` (per period, e.g. from
/// `expected_max_sharpe`, converted to the returns' period).
pub fn deflated_sharpe(returns: &[f64], sr0: f64, n_trials: usize) -> Result<Deflated> {
    let m = Moments::of(returns)?;
    let sr = m.sharpe();
    let (_, psr0) = psr(sr, 0.0, m.t, m.skew, m.kurtosis)?;
    let (z, dsr) = psr(sr, sr0, m.t, m.skew, m.kurtosis)?;
    Ok(Deflated { moments: m, sr, sr0, n_trials, z, psr_vs_zero: psr0, dsr })
}
