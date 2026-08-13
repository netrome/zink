//! Time behind ports, mirroring the relay's `clock.rs`. `Clock` is monotonic
//! (elapsed, backoff waits); `WallClock` is unix-millisecond wall time (wire
//! timestamps, cooldowns that outlive a process).

use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Monotonic time: elapsed measurement and backoff waits.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn sleep(&self, dur: Duration) -> impl Future<Output = ()> + Send;
}

/// Wall time in unix milliseconds: persisted and on-the-wire timestamps.
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

    fn sleep(&self, dur: Duration) -> impl Future<Output = ()> + Send {
        n0_future::time::sleep(dur)
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

// `TestClock`, the controllable double, lives in the submodule; P2b re-exports
// it here (`pub(crate) use`) once a consumer outside its own tests exists.
#[cfg(test)]
mod test_clock;
