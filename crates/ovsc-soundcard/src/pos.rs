//! Fractional positions on the media timeline.

/// A position on the media timeline, in samples: an absolute integer sample
/// index plus a fraction in `[0, 1)`.
///
/// Media sample indices are large (≈ 8·10¹³ at 48 kHz for PTP times near
/// today), far beyond what an `f64` can hold with sub-sample precision, hence
/// the split representation.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MediaPos {
    pub whole: u64,
    pub frac: f64,
}

impl MediaPos {
    pub fn new(whole: u64, frac: f64) -> Self {
        MediaPos { whole, frac: 0.0 }.offset(frac)
    }

    /// The media time `ns` (nanoseconds) as a sample position at `rate` Hz.
    pub fn from_ns(ns: u64, rate: u32) -> Self {
        let x = ns as u128 * rate as u128;
        MediaPos { whole: (x / 1_000_000_000) as u64, frac: (x % 1_000_000_000) as f64 / 1e9 }
    }

    /// This position moved by `d` samples (either direction), saturating at
    /// sample 0.
    pub fn offset(self, d: f64) -> Self {
        let t = self.frac + d;
        let fl = t.floor();
        let whole = if fl >= 0.0 {
            self.whole.saturating_add(fl as u64)
        } else {
            self.whole.saturating_sub((-fl) as u64)
        };
        let mut p = MediaPos { whole, frac: t - fl };
        p.normalize();
        p
    }

    /// `self - other`, in samples.
    pub fn minus(self, other: MediaPos) -> f64 {
        (self.whole as i128 - other.whole as i128) as f64 + (self.frac - other.frac)
    }

    /// Moves forward by `step` samples (`step >= 0`). Cheap enough to call
    /// once per frame.
    #[inline]
    pub fn advance(&mut self, step: f64) {
        self.frac += step;
        if self.frac >= 1.0 {
            let k = self.frac.floor();
            self.whole += k as u64;
            self.frac -= k;
            self.normalize();
        }
    }

    #[inline]
    fn normalize(&mut self) {
        // Rounding can leave exactly 1.0 behind (e.g. 1 - 1e-17 rounds up).
        if self.frac >= 1.0 {
            self.whole += 1;
            self.frac -= 1.0;
        }
        if self.frac < 0.0 {
            self.frac = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_ns_keeps_sub_sample_precision() {
        let ns = 1_700_000_000_123_456_789u64;
        let p = MediaPos::from_ns(ns, 48_000);
        assert_eq!(p.whole, 81_600_000_005_925);
        assert!((p.frac - 0.925_872).abs() < 1e-9, "{}", p.frac);
    }

    #[test]
    fn offset_and_minus_round_trip() {
        let p = MediaPos::new(1_000_000_000_000, 0.25);
        for d in [-1234.75, -0.5, -0.25, 0.0, 0.75, 1.0, 98765.125] {
            let q = p.offset(d);
            assert!((0.0..1.0).contains(&q.frac));
            assert!((q.minus(p) - d).abs() < 1e-9, "{d}: {}", q.minus(p));
        }
        assert_eq!(MediaPos::new(3, 0.0).offset(-10.0), MediaPos::new(0, 0.0));
    }

    #[test]
    fn advance_carries_into_whole() {
        let mut p = MediaPos::new(10, 0.5);
        for _ in 0..1000 {
            p.advance(1.000_05);
        }
        assert_eq!(p.whole, 1010);
        assert!((p.frac - 0.55).abs() < 1e-9);
    }
}
