//! Shared test helpers. Test-only — never compiled into the library.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::clock::Clock;

/// A controllable clock: tests advance the returned handle explicitly.
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
