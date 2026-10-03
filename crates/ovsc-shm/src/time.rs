//! Time conversions and the clock types shared by the daemon and the driver.
//!
//! Host time in OpenVirtualSoundcard is always nanoseconds on the system-wide monotonic
//! clock (`CLOCK_UPTIME_RAW` on macOS, `CLOCK_MONOTONIC` on Linux). Mach
//! ticks only appear at the Core Audio boundary, through [`Timebase`].

#![forbid(unsafe_code)]

/// Nanoseconds per second.
pub const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Converts a media time in nanoseconds to a sample index at `sample_rate`.
#[inline]
pub fn ns_to_samples(ns: u64, sample_rate: u32) -> u64 {
    ((ns as u128 * sample_rate as u128) / NANOS_PER_SEC as u128) as u64
}

/// Converts a sample index at `sample_rate` to media time in nanoseconds.
#[inline]
pub fn samples_to_ns(samples: u64, sample_rate: u32) -> u64 {
    ((samples as u128 * NANOS_PER_SEC as u128) / sample_rate as u128) as u64
}

/// A linear mapping from the local monotonic clock to media (network) time:
/// `media = media_ref_ns + (local - local_ref_ns) * rate`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockSnapshot {
    /// Local monotonic time (host nanoseconds) at the reference point.
    pub local_ref_ns: u64,
    /// Media time at the reference point.
    pub media_ref_ns: u64,
    /// Media nanoseconds elapsed per local nanosecond (close to 1.0).
    pub rate: f64,
}

impl ClockSnapshot {
    /// Media time corresponding to the local time `local_ns`.
    #[inline]
    pub fn media_ns_at(&self, local_ns: u64) -> u64 {
        let dl = local_ns as i128 - self.local_ref_ns as i128;
        let dm = (dl as f64 * self.rate) as i128;
        clamp_u64((self.media_ref_ns as i128).saturating_add(dm))
    }

    /// Local time at which media time will be `media_ns`.
    #[inline]
    pub fn local_ns_at(&self, media_ns: u64) -> u64 {
        let dm = media_ns as i128 - self.media_ref_ns as i128;
        let dl = (dm as f64 / self.rate) as i128;
        clamp_u64((self.local_ref_ns as i128).saturating_add(dl))
    }

    /// Frequency offset of the media clock relative to the local clock, in
    /// parts per billion.
    pub fn freq_offset_ppb(&self) -> f64 {
        (self.rate - 1.0) * 1e9
    }
}

/// Coarse synchronisation state, for status displays and heartbeats.
///
/// The discriminants are part of the shared-memory layout (the low byte of
/// the clock block's state word) and must not change.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClockState {
    /// No time source has been seen yet.
    #[default]
    Unlocked = 0,
    /// A master was selected and the servo is converging.
    Locking = 1,
    /// The servo is tracking the master.
    Locked = 2,
    /// The clock is free-running from a local source.
    FreeRunning = 3,
}

impl ClockState {
    /// The state's code in shared memory.
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// The state with code `v`, or `None` for an unknown code.
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(ClockState::Unlocked),
            1 => Some(ClockState::Locking),
            2 => Some(ClockState::Locked),
            3 => Some(ClockState::FreeRunning),
            _ => None,
        }
    }
}

/// The ratio between Mach absolute-time ticks and nanoseconds
/// (`mach_timebase_info`): `ns = ticks * numer / denom`.
///
/// It is 125/3 on Apple silicon and 1/1 on Intel Macs. A zero `numer` or
/// `denom` never comes from the system; the conversions treat it as 1 rather
/// than dividing by zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timebase {
    pub numer: u32,
    pub denom: u32,
}

impl Timebase {
    /// One tick per nanosecond (Intel Macs, and every non-Apple host).
    pub const NANOS: Timebase = Timebase { numer: 1, denom: 1 };

    /// Converts ticks to nanoseconds, rounding down. Saturates at `u64::MAX`.
    #[inline]
    pub fn ticks_to_ns(self, ticks: u64) -> u64 {
        let numer = self.numer.max(1) as u128;
        let denom = self.denom.max(1) as u128;
        saturate(ticks as u128 * numer / denom)
    }

    /// Converts nanoseconds to ticks, rounding up, so that converting the
    /// result back never gives a time before `ns`. Saturates at `u64::MAX`.
    #[inline]
    pub fn ns_to_ticks_ceil(self, ns: u64) -> u64 {
        let numer = self.numer.max(1) as u128;
        let denom = self.denom.max(1) as u128;
        saturate((ns as u128 * denom).div_ceil(numer))
    }
}

#[inline]
fn saturate(v: u128) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

/// Clamps to the u64 range. Snapshots may come from another process, so a
/// garbage rate must give a garbage time, not an overflow.
#[inline]
fn clamp_u64(v: i128) -> u64 {
    v.clamp(0, u64::MAX as i128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_conversions_round_trip() {
        let ns = 1_700_000_000 * NANOS_PER_SEC;
        assert_eq!(ns_to_samples(ns, 48_000), 1_700_000_000 * 48_000);
        assert_eq!(samples_to_ns(48_000, 48_000), NANOS_PER_SEC);
        assert_eq!(ns_to_samples(20_833, 48_000), 0);
        assert_eq!(ns_to_samples(20_834, 48_000), 1);
    }

    #[test]
    fn snapshot_maps_both_ways() {
        // 1 + 2^-10 keeps the arithmetic exact.
        let s = ClockSnapshot { local_ref_ns: 1_000, media_ref_ns: 5_000_000, rate: 1.0009765625 };
        assert_eq!(s.media_ns_at(1_000), 5_000_000);
        assert_eq!(s.media_ns_at(1_025_000), 6_025_000);
        assert_eq!(s.media_ns_at(0), 4_999_000);
        assert_eq!(s.local_ns_at(6_025_000), 1_025_000);
        assert_eq!(s.freq_offset_ppb(), 976_562.5);
        // Times before media zero clamp instead of wrapping.
        let early = ClockSnapshot { local_ref_ns: 10_000, media_ref_ns: 0, rate: 1.0 };
        assert_eq!(early.media_ns_at(0), 0);
        assert_eq!(early.local_ns_at(0), 10_000);
        // Garbage rates saturate instead of overflowing.
        for rate in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN, 1e300, -1e300, 0.0] {
            let s = ClockSnapshot { local_ref_ns: 1, media_ref_ns: u64::MAX - 1, rate };
            s.media_ns_at(u64::MAX);
            s.media_ns_at(0);
            s.local_ns_at(u64::MAX);
            s.local_ns_at(0);
        }
        let fast = ClockSnapshot { local_ref_ns: 0, media_ref_ns: u64::MAX - 1, rate: 2.0 };
        assert_eq!(fast.media_ns_at(u64::MAX), u64::MAX);
        let slow = ClockSnapshot { local_ref_ns: 0, media_ref_ns: 0, rate: 0.0 };
        assert_eq!(slow.local_ns_at(1), u64::MAX);
    }

    #[test]
    fn clock_state_codes() {
        for s in
            [ClockState::Unlocked, ClockState::Locking, ClockState::Locked, ClockState::FreeRunning]
        {
            assert_eq!(ClockState::from_u8(s.to_u8()), Some(s));
        }
        assert_eq!(ClockState::FreeRunning.to_u8(), 3);
        assert_eq!(ClockState::from_u8(4), None);
        assert_eq!(ClockState::default(), ClockState::Unlocked);
    }

    #[test]
    fn timebase_conversions() {
        let arm = Timebase { numer: 125, denom: 3 };
        assert_eq!(arm.ticks_to_ns(3), 125);
        assert_eq!(arm.ticks_to_ns(1), 41);
        assert_eq!(arm.ns_to_ticks_ceil(125), 3);
        assert_eq!(arm.ns_to_ticks_ceil(126), 4);
        assert_eq!(arm.ns_to_ticks_ceil(0), 0);
        for ns in 0..10_000u64 {
            let t = arm.ns_to_ticks_ceil(ns);
            assert!(arm.ticks_to_ns(t) >= ns);
            assert!(t == 0 || arm.ticks_to_ns(t - 1) < ns);
        }
        assert_eq!(Timebase::NANOS.ticks_to_ns(12_345), 12_345);
        assert_eq!(Timebase::NANOS.ns_to_ticks_ceil(12_345), 12_345);
        assert_eq!(arm.ticks_to_ns(u64::MAX), u64::MAX);
        assert_eq!(Timebase { numer: 0, denom: 0 }.ticks_to_ns(7), 7);
    }
}
