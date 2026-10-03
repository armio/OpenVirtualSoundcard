//! Band-limited interpolation: a Kaiser-windowed sinc evaluated at arbitrary
//! fractional positions.
//!
//! Both directions of the bridge resample by ratios within ±1000 ppm of 1,
//! and the ratio changes a little every callback. A fixed-ratio polyphase
//! resampler doesn't fit that, but a *fractional-delay interpolator* does:
//! every output sample is the band-limited reconstruction of the input at an
//! arbitrary real-valued position, so the ratio can change from one sample to
//! the next without any state to flush.
//!
//! The kernel is a 64-tap sinc (cutoff at 0.98 × Nyquist) under a Kaiser
//! window (β = 10.5), tabulated at 1024 sub-sample phases with linear
//! interpolation between neighbouring phases. Each phase is normalised to
//! unity DC gain, so sweeping slowly through the phases (which is what a ratio
//! of 1 ± a few ppm does) cannot modulate the level. Measured against the
//! exact value of a sine at random fractional positions, the error stays
//! below −100 dB from DC to 20 kHz at 48 kHz (see the tests), comparable to
//! good dedicated sample-rate converters. The price is 32 samples of
//! look-ahead (0.67 ms at 48 kHz), which the bridge includes in its latency.

use std::f64::consts::PI;
use std::sync::OnceLock;

/// Taps on each side of the interpolation point (and the look-ahead, in
/// samples, the interpolator needs).
pub const HALF: usize = 32;
/// Total taps per output sample.
pub const TAPS: usize = 2 * HALF;
/// Sub-sample phases in the coefficient table.
const PHASES: usize = 1024;
/// Kaiser window shape: ≈ 105 dB side-lobe attenuation.
const BETA: f64 = 10.5;
/// Cutoff as a fraction of the Nyquist frequency.
const CUTOFF: f64 = 0.98;

/// The shared coefficient table.
pub struct Kernel {
    /// `PHASES + 1` rows of `TAPS` coefficients; row `p` interpolates at
    /// fractional position `p / PHASES`.
    table: Box<[f32]>,
}

impl Kernel {
    /// The process-wide kernel. The first call builds the table (≈ 260 kB),
    /// so call it outside real-time threads before streams start.
    pub fn get() -> &'static Kernel {
        static KERNEL: OnceLock<Kernel> = OnceLock::new();
        KERNEL.get_or_init(Kernel::build)
    }

    fn build() -> Kernel {
        let mut table = vec![0f32; (PHASES + 1) * TAPS].into_boxed_slice();
        let i0_beta = bessel_i0(BETA);
        let mut row = [0f64; TAPS];
        for (p, out) in table.chunks_exact_mut(TAPS).enumerate() {
            let frac = p as f64 / PHASES as f64;
            for (j, c) in row.iter_mut().enumerate() {
                // Distance from the interpolation point to tap j.
                let t = frac + (HALF - 1) as f64 - j as f64;
                let u = t / HALF as f64;
                let window = if u.abs() >= 1.0 {
                    0.0
                } else {
                    bessel_i0(BETA * (1.0 - u * u).sqrt()) / i0_beta
                };
                let x = CUTOFF * t;
                let sinc = if x.abs() < 1e-12 { 1.0 } else { (PI * x).sin() / (PI * x) };
                *c = CUTOFF * sinc * window;
            }
            let sum: f64 = row.iter().sum();
            for (o, c) in out.iter_mut().zip(&row) {
                *o = (c / sum) as f32;
            }
        }
        Kernel { table }
    }

    /// Computes the coefficients for interpolating at `whole + frac`, where
    /// `frac` is in `[0, 1)`. Tap `j` multiplies the sample at index
    /// `whole - (HALF - 1) + j`.
    #[inline]
    pub fn coefficients(&self, frac: f64, out: &mut [f32; TAPS]) {
        let x = frac.clamp(0.0, 1.0) * PHASES as f64;
        let p = (x as usize).min(PHASES - 1);
        let w = (x - p as f64) as f32;
        let r0 = &self.table[p * TAPS..(p + 1) * TAPS];
        let r1 = &self.table[(p + 1) * TAPS..(p + 2) * TAPS];
        for ((o, &a), &b) in out.iter_mut().zip(r0).zip(r1) {
            *o = a + w * (b - a);
        }
    }
}

/// Dot product of the coefficients with `TAPS` samples starting at `x[0]`.
/// Eight independent accumulators let the compiler vectorise the loop.
#[inline]
pub fn dot(coeffs: &[f32; TAPS], x: &[f32]) -> f32 {
    let x = &x[..TAPS];
    let mut acc = [0f32; 8];
    for (c, s) in coeffs.chunks_exact(8).zip(x.chunks_exact(8)) {
        for ((a, &c), &s) in acc.iter_mut().zip(c).zip(s) {
            *a += c * s;
        }
    }
    acc.iter().sum()
}

/// Modified Bessel function of the first kind, order 0 (power series).
fn bessel_i0(x: f64) -> f64 {
    let q = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..200 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interpolate(k: &Kernel, x: &[f32], pos: f64) -> f64 {
        let whole = pos.floor();
        let mut c = [0f32; TAPS];
        k.coefficients(pos - whole, &mut c);
        let start = whole as usize + 1 - HALF;
        dot(&c, &x[start..]) as f64
    }

    #[test]
    fn integer_positions_reproduce_band_limited_signals() {
        let k = Kernel::get();
        let signal = |t: f64| {
            0.3 * (0.05 * t).sin() + 0.3 * (0.9 * t + 1.0).sin() + 0.3 * (2.4 * t + 2.0).sin()
        };
        let x: Vec<f32> = (0..400).map(|i| signal(i as f64) as f32).collect();
        for (i, &s) in x.iter().enumerate().take(400 - HALF).skip(HALF) {
            let y = interpolate(k, &x, i as f64);
            assert!((y - s as f64).abs() < 1e-5, "sample {i}: {y} vs {s}");
        }
    }

    #[test]
    fn every_phase_has_unity_dc_gain() {
        let k = Kernel::get();
        let mut c = [0f32; TAPS];
        for p in 0..=PHASES * 4 {
            k.coefficients(p as f64 / (PHASES * 4) as f64, &mut c);
            let sum: f64 = c.iter().map(|&v| v as f64).sum();
            assert!((sum - 1.0).abs() < 1e-5, "phase {p}: dc gain {sum}");
        }
    }

    #[test]
    fn sines_are_reconstructed_below_minus_100_db() {
        let k = Kernel::get();
        let fs = 48_000.0;
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for f in [50.0, 997.0, 5_000.0, 10_000.0, 15_000.0, 18_000.0, 20_000.0] {
            let w = 2.0 * PI * f / fs;
            let x: Vec<f32> = (0..2048).map(|i| (w * i as f64 + 0.3).sin() as f32).collect();
            let mut max_err = 0f64;
            for _ in 0..2000 {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let pos = 100.0 + (seed >> 11) as f64 / (1u64 << 53) as f64 * 1800.0;
                let err = (interpolate(k, &x, pos) - (w * pos + 0.3).sin()).abs();
                max_err = max_err.max(err);
            }
            let db = 20.0 * max_err.log10();
            assert!(db < -100.0, "{f} Hz: max error {db:.1} dB");
        }
    }

    #[test]
    fn dot_matches_naive_sum() {
        let c: [f32; TAPS] = std::array::from_fn(|i| (i as f32 - 20.0) * 0.01);
        let x: Vec<f32> = (0..TAPS + 3).map(|i| (i as f32).sin()).collect();
        let naive: f32 = c.iter().zip(&x[3..]).map(|(a, b)| a * b).sum();
        assert!((dot(&c, &x[3..]) - naive).abs() < 1e-5);
    }
}
