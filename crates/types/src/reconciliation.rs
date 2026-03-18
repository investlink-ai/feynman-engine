//! Reconciliation types — detect and report divergence between engine and venue state.
//!
//! The reconciliation loop periodically queries each venue for positions, orders,
//! and balances, then compares against the engine's internal state. Divergences
//! are reported to the Sequencer for correction.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{AccountId, InstrumentId, OrderId, VenueId, VenueOrderId};

/// Full report from a single reconciliation cycle for one venue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationReport {
    pub venue_id: VenueId,
    pub account_id: AccountId,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub position_divergences: Vec<PositionDivergence>,
    pub order_divergences: Vec<OrderDivergence>,
    pub balance_divergence: Option<BalanceDivergence>,
}

impl ReconciliationReport {
    /// True if no divergences were found.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.position_divergences.is_empty()
            && self.order_divergences.is_empty()
            && self.balance_divergence.is_none()
    }

    /// Total number of divergences.
    #[must_use]
    pub fn divergence_count(&self) -> usize {
        self.position_divergences.len()
            + self.order_divergences.len()
            + usize::from(self.balance_divergence.is_some())
    }
}

/// Position quantity mismatch between engine and venue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionDivergence {
    pub instrument: InstrumentId,
    /// What the engine thinks the position is.
    pub engine_qty: Decimal,
    /// What the venue reports the position is.
    pub venue_qty: Decimal,
    /// Absolute difference.
    pub delta: Decimal,
    /// Recommended action.
    pub action: ReconciliationAction,
}

/// Order state mismatch between engine and venue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderDivergence {
    /// Order exists in engine but not on venue (ghost order).
    MissingOnVenue {
        order_id: OrderId,
        engine_state: String,
    },
    /// Order exists on venue but not in engine (orphan order).
    OrphanOnVenue {
        venue_order_id: VenueOrderId,
        venue_state: String,
    },
    /// Order exists on both but states disagree.
    StateMismatch {
        order_id: OrderId,
        venue_order_id: VenueOrderId,
        engine_state: String,
        venue_state: String,
    },
}

/// Balance mismatch between engine tracking and venue-reported balance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceDivergence {
    /// Engine's tracked available balance.
    pub engine_balance: Decimal,
    /// Venue-reported available balance.
    pub venue_balance: Decimal,
    pub delta: Decimal,
}

/// What to do about a divergence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconciliationAction {
    /// Accept venue's value as truth and correct engine state.
    AcceptVenue,
    /// Log and alert, but don't auto-correct (needs human review).
    AlertOnly,
    /// Cancel the orphan order on the venue.
    CancelOrphan,
}

/// Configuration for the reconciliation loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationConfig {
    /// How often to run reconciliation (per venue).
    pub interval: std::time::Duration,
    /// Run immediately on reconnect after stale/disconnect.
    pub on_reconnect: bool,
    /// Maximum position divergence before alerting (in base units).
    /// Divergences smaller than this are auto-corrected silently.
    pub auto_correct_threshold: Decimal,
    /// Divergences larger than this trigger a halt.
    pub halt_threshold: Decimal,
}

impl Default for ReconciliationConfig {
    fn default() -> Self {
        Self {
            interval: std::time::Duration::from_secs(60),
            on_reconnect: true,
            auto_correct_threshold: Decimal::new(1, 4), // 0.0001
            halt_threshold: Decimal::new(10, 0),        // 10 units
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_report() {
        let report = ReconciliationReport {
            venue_id: VenueId("bybit".into()),
            account_id: AccountId("main".into()),
            started_at: Utc::now(),
            completed_at: Utc::now(),
            position_divergences: vec![],
            order_divergences: vec![],
            balance_divergence: None,
        };
        assert!(report.is_clean());
        assert_eq!(report.divergence_count(), 0);
    }

    #[test]
    fn test_report_with_divergences() {
        let report = ReconciliationReport {
            venue_id: VenueId("bybit".into()),
            account_id: AccountId("main".into()),
            started_at: Utc::now(),
            completed_at: Utc::now(),
            position_divergences: vec![PositionDivergence {
                instrument: InstrumentId("BTC".into()),
                engine_qty: Decimal::new(1, 0),
                venue_qty: Decimal::new(105, 2),
                delta: Decimal::new(5, 2),
                action: ReconciliationAction::AcceptVenue,
            }],
            order_divergences: vec![],
            balance_divergence: None,
        };
        assert!(!report.is_clean());
        assert_eq!(report.divergence_count(), 1);
    }
}
