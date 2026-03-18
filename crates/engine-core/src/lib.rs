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

// Re-export commonly used types.
pub use types::{
    AgentAllocation, AgentId, AgentRiskLimits, AgentStatus, ClientOrderId, Clock, EngineEvent,
    Fill, FirmBook, InstrumentId, OrderCore, OrderId, OrderState, PipelineOrder,
    ReconciliationReport, RiskApprovalStamp, RiskCheckResult, RiskLimits, RiskViolation,
    RoutingAssignment, SequenceId, SequencedEvent, Signal, Validated, VenueId,
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

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

// ─── Engine Core ───

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
    fn mark_to_market(&mut self, prices: &dyn types::PriceSource, now: DateTime<Utc>)
        -> Result<()>;

    /// Pause an agent (prevent new orders, allow fill processing).
    fn pause_agent(&mut self, agent: &AgentId, reason: String, now: DateTime<Utc>) -> Result<()>;

    /// Resume a paused agent.
    fn resume_agent(&mut self, agent: &AgentId, now: DateTime<Utc>) -> Result<()>;

    /// Update risk limits (hot-reload).
    fn update_risk_limits(&mut self, limits: RiskLimits) -> Result<()>;

    /// Halt all trading (emergency response to loss breaker).
    fn halt_all(&mut self, reason: String, now: DateTime<Utc>) -> Result<()>;

    /// Reset circuit breaker after investigation.
    fn reset_circuit_breaker(&mut self, now: DateTime<Utc>) -> Result<()>;
}

// ─── Event Journal ───

/// Append-only event log with snapshot support.
///
/// Every event produced by the Sequencer is appended to the journal.
/// State can be reconstructed by loading a snapshot and replaying events since.
///
/// Implementations:
/// - Live/Paper: SQLite or file-backed WAL
/// - Backtest: in-memory (or disabled for speed)
///
/// The journal is **not** on the hot path — the Sequencer processes commands first,
/// then appends the resulting events. If the journal is slow, events buffer in memory
/// (graceful degradation via `BufferAndContinue` policy).
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
    /// Returns events in sequence order.
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
    /// After this, `replay_from` only needs events after this point.
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

// ─── Reconciler ───

/// Queries venue state and produces reconciliation reports.
///
/// The Reconciler is an async task that runs on a timer (per venue).
/// It sends `ReconciliationReport` to the Sequencer via `SequencerCommand::OnReconciliation`.
///
/// It does NOT mutate engine state directly — only the Sequencer does that.
#[async_trait::async_trait]
pub trait Reconciler: Send + Sync {
    /// Run reconciliation for a single venue.
    /// Queries venue for positions, orders, and balances, then compares
    /// against the provided engine state snapshot.
    async fn reconcile(
        &self,
        venue_id: &VenueId,
        engine_snapshot: &EngineStateSnapshot,
    ) -> std::result::Result<ReconciliationReport, EngineError>;
}

// ─── Sequencer Command ───

/// Commands sent TO the Sequencer from other tasks.
///
/// Every command is assigned a `SequenceId` by the Sequencer before processing.
/// The Sequencer processes commands sequentially, one at a time.
/// All mutations to business state happen inside command processing.
#[derive(Debug)]
pub enum SequencerCommand {
    /// New order request (from gRPC handler, already validated).
    SubmitOrder {
        order: PipelineOrder<Validated>,
        respond: oneshot::Sender<Result<OrderAck>>,
    },

    /// New signal from LLM agent (needs sizing before risk check).
    SubmitSignal {
        signal: Signal,
        respond: oneshot::Sender<Result<SignalAck>>,
    },

    /// Fill received from venue adapter.
    OnFill { fill: Fill },

    /// Reconciliation report from Reconciler task.
    OnReconciliation { report: ReconciliationReport },

    /// Mark-to-market tick (periodic, e.g., every 1s).
    MarkToMarket,

    /// Risk limit change (from Taleb L2 or Feynman L3).
    AdjustLimits {
        agent: AgentId,
        limits: AgentRiskLimits,
        respond: oneshot::Sender<Result<()>>,
    },

    /// Pause an agent.
    PauseAgent { agent: AgentId, reason: String },

    /// Resume a paused agent.
    ResumeAgent { agent: AgentId },

    /// Emergency halt (circuit breaker triggered or manual).
    HaltAll { reason: String },

    /// Reset circuit breaker after investigation.
    ResetCircuitBreaker,

    /// Request a read-only snapshot of engine state.
    Snapshot {
        respond: oneshot::Sender<EngineStateSnapshot>,
    },

    /// Graceful shutdown signal.
    Shutdown,
}

// ─── Sequence Number Generator ───

/// Monotonic sequence number generator owned by the Sequencer.
///
/// Every command is assigned a sequence ID before processing.
/// This provides total ordering across all engine events.
#[derive(Debug)]
pub struct SequenceGenerator {
    next: SequenceId,
}

impl SequenceGenerator {
    /// Start from a given sequence ID (e.g., after loading from journal).
    #[must_use]
    pub fn new(start_from: SequenceId) -> Self {
        Self { next: start_from }
    }

    /// Start from zero.
    #[must_use]
    pub fn from_zero() -> Self {
        Self {
            next: SequenceId::ZERO,
        }
    }

    /// Get next sequence ID and advance the counter.
    pub fn next_id(&mut self) -> SequenceId {
        let current = self.next;
        self.next = SequenceId(self.next.0 + 1);
        current
    }

    /// Current value (the next ID that will be assigned).
    #[must_use]
    pub fn current(&self) -> SequenceId {
        self.next
    }
}

// ─── State types ───

/// Current state of the engine (positions, P&L, risk utilization).
#[derive(Debug, Clone)]
pub struct EngineState {
    pub orders: HashMap<OrderId, OrderRecord>,
    pub firm_book: FirmBook,
    pub agent_allocations: HashMap<AgentId, AgentAllocation>,
    pub risk_limits: RiskLimits,
    pub agent_statuses: HashMap<AgentId, AgentStatusEntry>,
    /// Idempotency cache: prevents duplicate order submission on retry.
    /// Keyed by `ClientOrderId` — if a submission with the same client ID
    /// arrives, the cached ack is returned without re-processing.
    pub idempotency_cache: HashMap<ClientOrderId, OrderAck>,
    /// Last sequence ID processed.
    pub last_sequence_id: SequenceId,
    /// Timestamp of last processed command (from Clock).
    pub last_updated: DateTime<Utc>,
}

/// Agent status as tracked by the engine.
#[derive(Debug, Clone)]
pub struct AgentStatusEntry {
    pub is_active: bool,
    pub paused_reason: Option<String>,
    pub paused_at: Option<DateTime<Utc>>,
}

/// Acknowledgment for SubmitOrder.
#[derive(Debug, Clone)]
#[must_use]
pub struct OrderAck {
    pub order_id: OrderId,
    pub client_order_id: ClientOrderId,
    pub accepted_at: DateTime<Utc>,
}

/// Acknowledgment for SubmitSignal.
#[derive(Debug, Clone)]
#[must_use]
pub struct SignalAck {
    pub order_id: OrderId,
    pub client_order_id: ClientOrderId,
    pub position_size: Decimal,
    pub accepted_at: DateTime<Utc>,
}

/// Immutable, cloneable snapshot of engine state.
/// Stale by design — consumers accept eventual consistency for reads.
#[derive(Debug, Clone)]
pub struct EngineStateSnapshot {
    pub state: EngineState,
    pub sequence_id: SequenceId,
    pub snapshot_at: DateTime<Utc>,
}

// ─── Order record (post-pipeline, stored in EngineState) ───

/// An order as stored in engine state after entering the pipeline.
///
/// Uses `OrderState` enum (not `String`) for type-safe exhaustive matching.
/// The `OrderCore` is cloned from the `PipelineOrder` before it is consumed.
#[derive(Debug, Clone)]
pub struct OrderRecord {
    pub core: OrderCore,
    pub state: OrderState,
    pub client_order_id: ClientOrderId,
    pub fills: Vec<Fill>,
    pub created_at: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
}

/// Approval from risk gate after successful evaluation.
///
/// Bridges the risk crate's evaluation result to the pipeline's `RiskApprovalStamp`.
#[derive(Debug, Clone)]
#[must_use]
pub struct RiskApproval {
    pub approved_at: DateTime<Utc>,
    pub checks_performed: Vec<RiskCheckResult>,
    pub warnings: Vec<RiskViolation>,
}

impl RiskApproval {
    /// Convert to a `RiskApprovalStamp` for the pipeline transition.
    #[must_use]
    pub fn into_stamp(self) -> RiskApprovalStamp {
        RiskApprovalStamp {
            approved_at: self.approved_at,
            checks_performed: self.checks_performed,
            warnings: self.warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sequence_generator() {
        let mut gen = SequenceGenerator::from_zero();
        assert_eq!(gen.next_id(), SequenceId(0));
        assert_eq!(gen.next_id(), SequenceId(1));
        assert_eq!(gen.next_id(), SequenceId(2));
        assert_eq!(gen.current(), SequenceId(3));
    }

    #[test]
    fn test_sequence_generator_resume() {
        let mut gen = SequenceGenerator::new(SequenceId(1000));
        assert_eq!(gen.next_id(), SequenceId(1000));
        assert_eq!(gen.next_id(), SequenceId(1001));
    }

    #[test]
    fn test_risk_approval_into_stamp() {
        let approval = RiskApproval {
            approved_at: Utc::now(),
            checks_performed: vec![
                RiskCheckResult {
                    check_name: "CB-all".into(),
                    passed: true,
                    message: None,
                },
                RiskCheckResult {
                    check_name: "RG-notional".into(),
                    passed: true,
                    message: None,
                },
            ],
            warnings: vec![RiskViolation {
                check_name: "near_limit".into(),
                violation_type: types::ViolationType::Soft,
                current_value: "0.89".into(),
                limit: "0.90".into(),
                suggested_action: "monitor".into(),
            }],
        };
        let stamp = approval.into_stamp();
        assert_eq!(stamp.checks_performed.len(), 2);
        assert_eq!(stamp.warnings.len(), 1);
    }
}
