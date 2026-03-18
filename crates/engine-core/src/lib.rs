//! engine-core — EngineCore synchronous decision logic and SequencerCommand types.
//!
//! EngineCore owns all mutable business state (positions, allocations, P&L).
//! All methods are synchronous (no .await) and operate on exclusive ownership (&mut self).
//! All financial math uses Decimal — no f64 anywhere inside the engine.
//!
//! SequencerCommand is the command protocol for the Sequencer task.
//! The Sequencer is the single-owner of all mutable state and processes commands sequentially.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;
use tokio::sync::oneshot;

pub use types::{AgentId, OrderId, InstrumentId};

/// Placeholder types that will be replaced by actual types from types crate.
#[derive(Debug)]
pub struct Fill;

/// Result type for engine operations.
pub type Result<T> = std::result::Result<T, anyhow::Error>;

/// ─── Engine Core ───
///
/// Synchronous decision logic and state mutations.
/// All methods are &mut self (exclusive ownership) — no locking needed.
/// All methods are synchronous — no .await, no channels, no tokio dependency.
/// All financial math uses Decimal — no f64 anywhere inside the engine.
///
/// The EngineCore is owned exclusively by the Sequencer task.
pub trait EngineCore: Send {
    /// Current state snapshot (for reading without mutation).
    fn state(&self) -> &EngineState;

    /// Evaluate and approve order through risk gate and circuit breaker.
    /// Returns approved order or rejection reason.
    fn evaluate_order(&mut self, order: &CanonicalOrder) -> std::result::Result<ApprovedOrder, String>;

    /// Update state on fill confirmation from venue.
    /// Mutates positions, P&L, and risk gate state.
    fn on_fill(&mut self, fill: &Fill) -> Result<()>;

    /// Update positions based on reconciliation result.
    /// Called when engine discovers divergence from venue state.
    fn on_position_corrected(&mut self, instrument: &InstrumentId, new_qty: Decimal) -> Result<()>;

    /// Pause an agent (prevent new orders, allow fill processing).
    fn pause_agent(&mut self, agent: &AgentId, reason: String) -> Result<()>;

    /// Resume a paused agent.
    fn resume_agent(&mut self, agent: &AgentId) -> Result<()>;

    /// Update risk limits (hot-reload).
    fn update_risk_limits(&mut self, limits: RiskLimits) -> Result<()>;

    /// Halt all trading (emergency response to loss breaker).
    fn halt_all(&mut self, reason: String) -> Result<()>;

    /// Reset circuit breaker after investigation.
    fn reset_circuit_breaker(&mut self) -> Result<()>;
}

/// Current state of the engine (positions, P&L, risk utilization).
#[derive(Debug, Clone)]
pub struct EngineState {
    /// All open and historical orders
    pub orders: HashMap<OrderId, OrderRecord>,
    /// Firm-level positions across all agents and venues
    pub firm_book: FirmBook,
    /// Per-agent allocations and utilization
    pub agent_allocations: HashMap<AgentId, AgentAllocation>,
    /// Current risk limit configuration
    pub risk_limits: RiskLimits,
    /// Agent statuses (Active, Paused, etc)
    pub agent_statuses: HashMap<AgentId, AgentStatus>,
    /// Current timestamp (used for deterministic testing)
    pub current_time: DateTime<Utc>,
}

/// Order that passed risk gate and circuit breaker checks.
#[derive(Debug, Clone)]
pub struct ApprovedOrder {
    /// The order itself
    pub order: CanonicalOrder,
    /// Risk gate approval
    pub approval: RiskApproval,
    /// When approved
    pub approved_at: DateTime<Utc>,
}

/// Stubs for types (will be imported from types crate in real implementation).
// These are minimal placeholders; the actual implementations come from crates/types.
#[derive(Debug, Clone)]
pub struct CanonicalOrder {
    pub id: OrderId,
    // ... additional fields
}

#[derive(Debug, Clone)]
pub struct OrderRecord {
    pub order: CanonicalOrder,
    pub state: String,
    // ... additional fields
}

#[derive(Debug, Clone)]
pub struct FirmBook {
    pub nav: Decimal,
    pub gross_notional: Decimal,
    // ... additional fields
}

#[derive(Debug, Clone)]
pub struct AgentAllocation {
    pub allocated_capital: Decimal,
    pub used_capital: Decimal,
    // ... additional fields
}

#[derive(Debug, Clone)]
pub struct AgentStatus {
    pub is_active: bool,
}

#[derive(Debug, Clone)]
pub struct RiskLimits {
    pub max_gross_notional: Decimal,
    // ... additional fields
}

#[derive(Debug, Clone)]
pub struct RiskApproval {
    pub approved_at: DateTime<Utc>,
    // ... additional fields
}

/// ─── Sequencer Command ───
///
/// Commands sent TO the Sequencer from other tasks.
/// The Sequencer processes commands sequentially, one at a time.
/// All mutations to business state happen inside command processing.
///
/// Commands include:
/// - SubmitOrder / SubmitSignal (from gRPC or bus)
/// - OnFill (from venue adapter)
/// - OnReconciliation (from consistency checker)
/// - Risk limit adjustments (from Taleb L2 or Feynman L3)
/// - Pause/Resume/Halt commands
/// - Snapshot requests (for dashboard/metrics without blocking)
#[derive(Debug)]
pub enum SequencerCommand {
    /// New order request (from gRPC or bus).
    /// Includes a oneshot channel for the response.
    SubmitOrder {
        order: CanonicalOrder,
        respond: oneshot::Sender<Result<OrderAck>>,
    },

    /// New signal from LLM agent.
    /// Needs position sizing before risk check.
    SubmitSignal {
        signal: Signal,
        respond: oneshot::Sender<Result<SignalAck>>,
    },

    /// Fill received from venue adapter.
    /// Processed asynchronously (no oneshot response).
    OnFill { fill: Fill },

    /// Reconciliation result from consistency checker.
    /// Updates positions if divergence detected.
    OnReconciliation {
        results: Vec<ReconciliationResult>,
    },

    /// Risk limit change (from Taleb L2 or Feynman L3).
    /// Hot-reload without restart.
    AdjustLimits {
        agent: AgentId,
        limits: AgentRiskLimits,
        respond: oneshot::Sender<Result<()>>,
    },

    /// Pause an agent (prevent new orders, allow fill processing).
    PauseAgent { agent: AgentId, reason: String },

    /// Resume a paused agent.
    ResumeAgent { agent: AgentId },

    /// Emergency halt (circuit breaker triggered or manual).
    /// Stops all new orders across all agents.
    HaltAll { reason: String },

    /// Request a read-only snapshot of engine state.
    /// Returns immediately (Sequencer clones its state).
    Snapshot {
        respond: oneshot::Sender<EngineStateSnapshot>,
    },
}

/// Acknowledgment for SubmitOrder.
#[derive(Debug, Clone)]
pub struct OrderAck {
    pub order_id: OrderId,
    pub accepted_at: DateTime<Utc>,
}

/// Acknowledgment for SubmitSignal.
#[derive(Debug, Clone)]
pub struct SignalAck {
    /// Position size calculated by engine
    pub position_size: Decimal,
    pub accepted_at: DateTime<Utc>,
}

/// Placeholder types for signal and reconciliation (minimal stubs).
#[derive(Debug, Clone)]
pub struct Signal {
    // ... signal fields
}

#[derive(Debug, Clone)]
pub struct ReconciliationResult {
    // ... reconciliation fields
}

#[derive(Debug, Clone)]
pub struct AgentRiskLimits {
    pub allocated_capital: Decimal,
    // ... additional fields
}

/// Immutable, cloneable snapshot of engine state.
/// Cheap to produce (Sequencer clones its state periodically, not on every request).
/// Stale by design — consumers accept eventual consistency for reads.
#[derive(Debug, Clone)]
pub struct EngineStateSnapshot {
    pub state: EngineState,
    pub snapshot_at: DateTime<Utc>,
}
