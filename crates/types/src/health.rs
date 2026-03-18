//! Graceful degradation model — subsystem health tracking and degradation policies.
//!
//! The engine is composed of subsystems (venue connections, bus, price feed, journal).
//! Each subsystem can independently fail. The engine continues operating in a
//! degraded mode when non-critical subsystems fail, rather than halting entirely.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Identifies a subsystem within the engine.
///
/// Exhaustive — no wildcard match. Adding a new subsystem forces handling everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SubsystemId {
    /// Venue WebSocket/REST connection.
    VenueConnection,
    /// Message bus (Redis Streams).
    MessageBus,
    /// Event journal (persistence).
    EventJournal,
    /// Price feed (market data cache).
    PriceFeed,
    /// Dashboard/observability.
    Dashboard,
    /// Reconciliation loop.
    Reconciler,
}

/// Health status of a single subsystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubsystemHealth {
    /// Operating normally.
    Healthy,
    /// Degraded — operating with reduced capability.
    /// Orders may still flow but with restrictions.
    Degraded {
        reason: String,
        since: DateTime<Utc>,
    },
    /// Failed — subsystem is non-functional.
    /// Engine behavior depends on `DegradationPolicy`.
    Failed {
        reason: String,
        since: DateTime<Utc>,
    },
}

impl SubsystemHealth {
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }

    #[must_use]
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// What the engine does when a subsystem fails.
///
/// Each subsystem has a configured policy. Critical subsystems halt trading;
/// non-critical subsystems degrade gracefully.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DegradationPolicy {
    /// Halt all trading (venue connections, risk gate).
    /// Used for critical subsystems where operating blind is dangerous.
    HaltTrading,
    /// Pause new orders but continue processing fills and cancels.
    /// Used for price feed (can't size new orders without prices).
    PauseNewOrders,
    /// Continue trading but buffer operations for later replay.
    /// Used for bus and journal (data can be replayed on recovery).
    BufferAndContinue,
    /// Continue trading, log the failure, alert operator.
    /// Used for dashboard and non-critical observability.
    LogAndContinue,
}

/// Default degradation policies for each subsystem.
impl SubsystemId {
    /// The degradation policy for this subsystem when it fails.
    #[must_use]
    pub fn default_policy(self) -> DegradationPolicy {
        match self {
            // Critical: can't trade without venue connection
            Self::VenueConnection => DegradationPolicy::HaltTrading,
            // Can buffer messages, replay on recovery
            Self::MessageBus => DegradationPolicy::BufferAndContinue,
            // Critical for state recovery, but can buffer events in memory
            Self::EventJournal => DegradationPolicy::BufferAndContinue,
            // Can't size new orders without prices
            Self::PriceFeed => DegradationPolicy::PauseNewOrders,
            // Non-critical: operator loses visibility but trading continues
            Self::Dashboard => DegradationPolicy::LogAndContinue,
            // Non-critical for short periods, but drift accumulates
            Self::Reconciler => DegradationPolicy::LogAndContinue,
        }
    }
}

/// Aggregate health of the entire engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineHealth {
    /// Per-subsystem health status.
    pub subsystems: Vec<(SubsystemId, SubsystemHealth)>,
    /// Overall engine status (derived from subsystem health + policies).
    pub overall: OverallHealth,
    /// Last time health was evaluated.
    pub evaluated_at: DateTime<Utc>,
}

/// Derived overall engine status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverallHealth {
    /// All subsystems healthy. Normal operation.
    Healthy,
    /// Some non-critical subsystems degraded. Trading continues with restrictions.
    Degraded,
    /// A critical subsystem failed. Trading is paused or halted.
    Halted,
}

impl EngineHealth {
    /// Compute overall health from subsystem states and their policies.
    #[must_use]
    pub fn evaluate(subsystems: Vec<(SubsystemId, SubsystemHealth)>, now: DateTime<Utc>) -> Self {
        let overall = if subsystems.iter().all(|(_, h)| h.is_healthy()) {
            OverallHealth::Healthy
        } else if subsystems
            .iter()
            .any(|(id, h)| h.is_failed() && id.default_policy() == DegradationPolicy::HaltTrading)
        {
            OverallHealth::Halted
        } else {
            OverallHealth::Degraded
        };

        Self {
            subsystems,
            overall,
            evaluated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_healthy() {
        let health = EngineHealth::evaluate(
            vec![
                (SubsystemId::VenueConnection, SubsystemHealth::Healthy),
                (SubsystemId::MessageBus, SubsystemHealth::Healthy),
                (SubsystemId::PriceFeed, SubsystemHealth::Healthy),
            ],
            Utc::now(),
        );
        assert_eq!(health.overall, OverallHealth::Healthy);
    }

    #[test]
    fn test_venue_failure_halts() {
        let health = EngineHealth::evaluate(
            vec![
                (
                    SubsystemId::VenueConnection,
                    SubsystemHealth::Failed {
                        reason: "WebSocket disconnected".into(),
                        since: Utc::now(),
                    },
                ),
                (SubsystemId::MessageBus, SubsystemHealth::Healthy),
            ],
            Utc::now(),
        );
        assert_eq!(health.overall, OverallHealth::Halted);
    }

    #[test]
    fn test_bus_failure_degrades() {
        let health = EngineHealth::evaluate(
            vec![
                (SubsystemId::VenueConnection, SubsystemHealth::Healthy),
                (
                    SubsystemId::MessageBus,
                    SubsystemHealth::Failed {
                        reason: "Redis connection refused".into(),
                        since: Utc::now(),
                    },
                ),
            ],
            Utc::now(),
        );
        assert_eq!(health.overall, OverallHealth::Degraded);
    }

    #[test]
    fn test_dashboard_failure_degrades_not_halts() {
        let health = EngineHealth::evaluate(
            vec![
                (SubsystemId::VenueConnection, SubsystemHealth::Healthy),
                (
                    SubsystemId::Dashboard,
                    SubsystemHealth::Failed {
                        reason: "Port in use".into(),
                        since: Utc::now(),
                    },
                ),
            ],
            Utc::now(),
        );
        assert_eq!(health.overall, OverallHealth::Degraded);
    }

    #[test]
    fn test_default_policies() {
        assert_eq!(
            SubsystemId::VenueConnection.default_policy(),
            DegradationPolicy::HaltTrading
        );
        assert_eq!(
            SubsystemId::PriceFeed.default_policy(),
            DegradationPolicy::PauseNewOrders
        );
        assert_eq!(
            SubsystemId::Dashboard.default_policy(),
            DegradationPolicy::LogAndContinue
        );
    }
}
