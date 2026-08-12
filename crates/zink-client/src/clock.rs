//! The client's time ports, mirroring the relay's `clock.rs`
//! (`crates/zink-relay/src/clock.rs`). Domain logic that waits or timestamps
//! goes through these traits, so tests inject a controllable clock and never
//! sleep on the wall.
//!
//! Two ports because they are different concepts (as in the relay): `Clock`
//! is monotonic and process-local — right for elapsed measurement and backoff
//! waits, immune to wall-clock jumps; `WallClock` is wall time in unix
//! milliseconds — right for the timestamps that go on the wire and the
//! negative-reach cooldowns that must survive a restart.
//!
//! Injected as one generic parameter (`Client<C: Clock + WallClock =
//! SystemClock>`), not a trait object: production monomorphizes to
//! `SystemClock` with no indirection or allocation, and edges keep writing
//! bare `Client` because the default type parameter fills in `SystemClock` —
//! the same shape as the relay's `InMemoryStore<C = SystemClock>`. That
//! generic seam is also why `sleep` can be `impl Future` rather than a boxed
//! one: with no `dyn`, there is nothing to box.

use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Monotonic time and sleeping, for elapsed measurement and backoff waits.
pub trait Clock: Send + Sync + 'static {
    /// A monotonic instant — meaningful only relative to another from the
    /// same clock, never across a restart.
    fn now(&self) -> Instant;

    /// Sleep for `dur`. `impl Future` (not a boxed future) because the port is
    /// a generic parameter, so this monomorphizes with no allocation; a
    /// `TestClock` (P2) will resolve it when the mock clock is advanced past
    /// the deadline rather than by real elapsed time.
    fn sleep(&self, dur: Duration) -> impl Future<Output = ()> + Send;
}

/// Wall time in unix milliseconds, for persisted and on-the-wire timestamps.
pub trait WallClock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

/// The real clock — implements both ports, like the relay's `SystemClock`.
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
