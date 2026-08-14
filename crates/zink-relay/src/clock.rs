//! The time ports: retention logic depends on these traits, so tests inject
//! controllable clocks and never sleep. Mirrored by the client's richer port
//! (ADR 0004); as there, no production code runs a raw timer — waits go
//! through `Clock::sleep`, deadlines through the derived `timeout`.
//!
//! Two ports because they are different concepts: `Clock` is monotonic and
//! process-local (right for in-memory stores — immune to wall-clock jumps,
//! meaningless across restarts), `WallClock` is wall time (right for
//! persisted timestamps — survives restarts, may jump).

use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Monotonic time, for state that dies with the process — and for the
/// process's own waits (the GC sweep cadence, the nudge deadline).
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn sleep(&self, dur: Duration) -> impl Future<Output = ()> + Send;

    /// Race `fut` against `sleep(dur)`; `Err(TimedOut)` if the clock crosses
    /// the deadline first. Derived from `sleep`, so a controllable clock's
    /// advance fires it deterministically.
    fn timeout<F>(
        &self,
        dur: Duration,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, TimedOut>> + Send
    where
        F: Future + Send,
    {
        let sleep = self.sleep(dur);
        async move {
            let mut fut = std::pin::pin!(fut);
            let mut sleep = std::pin::pin!(sleep);
            std::future::poll_fn(move |cx| {
                if let std::task::Poll::Ready(out) = fut.as_mut().poll(cx) {
                    return std::task::Poll::Ready(Ok(out));
                }
                sleep.as_mut().poll(cx).map(|()| Err(TimedOut))
            })
            .await
        }
    }
}

/// The deadline of [`Clock::timeout`] fired before its future resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedOut;

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

    fn sleep(&self, dur: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(dur)
    }
}

impl WallClock for SystemClock {
    fn now_ms(&self) -> u64 {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before 1970")
            .as_millis();
        u64::try_from(ms).expect("unix time in ms exceeds u64")
    }
}
