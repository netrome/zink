//! The time ports: retention logic depends on these traits, so tests inject
//! controllable clocks and never sleep.
//!
//! Two ports because they are different concepts: `Clock` is monotonic and
//! process-local (right for in-memory stores — immune to wall-clock jumps,
//! meaningless across restarts), `WallClock` is wall time (right for
//! persisted timestamps — survives restarts, may jump).

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Monotonic time, for state that dies with the process.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

/// Wall time in unix milliseconds, for persisted timestamps.
pub trait WallClock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

/// The real clock — implements both ports.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

impl WallClock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before 1970")
            .as_millis() as u64
    }
}
