//! Sample conversions between device formats, the internal `f32`
//! processing format, and the network's left-justified `i32`.

/// `i32` full scale.
const I32_SCALE: f32 = 2_147_483_648.0;
/// 24-bit full scale.
const I24_SCALE: f32 = 8_388_608.0;
/// 16-bit full scale.
const I16_SCALE: f32 = 32_768.0;

/// A device sample type the bridge can open streams with.
pub trait DeviceSample: cpal::SizedSample + Send + 'static {
    fn to_f32(self) -> f32;
    fn from_f32(v: f32) -> Self;
}

impl DeviceSample for f32 {
    #[inline]
    fn to_f32(self) -> f32 {
        self
    }
    #[inline]
    fn from_f32(v: f32) -> Self {
        // Band-limited resampling can overshoot full scale slightly on
        // clipped material; don't hand drivers values beyond ±1.
        v.clamp(-1.0, 1.0)
    }
}

impl DeviceSample for i32 {
    #[inline]
    fn to_f32(self) -> f32 {
        self as f32 * (1.0 / I32_SCALE)
    }
    #[inline]
    fn from_f32(v: f32) -> Self {
        // `as` saturates.
        (v * I32_SCALE).round() as i32
    }
}

impl DeviceSample for cpal::I24 {
    #[inline]
    fn to_f32(self) -> f32 {
        self.inner() as f32 * (1.0 / I24_SCALE)
    }
    #[inline]
    fn from_f32(v: f32) -> Self {
        let s = ((v * I24_SCALE).round() as i32).clamp(-(1 << 23), (1 << 23) - 1);
        cpal::I24::new_unchecked(s)
    }
}

impl DeviceSample for i16 {
    #[inline]
    fn to_f32(self) -> f32 {
        self as f32 * (1.0 / I16_SCALE)
    }
    #[inline]
    fn from_f32(v: f32) -> Self {
        (v * I16_SCALE).round() as i16
    }
}

/// A network (left-justified `i32`) sample as `f32` in `[-1, 1)`.
#[inline]
pub fn from_network(s: i32) -> f32 {
    s as f32 * (1.0 / I32_SCALE)
}

/// Converts `f32` to left-justified `i32`, rounded to the `bits` the
/// network carries (the transmitter keeps only the top `bits` bits).
#[derive(Clone, Copy, Debug)]
pub struct ToNetwork {
    scale: f32,
    min: f32,
    max: f32,
    shift: u32,
}

impl ToNetwork {
    pub fn new(bits: u32) -> Self {
        let bits = bits.clamp(8, 32);
        let full = (1u64 << (bits - 1)) as f32;
        // `full - 1` rounds to `full` for 32 bits in f32; the saturating
        // cast below takes care of that case.
        ToNetwork { scale: full, min: -full, max: full - 1.0, shift: 32 - bits }
    }

    #[inline]
    pub fn convert(&self, v: f32) -> i32 {
        let s = (v * self.scale).round().clamp(self.min, self.max) as i32;
        s.wrapping_shl(self.shift)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_formats_round_trip() {
        for v in [-1.0f32, -0.5, -1e-3, 0.0, 0.25, 0.999] {
            assert!((i32::from_f32(v).to_f32() - v).abs() < 1e-6, "i32 {v}");
            assert!((cpal::I24::from_f32(v).to_f32() - v).abs() < 1e-6, "i24 {v}");
            assert!((i16::from_f32(v).to_f32() - v).abs() < 1e-4, "i16 {v}");
            assert_eq!(f32::from_f32(v), v);
        }
        assert_eq!(i32::from_f32(1.5), i32::MAX);
        assert_eq!(i16::from_f32(-2.0), i16::MIN);
        assert_eq!(cpal::I24::from_f32(1.0).inner(), (1 << 23) - 1);
        assert_eq!(f32::from_f32(1.2), 1.0);
    }

    #[test]
    fn network_conversion_is_left_justified_and_rounded() {
        let to24 = ToNetwork::new(24);
        assert_eq!(to24.convert(0.5), 0x4000_0000);
        assert_eq!(to24.convert(-1.0), i32::MIN);
        assert_eq!(to24.convert(1.0), 0x7fff_ff00);
        // Exactly representable 24-bit values survive unchanged.
        let s = 0x1234_5600;
        assert_eq!(to24.convert(from_network(s)), s);
        assert_eq!(to24.convert(from_network(s + 0x80)), s + 0x100, "rounds to nearest");
        let to32 = ToNetwork::new(32);
        assert_eq!(to32.convert(1.0), i32::MAX);
        assert_eq!(to32.convert(-1.0), i32::MIN);
        assert_eq!(ToNetwork::new(16).convert(0.25), 0x2000_0000);
    }
}
