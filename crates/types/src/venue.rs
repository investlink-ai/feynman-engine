//! Venue connection health, heartbeat tracking, and timeout policies.
//!
//! Every venue connection has a health model. The engine knows when it's
//! flying blind (no heartbeat) and can act accordingly (pause new orders,
//! trigger reconciliation, alert).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::VenueId;

// ─── Connection state ───

/// Current state of a venue connection.
///
/// Exhaustive match required — every new state must be handled everywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    /// Not yet connected (initial state or after explicit disconnect).
    Disconnected,
    /// Connection attempt in progress.
    Connecting,
    /// Connected and receiving heartbeats.
    Connected,
    /// Connected but heartbeat is late (between warning and stale thresholds).
    HeartbeatLate {
        last_heartbeat: DateTime<Utc>,
        missed_count: u32,
    },
    /// No heartbeat for longer than stale threshold. Assume connection is dead.
    /// Engine should pause new orders and trigger reconciliation when reconnected.
    Stale {
        last_heartbeat: DateTime<Utc>,
        stale_since: DateTime<Utc>,
    },
    /// Reconnecting after disconnect or stale detection.
    Reconnecting { attempt: u32 },
}

/// Full health status for a venue connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueConnectionHealth {
    pub venue_id: VenueId,
    pub state: ConnectionState,
    /// Last time we received any message from the venue (heartbeat, fill, etc).
    pub last_message_at: Option<DateTime<Utc>>,
    /// Last explicit heartbeat/pong received.
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Round-trip latency of last heartbeat (ping→pong).
    pub heartbeat_latency_ms: Option<u32>,
    /// Number of successful reconnections since engine start.
    pub reconnect_count: u32,
    /// Time of last successful connection.
    pub connected_since: Option<DateTime<Utc>>,
}

impl VenueConnectionHealth {
    /// Create initial health state for a venue (not yet connected).
    #[must_use]
    pub fn new(venue_id: VenueId) -> Self {
        Self {
            venue_id,
            state: ConnectionState::Disconnected,
            last_message_at: None,
            last_heartbeat_at: None,
            heartbeat_latency_ms: None,
            reconnect_count: 0,
            connected_since: None,
        }
    }

    /// Check if the venue is in a state where order submission is safe.
    #[must_use]
    pub fn is_submittable(&self) -> bool {
        matches!(self.state, ConnectionState::Connected)
    }

    /// Check if the connection is degraded (late heartbeat or reconnecting).
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        matches!(
            self.state,
            ConnectionState::HeartbeatLate { .. }
                | ConnectionState::Stale { .. }
                | ConnectionState::Reconnecting { .. }
        )
    }
}

// ─── Heartbeat configuration ───

/// Heartbeat thresholds for a venue connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatConfig {
    /// How often to send heartbeat pings.
    pub interval: Duration,
    /// After this many missed heartbeats, mark as `HeartbeatLate`.
    pub warning_threshold: u32,
    /// After this duration without heartbeat, mark as `Stale`.
    pub stale_threshold: Duration,
    /// Maximum reconnection attempts before giving up.
    pub max_reconnect_attempts: u32,
    /// Delay between reconnection attempts (with exponential backoff).
    pub reconnect_base_delay: Duration,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            warning_threshold: 2,
            stale_threshold: Duration::from_secs(30),
            max_reconnect_attempts: 10,
            reconnect_base_delay: Duration::from_secs(1),
        }
    }
}

// ─── Timeout policies ───

/// Timeout budgets for venue operations. Every async venue call has a deadline.
///
/// If an operation exceeds its timeout, the engine:
/// 1. Assumes the operation may or may not have succeeded
/// 2. Triggers reconciliation for that venue
/// 3. Does NOT retry blindly (would cause duplicates)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutPolicy {
    /// Max time to wait for order submission acknowledgment.
    pub submit_order: Duration,
    /// Max time to wait for cancel acknowledgment.
    pub cancel_order: Duration,
    /// Max time to wait for amend acknowledgment.
    pub amend_order: Duration,
    /// Max time to wait for position query response.
    pub query_positions: Duration,
    /// Max time to wait for balance query response.
    pub query_balance: Duration,
    /// Max time to wait for open orders query.
    pub query_orders: Duration,
    /// If no fill arrives within this duration after submission,
    /// trigger a status check (not a cancel — order may have filled).
    pub fill_watchdog: Duration,
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            submit_order: Duration::from_secs(10),
            cancel_order: Duration::from_secs(10),
            amend_order: Duration::from_secs(10),
            query_positions: Duration::from_secs(15),
            query_balance: Duration::from_secs(15),
            query_orders: Duration::from_secs(15),
            fill_watchdog: Duration::from_secs(60),
        }
    }
}

/// What to do when an operation times out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeoutAction {
    /// Trigger reconciliation for this venue, then decide.
    Reconcile,
    /// Log and alert, but take no automatic action.
    AlertOnly,
    /// Attempt to cancel the order (if submission timed out).
    CancelAndReconcile,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_health_initial_state() {
        let health = VenueConnectionHealth::new(VenueId("bybit".into()));
        assert!(!health.is_submittable());
        assert!(!health.is_degraded());
        assert_eq!(health.state, ConnectionState::Disconnected);
    }

    #[test]
    fn test_timeout_policy_defaults() {
        let policy = TimeoutPolicy::default();
        assert_eq!(policy.submit_order, Duration::from_secs(10));
        assert_eq!(policy.fill_watchdog, Duration::from_secs(60));
    }

    #[test]
    fn test_heartbeat_config_defaults() {
        let config = HeartbeatConfig::default();
        assert_eq!(config.interval, Duration::from_secs(5));
        assert_eq!(config.stale_threshold, Duration::from_secs(30));
    }
}
