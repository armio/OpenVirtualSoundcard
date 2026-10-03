//! Conversion between ring samples and Core Audio's Float32.
//!
//! Ring samples are i32, left-justified: a 24-bit Dante sample `k` is stored
//! as `k << 8`. Full scale maps to ±1.0 by a power of two, so every 24-bit
//! code converts to f32 exactly (`k · 2^-23` has at most 24 significant bits)
//! and comes back bit-exactly.

#![forbid(unsafe_code)]
#![deny(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

/// 2^31: i32 full scale.
const FULL_SCALE: f32 = 2_147_483_648.0;

/// Converts a left-justified sample to a float in [-1.0, 1.0).
#[inline]
pub fn to_f32(s: i32) -> f32 {
    s as f32 * (1.0 / FULL_SCALE)
}

/// Converts a float to a left-justified sample. Values outside [-1.0, 1.0)
/// saturate to `i32::MIN`/`i32::MAX`, and NaN gives 0 (the semantics of an
/// `as` cast).
#[inline]
pub fn from_f32(x: f32) -> i32 {
    (x * FULL_SCALE) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_scale_and_specials() {
        assert_eq!(to_f32(i32::MIN), -1.0);
        assert_eq!(to_f32(0), 0.0);
        assert_eq!(to_f32(1 << 30), 0.5);
        assert_eq!(from_f32(-1.0), i32::MIN);
        assert_eq!(from_f32(1.0), i32::MAX);
        assert_eq!(from_f32(2.5), i32::MAX);
        assert_eq!(from_f32(-7.0), i32::MIN);
        assert_eq!(from_f32(f32::INFINITY), i32::MAX);
        assert_eq!(from_f32(f32::NEG_INFINITY), i32::MIN);
        assert_eq!(from_f32(f32::NAN), 0);
        assert_eq!(from_f32(-0.0), 0);
    }
}
