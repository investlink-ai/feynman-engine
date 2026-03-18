//! Clock abstraction — injectable time source for deterministic replay.
//!
//! **Rule:** No code in the core path calls `Utc::now()` directly.
//! All timestamps come from `Clock::now()`. This enables:
//! - Live mode: `WallClock` (real time)
//! - Backtest mode: `SimulatedClock` (controlled by harness)
//! - Replay mode: `SimulatedClock` (driven by journal timestamps)

use chrono::{DateTime, Utc};
use std::sync::atomic::{AtomicI64, Ordering};

/// Time source for the engine. Injected into the Sequencer at startup.
///
/// `Clock` is `Send + Sync` so it can be shared across async tasks.
/// The Sequencer passes `&dyn Clock` to all state-mutating operations.
pub trait Clock: Send + Sync {
    /// Current timestamp. In live mode, this is wall time.
    /// In backtest/replay, this is the simulated time.
    fn now(&self) -> DateTime<Utc>;
}

/// Wall clock — uses real system time. For live and paper trading.
#[derive(Debug, Clone, Copy)]
pub struct WallClock;

impl Clock for WallClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Simulated clock — time is controlled externally.
///
/// Used in backtest mode (harness advances via `AdvanceClock` RPC)
/// and replay mode (journal timestamps drive advancement).
///
/// Thread-safe via `AtomicI64` — no mutex needed.
/// Stores epoch milliseconds internally.
#[derive(Debug)]
pub struct SimulatedClock {
    epoch_millis: AtomicI64,
}

impl SimulatedClock {
    /// Create a new simulated clock at the given time.
    #[must_use]
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            epoch_millis: AtomicI64::new(start.timestamp_millis()),
        }
    }

    /// Create a simulated clock at Unix epoch (1970-01-01 00:00:00 UTC).
    #[must_use]
    pub fn at_epoch() -> Self {
        Self {
            epoch_millis: AtomicI64::new(0),
        }
    }

    /// Advance clock to a specific time.
    ///
    /// # Panics
    /// Panics if `to` is before the current time (clock must not go backward).
    pub fn set(&self, to: DateTime<Utc>) {
        let new_millis = to.timestamp_millis();
        let old_millis = self.epoch_millis.load(Ordering::Acquire);
        assert!(
            new_millis >= old_millis,
            "SimulatedClock cannot go backward: current={old_millis}ms, requested={new_millis}ms"
        );
        self.epoch_millis.store(new_millis, Ordering::Release);
    }

    /// Advance clock by a duration.
    pub fn advance(&self, duration: chrono::Duration) {
        let millis = duration.num_milliseconds();
        assert!(millis >= 0, "Cannot advance by negative duration");
        self.epoch_millis.fetch_add(millis, Ordering::AcqRel);
    }

    /// Advance clock by milliseconds.
    pub fn advance_millis(&self, millis: i64) {
        assert!(millis >= 0, "Cannot advance by negative milliseconds");
        self.epoch_millis.fetch_add(millis, Ordering::AcqRel);
    }

    /// Raw epoch milliseconds (for diagnostics).
    #[must_use]
    pub fn epoch_millis(&self) -> i64 {
        self.epoch_millis.load(Ordering::Acquire)
    }
}

impl Clock for SimulatedClock {
    fn now(&self) -> DateTime<Utc> {
        let millis = self.epoch_millis.load(Ordering::Acquire);
        DateTime::from_timestamp_millis(millis).expect("SimulatedClock epoch_millis out of range")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wall_clock_returns_recent_time() {
        let clock = WallClock;
        let before = Utc::now();
        let now = clock.now();
        let after = Utc::now();
        assert!(now >= before);
        assert!(now <= after);
    }

    #[test]
    fn test_simulated_clock_starts_at_given_time() {
        let start = DateTime::parse_from_rfc3339("2026-03-19T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let clock = SimulatedClock::new(start);
        assert_eq!(clock.now(), start);
    }

    #[test]
    fn test_simulated_clock_advance() {
        let start = DateTime::parse_from_rfc3339("2026-03-19T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let clock = SimulatedClock::new(start);
        clock.advance_millis(5000);
        let expected = DateTime::parse_from_rfc3339("2026-03-19T12:00:05Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(clock.now(), expected);
    }

    #[test]
    fn test_simulated_clock_set() {
        let start = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let target = DateTime::parse_from_rfc3339("2026-06-15T10:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let clock = SimulatedClock::new(start);
        clock.set(target);
        assert_eq!(clock.now(), target);
    }

    #[test]
    #[should_panic(expected = "cannot go backward")]
    fn test_simulated_clock_rejects_backward() {
        let start = DateTime::parse_from_rfc3339("2026-03-19T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let earlier = DateTime::parse_from_rfc3339("2026-03-19T11:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let clock = SimulatedClock::new(start);
        clock.set(earlier); // should panic
    }
}
