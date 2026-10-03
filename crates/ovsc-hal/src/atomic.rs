//! Atomic read-modify-write without `fetch_update`, which newer compilers
//! deprecate in favour of `try_update`, a name the minimum supported Rust
//! does not have.

use std::sync::atomic::{AtomicU32, Ordering};

/// Applies `f` to the value until it returns `None` or the store succeeds:
/// `Ok(previous)` if stored, `Err(current)` if `f` declined. Same contract
/// as `AtomicU32::fetch_update`.
pub(crate) fn update_u32(
    a: &AtomicU32,
    set: Ordering,
    fetch: Ordering,
    mut f: impl FnMut(u32) -> Option<u32>,
) -> Result<u32, u32> {
    let mut prev = a.load(fetch);
    while let Some(next) = f(prev) {
        match a.compare_exchange_weak(prev, next, set, fetch) {
            Ok(p) => return Ok(p),
            Err(p) => prev = p,
        }
    }
    Err(prev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_fetch_update() {
        let a = AtomicU32::new(1);
        let dec = |n: u32| n.checked_sub(1);
        assert_eq!(update_u32(&a, Ordering::SeqCst, Ordering::SeqCst, dec), Ok(1));
        assert_eq!(update_u32(&a, Ordering::SeqCst, Ordering::SeqCst, dec), Err(0));
        assert_eq!(a.load(Ordering::SeqCst), 0);
        let inc = |n: u32| Some(n.saturating_add(1));
        assert_eq!(update_u32(&a, Ordering::SeqCst, Ordering::SeqCst, inc), Ok(0));
        assert_eq!(a.load(Ordering::SeqCst), 1);
    }
}
