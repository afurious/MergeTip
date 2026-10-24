//! Time source. Expiry windows (24 h, 1 h, 30 days) are tested by moving a [`FixedClock`]
//! instead of sleeping.

use std::sync::Mutex;

use time::{Duration, OffsetDateTime};

pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

/// The real wall clock, in UTC.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// A clock that only moves when told to. Intended for tests in any crate.
#[derive(Debug)]
pub struct FixedClock(Mutex<OffsetDateTime>);

impl FixedClock {
    pub fn new(at: OffsetDateTime) -> Self {
        Self(Mutex::new(at))
    }

    pub fn advance(&self, by: Duration) {
        let mut now = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *now += by;
    }

    pub fn set(&self, at: OffsetDateTime) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = at;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> OffsetDateTime {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn fixed_clock_moves_only_when_advanced() {
        let clock = FixedClock::new(datetime!(2026-11-02 09:00 UTC));
        assert_eq!(clock.now(), datetime!(2026-11-02 09:00 UTC));
        clock.advance(Duration::hours(24));
        assert_eq!(clock.now(), datetime!(2026-11-03 09:00 UTC));
    }
}
