//! Time behind ports, mirroring the relay's `clock.rs`. `Clock` is monotonic
//! (elapsed, backoff waits, deadlines); `WallClock` is unix-millisecond wall
//! time (wire timestamps, cooldowns that outlive a process).

use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before 1970")
            .as_millis();
        u64::try_from(ms).expect("unix time in ms exceeds u64")
    }
}

#[cfg(test)]
mod test_clock;
#[cfg(test)]
pub(crate) use test_clock::{TestClock, TestWallClock};
