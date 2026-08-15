//! Timing entropy behind a port: domain code that randomizes a delay takes
//! draws through `Draw`, so the policy around them stays testable and no
//! domain file reads ambient entropy. This is **not** where cryptographic
//! randomness comes from — sealing and key generation take a
//! `CryptoRngCore` explicitly at the call site (`zink-protocol`'s own
//! testable seam); routing those through a scriptable port would be a
//! footgun, not a feature.

/// Draw a uniform number below `bound` — randomness enough for timing
/// decisions, never for secrets. `bound` must be ≥ 1; a zero bound is a
/// caller bug and may panic.
pub trait Draw {
    fn draw(&self, bound: u64) -> u64;
}

/// The draw double: yields its value clamped into the caller's bound, so
/// `TestDraw(0)` is the low extreme and `TestDraw(u64::MAX)` the high one —
/// a test pins band edges without knowing the bound.
#[cfg(test)]
pub(crate) struct TestDraw(pub u64);

#[cfg(test)]
impl Draw for TestDraw {
    fn draw(&self, bound: u64) -> u64 {
        self.0.min(bound.saturating_sub(1))
    }
}
