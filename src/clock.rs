//! Injectable time source for every time-based decision in the library.
//!
//! The router's velocity EWMA, the `SpaceSaver` count decay and the cache engine's
//! score decay all read time through a [`Clock`]. Production code uses
//! [`SystemClock`]; replays and tests use [`ManualClock`] so time follows the log's
//! timestamps (or the test's script) instead of the wall clock.

use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A source of the current time, in milliseconds since an arbitrary epoch.
///
/// Only differences between readings are meaningful, so the epoch can be anything
/// as long as one clock is used consistently.
pub trait Clock: Send + Sync + Debug {
    /// The current time in milliseconds since this clock's epoch.
    fn now_millis(&self) -> u64;
}

/// The real wall clock: milliseconds since the Unix epoch.
///
/// This is the default clock everywhere.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl SystemClock {
    /// A shared handle to the system clock, ready to pass to `with_clock` constructors.
    pub fn shared() -> Arc<dyn Clock> {
        Arc::new(Self)
    }
}

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX)
    }
}

/// A clock that only moves when told to, for log replays and tests.
///
/// Time never moves backwards: [`set_millis`](Self::set_millis) ignores values earlier
/// than the current time, so concurrent replay tasks setting the clock out of order
/// cannot rewind it.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    /// Creates a clock that reads `start_millis` until moved.
    pub const fn new(start_millis: u64) -> Self {
        Self(AtomicU64::new(start_millis))
    }

    /// Moves the clock to `millis`, unless it already reads a later time.
    pub fn set_millis(&self, millis: u64) {
        self.0.fetch_max(millis, Ordering::Relaxed);
    }

    /// Moves the clock forward by `by` (saturating at `u64::MAX` milliseconds).
    pub fn advance(&self, by: Duration) {
        let by = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        // The closure always returns `Some`, so `fetch_update` cannot fail.
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |now| {
                Some(now.saturating_add(by))
            });
    }
}

impl Clock for ManualClock {
    fn now_millis(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_never_moves_backwards() {
        let clock = ManualClock::new(1_000);
        clock.set_millis(500);
        assert_eq!(clock.now_millis(), 1_000);
        clock.set_millis(2_500);
        assert_eq!(clock.now_millis(), 2_500);
        clock.set_millis(2_499);
        assert_eq!(clock.now_millis(), 2_500);
    }

    #[test]
    fn manual_clock_advance_adds_duration() {
        let clock = ManualClock::default();
        clock.advance(Duration::from_secs(3));
        clock.advance(Duration::from_millis(250));
        assert_eq!(clock.now_millis(), 3_250);
    }

    #[test]
    fn manual_clock_advance_saturates() {
        let clock = ManualClock::new(u64::MAX - 1);
        clock.advance(Duration::from_secs(1));
        assert_eq!(clock.now_millis(), u64::MAX);
    }

    #[test]
    fn system_clock_is_after_2020() {
        // 2020-01-01T00:00:00Z in milliseconds.
        assert!(SystemClock.now_millis() > 1_577_836_800_000);
    }
}
