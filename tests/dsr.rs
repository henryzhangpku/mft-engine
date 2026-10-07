//! The deflated Sharpe ratio against hand-computed values, and its edges.

use mft_engine::dsr::{deflated_sharpe, expected_max_sharpe, norm_cdf, norm_inv, psr, variance, Moments, EULER_GAMMA};
use mft_engine::trials::minute_pnl;

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

#[test]
fn normal_cdf_and_inverse_match_tables() {
    for (z, p) in [
        (0.0, 0.5),
        (1.0, 0.841_344_746_068_542_9),
        (-3.0, 0.001_349_898_031_630_103_5),
        (1.959_963_984_540_054, 0.975),
        (-8.0, 6.106_226_635_438_361e-16),
    ] {
        assert!(close(norm_cdf(z), p, 1e-15_f64.max(p * 1e-12)), "Phi({z}) = {} not {p}", norm_cdf(z));
    }
    for (p, z) in [(0.975, 1.959_963_984_540_053_6), (0.9, 1.281_551_565_544_600_8), (1e-10, -6.361_340_902_404_056), (0.5, 0.0)] {
        assert!(close(norm_inv(p), z, 1e-9), "PhiInv({p}) = {} not {z}", norm_inv(p));
    }
    assert_eq!(norm_inv(0.0), f64::NEG_INFINITY);
    assert_eq!(norm_inv(1.0), f64::INFINITY);
}

/// Eight per-period returns, worked by hand:
///
/// ```text
/// r      = 0.012, -0.004, 0.009, 0.015, -0.007, 0.003, 0.010, -0.002
/// mean   = 0.036 / 8                                   = 0.0045
/// m2     = sum (r - mean)^2 / 8 = 0.000466 / 8           = 5.825e-5
/// m3     = sum (r - mean)^3 / 8 = -5.76e-7 / 8          = -7.2e-8
/// m4     = sum (r - mean)^4 / 8 = 4.11445e-8 / 8         = 5.1430625e-9
/// std    = sqrt(m2 * 8/7)                               = 0.008159131606
/// SR     = 0.0045 / 0.008159131606                      = 0.551529282410
/// skew   = m3 / m2^1.5 = -7.2e-8 / 4.44575e-7            = -0.161952852566
/// kurt   = m4 / m2^2   = 5.1430625e-9 / 3.3930625e-9     = 1.515758256737
/// denom  = sqrt(1 - skew SR + (kurt - 1)/4 SR^2)
///        = sqrt(1 + 0.089321 + 0.039224)                = 1.062329122158
/// PSR(0) = Phi(0.551529 * sqrt(7) / 1.062329) = Phi(1.373594389527) = 0.915216180165
/// ```
const R: [f64; 8] = [0.012, -0.004, 0.009, 0.015, -0.007, 0.003, 0.010, -0.002];

#[test]
fn moments_and_psr_match_hand_computation() {
    let m = Moments::of(&R).unwrap();
    assert_eq!(m.t, 8);
    assert!(close(m.mean, 0.0045, 1e-15));
    assert!(close(m.std, 0.008_159_131_606_453_505, 1e-15));
    assert!(close(m.sharpe(), 0.551_529_282_410_483_8, 1e-12));
    assert!(close(m.skew, -0.161_952_852_565_803_08, 1e-12));
    assert!(close(m.kurtosis, 1.515_758_256_737_092_2, 1e-12));
    let (z, p) = psr(m.sharpe(), 0.0, m.t, m.skew, m.kurtosis).unwrap();
    assert!(close(z, 1.373_594_389_527_308_8, 1e-10));
    assert!(close(p, 0.915_216_180_164_538_9, 1e-10));
}

/// SR0 for N = 10 trials with trial-Sharpe variance V = 0.04, by hand:
///
/// ```text
/// PhiInv(1 - 1/10)        = PhiInv(0.9)       = 1.281551565545
/// PhiInv(1 - 1/(10 e))    = PhiInv(0.963212)  = 1.789241764582
/// SR0 = sqrt(0.04) ((1 - 0.5772157) 1.2815516 + 0.5772157 1.7892418)
///     = 0.2 (0.541814 + 1.032774)                      = 0.314919660269
/// z   = (0.551529 - 0.314920) sqrt(7) / 1.062329         = 0.589280859325
/// DSR = Phi(0.589281)                                    = 0.722163558672
/// ```
/// and the same for N = 2, V = 0.09 (PhiInv(1/2) = 0, so only the second
/// term: SR0 = 0.3 x 0.5772157 x 0.9004526 = 0.155926603284, DSR =
/// 0.837750868418) and N = 100, V = 0.01 (SR0 = 0.253060289320, DSR =
/// 0.771362924611).
#[test]
fn deflated_sharpe_matches_hand_computation() {
    for (n, v, sr0, dsr) in [
        (10, 0.04, 0.314_919_660_269_15, 0.722_163_558_672_203),
        (2, 0.09, 0.155_926_603_284_178_15, 0.837_750_868_418_191_8),
        (100, 0.01, 0.253_060_289_320_168_5, 0.771_362_924_611_049_2),
    ] {
        let got = expected_max_sharpe(n, v).unwrap();
        assert!(close(got, sr0, 1e-9), "SR0(N={n}, V={v}) = {got} not {sr0}");
        let d = deflated_sharpe(&R, got, n).unwrap();
        assert!(close(d.dsr, dsr, 1e-9), "DSR(N={n}) = {} not {dsr}", d.dsr);
        assert!(d.dsr < d.psr_vs_zero, "deflating by more than one trial can only lower it");
    }
    assert!(close(EULER_GAMMA, 0.577_215_664_901_532_9, 0.0));
}

#[test]
fn one_trial_reduces_to_psr_against_zero() {
    // Whatever the variance, a single trial has nothing to deflate.
    for v in [0.0, 0.04, 9.0] {
        assert_eq!(expected_max_sharpe(1, v).unwrap(), 0.0);
    }
    let d = deflated_sharpe(&R, expected_max_sharpe(1, 0.04).unwrap(), 1).unwrap();
    assert_eq!(d.dsr, d.psr_vs_zero);
    assert!(close(d.dsr, 0.915_216_180_164_538_9, 1e-10));
}

#[test]
fn too_few_or_flat_observations_are_errors() {
    assert!(Moments::of(&R[..4]).unwrap_err().to_string().contains("at least 5"));
    assert!(deflated_sharpe(&R[..4], 0.0, 1).is_err());
    assert!(psr(0.5, 0.0, 4, 0.0, 3.0).is_err());
    assert!(Moments::of(&[0.0; 50]).unwrap_err().to_string().contains("do not vary"));
    assert!(Moments::of(&[0.01, f64::NAN, 0.0, 0.02, 0.01]).is_err());
    assert!(expected_max_sharpe(0, 1.0).is_err());
    assert!(expected_max_sharpe(5, -1.0).is_err());
    assert_eq!(variance(&[1.5]), 0.0);
    assert!(close(variance(&[1.0, 2.0, 3.0]), 1.0, 1e-15));
}

#[test]
fn more_trials_raise_the_bar() {
    let a = expected_max_sharpe(5, 1.0).unwrap();
    let b = expected_max_sharpe(50, 1.0).unwrap();
    let c = expected_max_sharpe(500, 1.0).unwrap();
    assert!(0.0 < a && a < b && b < c, "{a} {b} {c}");
}

#[test]
fn minute_pnl_carries_through_quiet_minutes() {
    // Two samples in minute 0 (last wins), none in minute 1, one in minute 2.
    let curve = [(0, 1.0), (30_000, 2.0), (120_000, 5.0)];
    assert_eq!(minute_pnl(&curve), vec![2.0, 0.0, 3.0]);
    assert!(minute_pnl(&[]).is_empty());
}
