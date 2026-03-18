//! Deterministic event sequencing — every state change is numbered and journaled.
//!
//! The Sequencer assigns a monotonic `SequenceId` to every command before processing.
//! Events produced during processing carry that sequence ID. This enables:
//! - **Replay:** reconstruct engine state from any snapshot by replaying events
//! - **Debugging:** "what happened at seq:4281?" → instant lookup
//! - **Backtest fidelity:** same code path for live and replay

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{AgentId, InstrumentId, OrderId, SequenceId, SubsystemId, VenueId, VenueOrderId};

/// An event wrapped with sequencing metadata.
///
/// Every event emitted by the engine is wrapped in this envelope.
/// The `sequence_id` provides total ordering across all events.
/// The `timestamp` comes from `Clock::now()` — never wall time directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequencedEvent<T> {
    /// Monotonic sequence number (assigned by Sequencer).
    pub sequence_id: SequenceId,
    /// Timestamp from the engine's Clock (deterministic in backtest/replay).
    pub timestamp: DateTime<Utc>,
    /// The event payload.
    pub event: T,
}

/// All events the engine can produce. Exhaustive — no wildcard match.
///
/// Each variant carries only identifier references and `Decimal` values.
/// Full objects (orders, positions) are looked up from state by ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EngineEvent {
    // ── Order lifecycle ──
    OrderReceived {
        order_id: OrderId,
        agent_id: AgentId,
        instrument_id: InstrumentId,
        venue_id: VenueId,
    },
    OrderValidated {
        order_id: OrderId,
    },
    OrderRiskChecked {
        order_id: OrderId,
        outcome: RiskOutcomeKind,
    },
    OrderRouted {
        order_id: OrderId,
        venue_id: VenueId,
    },
    OrderSubmitted {
        order_id: OrderId,
        venue_order_id: VenueOrderId,
    },
    OrderFilled {
        order_id: OrderId,
        filled_qty: Decimal,
        price: Decimal,
        fee: Decimal,
    },
    OrderPartiallyFilled {
        order_id: OrderId,
        filled_qty: Decimal,
        remaining_qty: Decimal,
        price: Decimal,
        fee: Decimal,
    },
    OrderCancelled {
        order_id: OrderId,
        reason: String,
    },
    OrderRejected {
        order_id: OrderId,
        stage: String,
        reason: String,
    },
    OrderAmended {
        order_id: OrderId,
        new_price: Option<Decimal>,
        new_qty: Option<Decimal>,
    },

    // ── Risk ──
    RiskBreached {
        agent_id: AgentId,
        check_name: String,
        detail: String,
    },
    CircuitBreakerTripped {
        breaker_id: String,
        reason: String,
    },
    CircuitBreakerReset {
        breaker_id: String,
    },

    // ── Reconciliation ──
    PositionCorrected {
        instrument_id: InstrumentId,
        venue_id: VenueId,
        old_qty: Decimal,
        new_qty: Decimal,
    },
    ReconciliationCompleted {
        venue_id: VenueId,
        divergences_found: u32,
    },

    // ── Agent lifecycle ──
    AgentPaused {
        agent_id: AgentId,
        reason: String,
    },
    AgentResumed {
        agent_id: AgentId,
    },

    // ── System ──
    EngineStarted {
        mode: String,
    },
    EngineHalted {
        reason: String,
    },
    SnapshotCreated {
        at_sequence: SequenceId,
    },

    // ── Health ──
    VenueConnected {
        venue_id: VenueId,
    },
    VenueDisconnected {
        venue_id: VenueId,
        reason: String,
    },
    SubsystemDegraded {
        subsystem: SubsystemId,
        reason: String,
    },
    SubsystemRecovered {
        subsystem: SubsystemId,
    },
}

/// Discriminant for risk check outcome (without full violation details).
/// Full details live in the order record; the event just logs the outcome kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RiskOutcomeKind {
    Approved,
    Resized,
    Rejected,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sequenced_event_serde_round_trip() {
        let event = SequencedEvent {
            sequence_id: SequenceId(42),
            timestamp: Utc::now(),
            event: EngineEvent::OrderValidated {
                order_id: OrderId("ord-1".into()),
            },
        };

        let json = serde_json::to_string(&event).unwrap();
        let deser: SequencedEvent<EngineEvent> = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.sequence_id, event.sequence_id);
    }
}
