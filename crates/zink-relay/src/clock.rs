//! The time port: retention logic depends on this trait, so tests inject a
//! controllable clock and never sleep.

use std::time::Instant;

pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

/// The real, monotonic clock. Note for B5: `Instant` is process-local — a
/// persistent store needs wall-clock timestamps instead.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}
