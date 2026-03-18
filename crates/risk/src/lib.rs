//! risk — CircuitBreaker (L0) and RiskGate (L1) for the Feynman execution engine.
//!
//! Path-aware checks: universal (all order paths) + signal-specific (SubmitSignal only).
//! CircuitBreaker provides hardcoded, compiled-in safety checks.
//! RiskGate provides configurable, stateful risk evaluation with hot-reload support.
//!
//! All time-dependent checks receive timestamps from `Clock::now()`,
//! never `Utc::now()` directly. This enables deterministic backtest/replay.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

pub use types::{
    AgentAllocation, AgentId, AgentRiskLimits, Clock, Fill, FirmBook, FirmRiskLimits, InstrumentId,
    InstrumentRiskLimits, OrderCore, PipelineOrder, PredictionMarketLimits, PriceSource,
    RiskCheckResult, RiskLimits, RiskOutcome, RiskViolation, Validated, VenueId, VenueRiskLimits,
};

/// Typed errors for risk operations (thiserror for libs).
#[derive(Debug, thiserror::Error)]
pub enum RiskError {
    #[error("circuit breaker tripped: [{breaker}] {reason}")]
    CircuitBreakerTripped { breaker: String, reason: String },

    #[error("risk gate rejected: {0}")]
    Rejected(String),

    #[error("price data stale for {instrument}: cannot evaluate risk")]
    StalePriceData { instrument: String },

    #[error("agent not found: {0}")]
    AgentNotFound(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, RiskError>;

// ─── Circuit Breaker (Layer 0) ───

/// Hardcoded, compiled-in safety checks. Not configurable at runtime.
/// Changed only via code deploy + review. The absolute last line of defense.
///
/// Accepts `&PipelineOrder<Validated>` — the type system guarantees the order
/// has passed structural validation before circuit breaker evaluation.
///
/// All time-dependent checks receive `now` from the Sequencer's Clock.
///
/// Sealed trait — external crates cannot implement this.
pub trait CircuitBreaker: Send + Sync + sealed::Sealed {
    /// Check if order passes all hardcoded circuit breaker rules.
    /// `now` comes from `Clock::now()`.
    fn check(
        &self,
        order: &PipelineOrder<Validated>,
        firm_book: &FirmBook,
        now: DateTime<Utc>,
    ) -> std::result::Result<(), CircuitBreakerTrip>;

    /// System-wide kill switch. When tripped, ALL execution halts.
    fn is_halted(&self) -> bool;

    /// Trip the breaker manually (emergency halt).
    fn trip(&mut self, reason: String, now: DateTime<Utc>);

    /// Reset after investigation (requires explicit action).
    fn reset(&mut self, now: DateTime<Utc>) -> std::result::Result<(), RiskError>;
}

/// Sealed trait pattern.
mod sealed {
    pub trait Sealed {}
}

/// Describes why a circuit breaker rejected an order.
#[derive(Debug, Clone)]
pub struct CircuitBreakerTrip {
    /// Which breaker tripped (e.g., "CB-4", "CB-1").
    pub breaker: String,
    /// Human-readable reason.
    pub reason: String,
    /// When the breaker tripped (from Clock).
    pub tripped_at: DateTime<Utc>,
}

// ─── Risk Gate (Layer 1) ───

/// Stateful risk evaluation. Knows current firm book. Configurable limits (hot-reloadable).
///
/// Accepts `&PipelineOrder<Validated>` — the type system guarantees the order
/// has passed structural validation and circuit breaker checks.
///
/// All time-dependent checks receive `now` from the Sequencer's Clock.
/// Mark-to-market checks use `PriceSource` for current prices.
///
/// Fast (<1ms per evaluation).
pub trait RiskGate: Send + Sync {
    /// Evaluate order against all risk limits.
    /// `now` from `Clock::now()`, `prices` for mark-to-market checks.
    fn evaluate(
        &self,
        order: &PipelineOrder<Validated>,
        prices: &dyn PriceSource,
        now: DateTime<Utc>,
    ) -> std::result::Result<RiskApproval, Vec<RiskViolation>>;

    /// Current firm book snapshot (positions, allocations, P&L).
    fn firm_book(&self) -> &FirmBook;

    /// Hot-reload risk limits (called by Taleb L2 or Feynman L3).
    fn update_limits(&mut self, limits: RiskLimits);

    /// Update internal state when a fill is processed.
    fn on_fill(&mut self, fill: &Fill, now: DateTime<Utc>);

    /// Update internal state during reconciliation.
    fn on_position_corrected(
        &mut self,
        instrument: &InstrumentId,
        new_qty: Decimal,
        now: DateTime<Utc>,
    );

    /// Get current limits (for display/monitoring).
    fn current_limits(&self) -> &RiskLimits;
}

/// Approval from RiskGate after successful evaluation.
#[derive(Debug, Clone)]
#[must_use]
pub struct RiskApproval {
    pub approved_at: DateTime<Utc>,
    pub warnings: Vec<RiskViolation>,
    pub checks_performed: Vec<RiskCheckResult>,
}
