//! Time behind ports (ADR 0004). `Clock` is monotonic (elapsed, backoff
//! waits, deadlines); `WallClock` is unix-millisecond wall time (wire
//! timestamps, cooldowns that outlive a process). The production
//! implementation of both is `crate::adapters::system_clock::SystemClock`.
//!
//! The discipline: the two are SEPARATE `Client` parameters because wall and
//! monotonic time move independently in the real world — tests drive them
//! apart (a wall rewind under monotonic progress). `timeout` is derived from
//! `sleep`, so a test clock's `advance` fires any deadline deterministically
//! — which is also why no port or adapter ever runs a timer of its own.
//! Doubles live in `clock/test_clock.rs`, one per port.

use std::future::Future;
use std::time::{Duration, Instant};

/// Monotonic time: elapsed measurement, backoff waits, and deadlines.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn sleep(&self, dur: Duration) -> impl Future<Output = ()> + Send;

    /// Race `fut` against `sleep(dur)`; `Err(TimedOut)` if the clock crosses
    /// the deadline first. Derived from `sleep`, so a test clock's `advance`
    /// fires it deterministically.
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

/// Wall time in unix milliseconds: persisted and on-the-wire timestamps.
pub trait WallClock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

#[cfg(test)]
mod test_clock;
#[cfg(test)]
pub(crate) use test_clock::{TestClock, TestWallClock};
