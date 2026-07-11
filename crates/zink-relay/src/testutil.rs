//! Shared test helpers. Test-only — never compiled into the library.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::clock::{Clock, WallClock};

/// A controllable monotonic clock: tests advance the handle explicitly.
pub(crate) fn test_clock() -> (Arc<Mutex<Instant>>, TestClock) {
    let now = Arc::new(Mutex::new(Instant::now()));
    (now.clone(), TestClock(now))
}

pub(crate) struct TestClock(Arc<Mutex<Instant>>);

impl Clock for TestClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

/// A controllable wall clock (unix ms): tests advance the handle explicitly.
pub(crate) fn test_wall_clock() -> (Arc<Mutex<u64>>, TestWallClock) {
    let now = Arc::new(Mutex::new(1_700_000_000_000));
    (now.clone(), TestWallClock(now))
}

#[derive(Clone)]
pub(crate) struct TestWallClock(Arc<Mutex<u64>>);

impl WallClock for TestWallClock {
    fn now_ms(&self) -> u64 {
        *self.0.lock().unwrap()
    }
}
