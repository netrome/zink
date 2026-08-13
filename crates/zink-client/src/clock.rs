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

#[cfg(test)]
pub(crate) use test_clock::TestClock;

#[cfg(test)]
mod test_clock {
    use super::{Clock, WallClock};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::{Duration, Instant};

    /// A hand-driven clock: `now`/`now_ms` move only on `advance`, and `sleep`
    /// resolves the moment an `advance` crosses its deadline — never by real
    /// elapsed time. Scoped, not global, so it drives a deadline while real
    /// iroh I/O on the same runtime keeps its own timers. Clones share one
    /// timeline.
    #[derive(Clone)]
    pub(crate) struct TestClock(Arc<Mutex<Inner>>);

    struct Inner {
        now: Instant,
        now_ms: u64,
        sleepers: Vec<Arc<Mutex<Sleeper>>>,
        watchers: Vec<Waker>,
    }

    struct Sleeper {
        deadline: Instant,
        fired: bool,
        waker: Option<Waker>,
    }

    impl TestClock {
        pub(crate) fn new() -> Self {
            TestClock(Arc::new(Mutex::new(Inner {
                now: Instant::now(),
                now_ms: 1_700_000_000_000,
                sleepers: Vec::new(),
                watchers: Vec::new(),
            })))
        }

        /// Move time forward, firing every sleeper it reaches.
        pub(crate) fn advance(&self, by: Duration) {
            let mut inner = self.0.lock().unwrap();
            inner.now += by;
            inner.now_ms += by.as_millis() as u64;
            let now = inner.now;
            inner.sleepers.retain(|s| {
                let mut sleeper = s.lock().unwrap();
                if sleeper.deadline <= now {
                    sleeper.fired = true;
                    if let Some(waker) = sleeper.waker.take() {
                        waker.wake();
                    }
                    false
                } else {
                    true
                }
            });
        }

        /// Resolve once `n` sleepers are parked — the hook to wait until a
        /// concurrent fan-out has all its timers pending before advancing them
        /// together. Serial code never parks two at once, so this hangs on it:
        /// that is the concurrency assertion.
        pub(crate) fn wait_for_sleepers(&self, n: usize) -> impl Future<Output = ()> + Send {
            WaitForSleepers {
                inner: self.0.clone(),
                n,
            }
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Instant {
            self.0.lock().unwrap().now
        }

        fn sleep(&self, dur: Duration) -> impl Future<Output = ()> + Send {
            Sleep {
                inner: self.0.clone(),
                dur,
                sleeper: None,
            }
        }
    }

    impl WallClock for TestClock {
        fn now_ms(&self) -> u64 {
            self.0.lock().unwrap().now_ms
        }
    }

    /// Registers its deadline on first poll (so a parked sleeper is visible to
    /// `wait_for_sleepers`) and deregisters on drop (so a race's losing timer
    /// stops counting).
    struct Sleep {
        inner: Arc<Mutex<Inner>>,
        dur: Duration,
        sleeper: Option<Arc<Mutex<Sleeper>>>,
    }

    impl Future for Sleep {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let this = self.get_mut();
            let mut inner = this.inner.lock().unwrap();
            match &this.sleeper {
                Some(sleeper) => {
                    let mut sleeper = sleeper.lock().unwrap();
                    if sleeper.fired {
                        return Poll::Ready(());
                    }
                    sleeper.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
                None => {
                    let deadline = inner.now + this.dur;
                    if deadline <= inner.now {
                        return Poll::Ready(());
                    }
                    let sleeper = Arc::new(Mutex::new(Sleeper {
                        deadline,
                        fired: false,
                        waker: Some(cx.waker().clone()),
                    }));
                    inner.sleepers.push(sleeper.clone());
                    for watcher in inner.watchers.drain(..) {
                        watcher.wake();
                    }
                    this.sleeper = Some(sleeper);
                    Poll::Pending
                }
            }
        }
    }

    impl Drop for Sleep {
        fn drop(&mut self) {
            if let Some(sleeper) = &self.sleeper {
                let mut inner = self.inner.lock().unwrap();
                inner.sleepers.retain(|s| !Arc::ptr_eq(s, sleeper));
            }
        }
    }

    struct WaitForSleepers {
        inner: Arc<Mutex<Inner>>,
        n: usize,
    }

    impl Future for WaitForSleepers {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let mut inner = self.inner.lock().unwrap();
            if inner.sleepers.len() >= self.n {
                Poll::Ready(())
            } else {
                inner.watchers.push(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    #[test]
    fn advance__should_move_both_monotonic_and_wall_time() {
        // Given
        let clock = TestClock::new();
        let (mono, wall) = (clock.now(), clock.now_ms());

        // When
        clock.advance(Duration::from_secs(90));

        // Then
        assert_eq!(clock.now().duration_since(mono), Duration::from_secs(90));
        assert_eq!(clock.now_ms() - wall, 90_000);
    }

    #[tokio::test]
    async fn sleep__should_resolve_only_once_advanced_past_its_deadline() {
        // Given: a sleep parked well short of its deadline
        let clock = TestClock::new();
        let woke = AtomicUsize::new(0);
        tokio::join!(
            async {
                clock.sleep(Duration::from_secs(10)).await;
                woke.fetch_add(1, SeqCst);
            },
            async {
                clock.wait_for_sleepers(1).await;
                // Not yet: a partial advance leaves it parked.
                clock.advance(Duration::from_secs(9));
                assert_eq!(woke.load(SeqCst), 0);
                // Crossing the deadline fires it.
                clock.advance(Duration::from_secs(1));
            },
        );
        assert_eq!(woke.load(SeqCst), 1);
    }

    #[tokio::test]
    async fn sleep__should_fire_two_concurrent_timers_on_one_advance() {
        // Given: two timers parked at once (what a parallel fan-out does)
        let clock = TestClock::new();
        let woke = AtomicUsize::new(0);
        let deadline = Duration::from_secs(5);

        // When: one advance crosses both deadlines
        tokio::join!(
            async {
                clock.sleep(deadline).await;
                woke.fetch_add(1, SeqCst);
            },
            async {
                clock.sleep(deadline).await;
                woke.fetch_add(1, SeqCst);
            },
            async {
                clock.wait_for_sleepers(2).await;
                clock.advance(deadline);
            },
        );

        // Then: both fired — serial code would have parked only one at a time
        // and left `wait_for_sleepers(2)` hanging.
        assert_eq!(woke.load(SeqCst), 2);
    }
}
