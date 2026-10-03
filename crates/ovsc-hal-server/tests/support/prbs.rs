//! The 24-bit pseudo-random test signal of `tools/coreaudio-check`
//! (`src/analysis.rs`), copied so that the simulated HAL plays and checks
//! exactly what the macOS end-to-end loopback does.
//!
//! Every value is exact in `f32` and in the 24-bit integers on the wire, so a
//! correct path through the driver, the shared region and the network
//! returns it bit for bit.

const SCALE: f32 = (1u32 << 23) as f32;

/// The integer sent on `channel` at frame `n`: pseudo-random in
/// [-2^22, 2^22), different on every channel.
pub fn code(channel: usize, n: u64) -> i32 {
    let h = splitmix64(n ^ (channel as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    (h >> 41) as i32 - (1 << 22)
}

/// The sample sent on `channel` at frame `n`: `code / 2^23`, at most -6 dBFS.
pub fn sample(channel: usize, n: u64) -> f32 {
    code(channel, n) as f32 / SCALE
}

/// The code a received sample carries, if it is one of ours.
pub fn to_code(x: f32) -> Option<i32> {
    let y = x * SCALE;
    (y.fract() == 0.0 && y.abs() <= SCALE).then_some(y as i32)
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Where a received sample was sent: the frame within `window` frames of
/// `near` whose code on `channel` is `x`'s, closest to `near` first. For
/// failure messages: the received frame's time minus the result is the
/// delay it came back at.
pub fn locate(channel: usize, x: f32, near: i64, window: i64) -> Option<i64> {
    let c = to_code(x)?;
    (0..=window)
        .flat_map(|d| [near - d, near + d])
        .find(|&n| n >= 0 && code(channel, n as u64) == c)
}

#[test]
fn samples_survive_the_driver_and_the_wire() {
    use ovsc_shm::sample::{from_f32, to_f32};
    for n in 0..100_000 {
        let x = sample(5, n);
        assert!(x.abs() <= 0.5);
        assert_eq!(to_code(x), Some(code(5, n)));
        // f32 -> left-justified i32 (WriteMix) -> 24 bits (the wire) -> i32
        // (the RX ring) -> f32 (ReadInput).
        let wire = from_f32(x) >> 8;
        assert_eq!(to_f32(wire << 8).to_bits(), x.to_bits());
    }
    assert_ne!(code(0, 7), code(1, 7));
    assert_eq!(locate(2, sample(2, 1000), 1003, 10), Some(1000));
    assert_eq!(locate(2, 0.3, 1000, 10), None);
}
