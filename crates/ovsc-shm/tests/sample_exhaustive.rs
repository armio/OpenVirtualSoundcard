//! Every 24-bit Dante sample survives the trip through Core Audio's Float32.

use ovsc_shm::sample::{from_f32, to_f32};

/// 2^-23: one step of a 24-bit code in [-1.0, 1.0).
const STEP: f32 = 1.0 / 8_388_608.0;

#[test]
fn every_24_bit_code_round_trips_bit_exactly() {
    for k in -(1i32 << 23)..(1i32 << 23) {
        let s = k << 8;
        let x = to_f32(s);
        // The float is exactly k * 2^-23 ...
        if x.to_bits() != (k as f32 * STEP).to_bits() {
            panic!("code {k}: {x} is not exact");
        }
        // ... and converts back to the same left-justified sample.
        if from_f32(x) != s {
            panic!("code {k}: {x} came back as {}", from_f32(x));
        }
    }
}

#[test]
fn full_scale_and_saturation() {
    assert_eq!(to_f32(i32::MIN), -1.0);
    assert_eq!(to_f32(((1 << 23) - 1) << 8), 1.0 - STEP);
    assert_eq!(from_f32(-1.0), i32::MIN);
    // +1.0 is one step past the largest code and saturates.
    assert_eq!(from_f32(1.0), i32::MAX);
    assert_eq!(from_f32(1.5), i32::MAX);
    assert_eq!(from_f32(-1.5), i32::MIN);
    assert_eq!(from_f32(f32::INFINITY), i32::MAX);
    assert_eq!(from_f32(f32::NEG_INFINITY), i32::MIN);
    assert_eq!(from_f32(f32::MAX), i32::MAX);
    assert_eq!(from_f32(f32::NAN), 0);
    assert_eq!(from_f32(-f32::NAN), 0);
    assert_eq!(from_f32(0.0), 0);
    assert_eq!(from_f32(-0.0), 0);
}

#[test]
fn values_between_codes_truncate_toward_zero() {
    // Half a 24-bit step lands between left-justified codes; the cast
    // truncates toward zero, like the `as` conversion it is.
    assert_eq!(from_f32(0.5 * STEP), 128);
    assert_eq!(from_f32(-0.5 * STEP), -128);
    assert_eq!(from_f32(f32::MIN_POSITIVE), 0);
}
