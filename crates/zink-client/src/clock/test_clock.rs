//! Controllable doubles for the time ports — deliberately one per port. Wall
//! and monotonic time do not move together in the real world (NTP steps, a
//! user resetting the date), so tests must be able to drive them apart —
//! e.g. rewind the wall clock while monotonic time advances.

use super::{Clock, WallClock};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

/// A hand-driven monotonic clock: `now` moves only on `advance`, and `sleep`
/// resolves the moment an `advance` crosses its deadline — never by real
/// elapsed time. Scoped, not global, so it drives a deadline while real iroh
/// I/O on the same runtime keeps its own timers. Clones share one timeline.
#[derive(Clone)]
pub(crate) struct TestClock(Arc<Mutex<Inner>>);

struct Inner {
    now: Instant,
    next_id: u64,
    /// The parked sleeps. Whether a sleep has fired is derived
    /// (`now >= deadline`), never stored.
    sleepers: Vec<(u64, Instant, Waker)>,
    /// Wakers parked in `wait_for_sleepers`, woken when a sleeper parks.
    watchers: Vec<Waker>,
}

impl TestClock {
    pub(crate) fn new() -> Self {
        TestClock(Arc::new(Mutex::new(Inner {
            now: Instant::now(),
            next_id: 0,
            sleepers: Vec::new(),
            watchers: Vec::new(),
        })))
    }

    /// Move time forward, waking every sleeper it reaches.
    pub(crate) fn advance(&self, by: Duration) {
        let mut inner = self.0.lock().unwrap();
        inner.now = inner
            .now
            .checked_add(by)
            .expect("advance overflowed Instant");
        let now = inner.now;
        inner.sleepers.retain(|(_, deadline, waker)| {
            let due = *deadline <= now;
            if due {
                waker.wake_by_ref();
            }
            !due
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
            registration: None,
        }
    }
}

/// Takes its deadline and parks on first poll (so a parked sleeper is
/// visible to `wait_for_sleepers`) and deregisters on drop (so a race's
/// losing timer stops counting).
struct Sleep {
    inner: Arc<Mutex<Inner>>,
    dur: Duration,
    /// `(id, deadline)`, fixed at first poll.
    registration: Option<(u64, Instant)>,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let mut inner = this.inner.lock().unwrap();
        let (id, deadline) = *this.registration.get_or_insert_with(|| {
            let deadline = inner
                .now
                .checked_add(this.dur)
                .expect("sleep deadline overflowed Instant");
            inner.next_id += 1;
            (inner.next_id, deadline)
        });
        if inner.now >= deadline {
            inner.sleepers.retain(|(i, ..)| *i != id);
            return Poll::Ready(());
        }
        match inner.sleepers.iter_mut().find(|(i, ..)| *i == id) {
            Some(parked) => parked.2 = cx.waker().clone(),
            None => {
                inner.sleepers.push((id, deadline, cx.waker().clone()));
                for watcher in inner.watchers.drain(..) {
                    watcher.wake();
                }
            }
        }
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some((id, _)) = self.registration {
            let mut inner = self.inner.lock().unwrap();
            inner.sleepers.retain(|(i, ..)| *i != id);
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

/// A settable wall clock. `set_ms` is the only control: wall time *jumps* —
/// forward or backward — rather than flowing, which is exactly how the real
/// one misbehaves. Clones share the value.
#[derive(Clone)]
pub(crate) struct TestWallClock(Arc<Mutex<u64>>);

impl TestWallClock {
    pub(crate) fn new(now_ms: u64) -> Self {
        TestWallClock(Arc::new(Mutex::new(now_ms)))
    }

    pub(crate) fn set_ms(&self, now_ms: u64) {
        *self.0.lock().unwrap() = now_ms;
    }
}

impl WallClock for TestWallClock {
    fn now_ms(&self) -> u64 {
        *self.0.lock().unwrap()
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::{TestClock, TestWallClock};
    use crate::clock::{Clock, WallClock};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::time::Duration;

    #[test]
    fn advance__should_move_monotonic_time() {
        // Given
        let clock = TestClock::new();
        let before = clock.now();

        // When
        clock.advance(Duration::from_secs(90));

        // Then
        assert_eq!(clock.now().duration_since(before), Duration::from_secs(90));
    }

    #[test]
    fn set_ms__should_jump_wall_time_in_either_direction() {
        // Given
        let wall = TestWallClock::new(2_000);

        // When
        wall.set_ms(500);
        let rewound = wall.now_ms();
        wall.set_ms(3_000);
        let advanced = wall.now_ms();

        // Then
        assert_eq!(rewound, 500);
        assert_eq!(advanced, 3_000);
    }

    #[tokio::test]
    async fn sleep__should_resolve_only_once_advanced_past_its_deadline() {
        // Given
        let clock = TestClock::new();
        let woke = AtomicUsize::new(0);

        // When: advance to just short of the deadline, then across it
        let mut woke_before_deadline = usize::MAX;
        tokio::join!(
            async {
                clock.sleep(Duration::from_secs(10)).await;
                woke.fetch_add(1, SeqCst);
            },
            async {
                clock.wait_for_sleepers(1).await;
                clock.advance(Duration::from_secs(9));
                woke_before_deadline = woke.load(SeqCst);
                clock.advance(Duration::from_secs(1));
            },
        );

        // Then
        assert_eq!(woke_before_deadline, 0);
        assert_eq!(woke.load(SeqCst), 1);
    }

    #[tokio::test]
    async fn sleep__should_fire_two_concurrent_timers_on_one_advance() {
        // Given
        let clock = TestClock::new();
        let woke = AtomicUsize::new(0);
        let deadline = Duration::from_secs(5);

        // When
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

        // Then: serial code would park only one timer at a time and hang
        // `wait_for_sleepers(2)`.
        assert_eq!(woke.load(SeqCst), 2);
    }
}
