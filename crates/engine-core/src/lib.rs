//! engine-core — Sequencer command protocol, EngineCore decision logic, EventJournal, Reconciler.
//!
//! The Sequencer is the single-owner of all mutable business state.
//! All methods are synchronous (&mut self, no .await).
//! All financial math uses Decimal — no f64.
//! All timestamps come from `Clock::now()` — never `Utc::now()` directly.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;
use tokio::sync::oneshot;

mod sequencer;

#[cfg(test)]
mod sequencer_tests;

// Re-export commonly used types.
pub use types::{
    AgentAllocation, AgentId, AgentRiskLimits, AgentStatus, ClientOrderId, ClientOrderIdGenerator,
    Clock, EngineEvent, Fill, FirmBook, InstrumentId, OrderCore, OrderId, OrderState,
    PipelineOrder, PriceSource, ReconciliationAction, ReconciliationReport, RiskApprovalStamp,
    RiskCheckResult, RiskLimits, RiskViolation, RoutingAssignment, SequenceId, SequencedEvent,
    Signal, SignalId, TrackedPosition, Validated, VenueId,
};

pub use sequencer::{
    Sequencer, SequencerHandle, COMMAND_CHANNEL_CAPACITY, HIGH_PRIORITY_CHANNEL_CAPACITY,
};

/// Result type for engine operations.
pub type Result<T> = std::result::Result<T, EngineError>;

/// Typed errors for engine-core (thiserror for libs).
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("risk check failed: {reason}")]
    RiskRejected { reason: String },

    #[error("agent {agent_id} is paused: {reason}")]
    AgentPaused { agent_id: String, reason: String },

    #[error("engine is halted: {reason}")]
    EngineHalted { reason: String },

    #[error("agent not found: {0}")]
    AgentNotFound(String),

    #[error("order not found: {0}")]
    OrderNotFound(String),

    #[error("invalid state transition: {0}")]
    InvalidTransition(String),

    #[error("duplicate order: client_order_id {0} already submitted")]
    DuplicateOrder(ClientOrderId),

    #[error("journal error: {0}")]
    Journal(String),

    #[error("reconciliation error: {0}")]
    Reconciliation(String),

    #[error("unsupported command: {command}")]
    UnsupportedCommand { command: &'static str },

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Synchronous decision logic and state mutations.
///
/// All methods are `&mut self` (exclusive ownership) — no locking needed.
/// All methods are synchronous — no `.await`, no channels.
/// All timestamps provided by the `clock` parameter, never `Utc::now()`.
///
/// The EngineCore is owned exclusively by the Sequencer task.
pub trait EngineCore: Send {
    /// Current state snapshot (for reading without mutation).
    fn state(&self) -> &EngineState;

    /// Mutable access to engine state. Only the Sequencer should call this.
    fn state_mut(&mut self) -> &mut EngineState;

    /// Cheap clone for read-only consumers.
    #[must_use]
    fn snapshot(&self, sequence_id: SequenceId, snapshot_at: DateTime<Utc>) -> EngineStateSnapshot {
        EngineStateSnapshot {
            state: self.state().clone(),
            sequence_id,
            snapshot_at,
        }
    }

    /// Evaluate a validated order through circuit breaker and risk gate.
    ///
    /// Accepts `PipelineOrder<Validated>` — the type system guarantees the
    /// order has passed structural validation before reaching this point.
    /// Returns `PipelineOrder<RiskChecked>` on approval (consuming the input).
    ///
    /// `now` comes from `Clock::now()` — deterministic in backtest/replay.
    fn evaluate_order(
        &mut self,
        order: PipelineOrder<Validated>,
        now: DateTime<Utc>,
    ) -> std::result::Result<types::PipelineOrder<types::RiskChecked>, EngineError>;

    /// Update state on fill confirmation from venue.
    fn on_fill(&mut self, fill: &Fill, now: DateTime<Utc>) -> Result<()>;

    /// Update positions based on reconciliation result.
    fn on_reconciliation(
        &mut self,
        report: &ReconciliationReport,
        now: DateTime<Utc>,
    ) -> Result<()>;

    /// Mark-to-market: update unrealized P&L from current prices.
    /// Called periodically by the Sequencer (e.g., every second).
    fn mark_to_market(&mut self, prices: &dyn PriceSource, now: DateTime<Utc>) -> Result<()>;

    /// Pause an agent (prevent new orders, allow fill processing).
    fn pause_agent(&mut self, agent: &AgentId, reason: String, now: DateTime<Utc>) -> Result<()>;

    /// Resume a paused agent.
    fn resume_agent(&mut self, agent: &AgentId, now: DateTime<Utc>) -> Result<()>;

    /// Update per-agent risk limits (hot-reload).
    fn adjust_limits(&mut self, agent: &AgentId, limits: AgentRiskLimits) -> Result<()>;

    /// Halt all trading (emergency response to loss breaker).
    fn halt_all(&mut self, reason: String, now: DateTime<Utc>) -> Result<()>;

    /// Reset circuit breaker after investigation.
    fn reset_circuit_breaker(&mut self, now: DateTime<Utc>) -> Result<()>;
}

/// Append-only event log with snapshot support.
///
/// Every event produced by the Sequencer is appended to the journal.
/// State can be reconstructed by loading a snapshot and replaying events since.
#[async_trait::async_trait]
pub trait EventJournal: Send + Sync {
    /// Append a sequenced event to the journal.
    async fn append(
        &self,
        event: &SequencedEvent<EngineEvent>,
    ) -> std::result::Result<(), EngineError>;

    /// Append a batch of events atomically.
    async fn append_batch(
        &self,
        events: &[SequencedEvent<EngineEvent>],
    ) -> std::result::Result<(), EngineError>;

    /// Replay events from a sequence ID (inclusive).
    async fn replay_from(
        &self,
        from: SequenceId,
    ) -> std::result::Result<Vec<SequencedEvent<EngineEvent>>, EngineError>;

    /// Replay events in a range [from, to] inclusive.
    async fn replay_range(
        &self,
        from: SequenceId,
        to: SequenceId,
    ) -> std::result::Result<Vec<SequencedEvent<EngineEvent>>, EngineError>;

    /// Save a state snapshot at the given sequence ID.
    async fn save_snapshot(
        &self,
        sequence_id: SequenceId,
        snapshot: &EngineStateSnapshot,
    ) -> std::result::Result<(), EngineError>;

    /// Load the most recent snapshot (if any).
    async fn load_latest_snapshot(
        &self,
    ) -> std::result::Result<Option<(SequenceId, EngineStateSnapshot)>, EngineError>;

    /// The highest sequence ID in the journal.
    async fn latest_sequence_id(&self) -> std::result::Result<SequenceId, EngineError>;
}

/// Queries venue state and produces reconciliation reports.
#[async_trait::async_trait]
pub trait Reconciler: Send + Sync {
    async fn reconcile(
        &self,
        venue_id: &VenueId,
        engine_snapshot: &EngineStateSnapshot,
    ) -> std::result::Result<ReconciliationReport, EngineError>;
}

/// Commands sent TO the Sequencer from other tasks.
#[derive(Debug)]
pub enum SequencerCommand {
    SubmitOrder {
        order: PipelineOrder<Validated>,
        respond: oneshot::Sender<Result<OrderAck>>,
    },
    SubmitSignal {
        signal: Signal,
        respond: oneshot::Sender<Result<SignalAck>>,
    },
    OnFill {
        fill: Fill,
    },
    OnReconciliation {
        report: ReconciliationReport,
    },
    MarkToMarket,
    AdjustLimits {
        agent: AgentId,
        limits: AgentRiskLimits,
        respond: oneshot::Sender<Result<()>>,
    },
    PauseAgent {
        agent: AgentId,
        reason: String,
    },
    ResumeAgent {
        agent: AgentId,
    },
    HaltAll {
        reason: String,
    },
    ResetCircuitBreaker {
        breaker_id: String,
    },
    Snapshot {
        respond: oneshot::Sender<EngineStateSnapshot>,
    },
    Shutdown {
        respond: oneshot::Sender<()>,
    },
    #[cfg(test)]
    PoisonPill,
}

impl SequencerCommand {
    #[must_use]
    pub fn is_high_priority(&self) -> bool {
        match self {
            Self::OnFill { .. }
            | Self::OnReconciliation { .. }
            | Self::HaltAll { .. }
            | Self::ResetCircuitBreaker { .. }
            | Self::Shutdown { .. } => true,
            #[cfg(test)]
            Self::PoisonPill => true,
            Self::SubmitOrder { .. }
            | Self::SubmitSignal { .. }
            | Self::MarkToMarket
            | Self::AdjustLimits { .. }
            | Self::PauseAgent { .. }
            | Self::ResumeAgent { .. }
            | Self::Snapshot { .. } => false,
        }
    }
}

/// Monotonic sequence number generator owned by the Sequencer.
#[derive(Debug)]
pub struct SequenceGenerator {
    next: SequenceId,
}

impl SequenceGenerator {
    #[must_use]
    pub fn new(start_from: SequenceId) -> Self {
        Self { next: start_from }
    }

    #[must_use]
    pub fn from_zero() -> Self {
        Self {
            next: SequenceId::ZERO,
        }
    }

    pub fn next_id(&mut self) -> SequenceId {
        let current = self.next;
        self.next = SequenceId(self.next.0 + 1);
        current
    }

    #[must_use]
    pub fn current(&self) -> SequenceId {
        self.next
    }
}

/// Current state of the engine (positions, P&L, risk utilization).
#[derive(Debug, Clone)]
pub struct EngineState {
    pub orders: HashMap<OrderId, OrderRecord>,
    pub positions: HashMap<(AgentId, VenueId, InstrumentId), TrackedPosition>,
    pub pending_signals: HashMap<SignalId, Signal>,
    pub firm_book: FirmBook,
    pub agent_allocations: HashMap<AgentId, AgentAllocation>,
    pub risk_limits: RiskLimits,
    pub agent_statuses: HashMap<AgentId, AgentStatus>,
    pub idempotency_cache: HashMap<ClientOrderId, OrderAck>,
    pub last_sequence_id: SequenceId,
    pub last_updated: DateTime<Utc>,
}

#[derive(Debug, Clone)]
#[must_use]
pub struct OrderAck {
    pub order_id: OrderId,
    pub client_order_id: ClientOrderId,
    pub accepted_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
#[must_use]
pub struct SignalAck {
    pub order_id: OrderId,
    pub client_order_id: ClientOrderId,
    pub position_size: Decimal,
    pub accepted_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct EngineStateSnapshot {
    pub state: EngineState,
    pub sequence_id: SequenceId,
    pub snapshot_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct OrderRecord {
    pub core: OrderCore,
    pub state: OrderState,
    pub client_order_id: ClientOrderId,
    pub fills: Vec<Fill>,
    pub created_at: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
}

#[derive(Debug, Clone)]
#[must_use]
pub struct RiskApproval {
    pub approved_at: DateTime<Utc>,
    pub checks_performed: Vec<RiskCheckResult>,
    pub warnings: Vec<RiskViolation>,
}

impl RiskApproval {
    #[must_use]
    pub fn into_stamp(self) -> RiskApprovalStamp {
        RiskApprovalStamp {
            approved_at: self.approved_at,
            checks_performed: self.checks_performed,
            warnings: self.warnings,
        }
    }
}
