//! The real clock — implements both time ports over `std::time`.

use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::ports::clock::{Clock, WallClock};

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
