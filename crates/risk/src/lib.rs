//! risk — CircuitBreaker (L0) and RiskGate (L1) for the Feynman execution engine.
//!
//! Path-aware checks: universal (all order paths) + signal-specific (SubmitSignal only).
//! CircuitBreaker provides hardcoded, compiled-in safety checks.
//! RiskGate provides configurable, stateful risk evaluation with hot-reload support.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;

/// Re-exports for convenience (imported from types crate).
/// These will be re-imported from types in the real implementation.
pub use types::{
    AgentId, AgentRiskLimits, InstrumentId, InstrumentRiskLimits,
    PredictionMarketLimits, VenueId, VenueRiskLimits,
};

/// Placeholder types that will be replaced by actual types from types crate.
#[derive(Debug, Clone)]
pub struct CanonicalOrder;
#[derive(Debug, Clone)]
pub struct Fill;
#[derive(Debug, Clone)]
pub struct RiskViolation;

/// Result type for risk operations.
pub type Result<T> = std::result::Result<T, anyhow::Error>;

/// ─── Circuit Breaker (Layer 0) ───
///
/// Hardcoded, compiled-in safety checks. Not configurable at runtime.
/// Changed only via code deploy + review. The absolute last line of defense.
///
/// When CB-4/CB-5 (loss-based) trip, they trigger `HaltAll` — the nuclear option.
/// Other breakers reject individual orders or pause per-agent/per-venue routing.
pub trait CircuitBreaker: Send + Sync {
    /// Check if order passes all hardcoded circuit breaker rules.
    /// Returns `Err(CircuitBreakerTrip)` if order must be killed unconditionally.
    fn check(&self, order: &CanonicalOrder) -> std::result::Result<(), CircuitBreakerTrip>;

    /// System-wide kill switch. When tripped, ALL execution halts.
    fn is_halted(&self) -> bool;

    /// Trip the breaker manually (emergency halt).
    fn trip(&mut self, reason: String);

    /// Reset after investigation (requires explicit action).
    fn reset(&mut self) -> std::result::Result<(), anyhow::Error>;
}

/// Describes why a circuit breaker rejected an order.
#[derive(Debug, Clone)]
pub struct CircuitBreakerTrip {
    /// Which breaker tripped (e.g., "CB-4", "CB-1")
    pub breaker: String,
    /// Human-readable reason
    pub reason: String,
    /// When the breaker tripped
    pub tripped_at: DateTime<Utc>,
}

/// ─── Risk Gate (Layer 1) ───
///
/// Stateful risk evaluation. Knows current firm book. Configurable limits (hot-reloadable).
/// Evaluates orders against universal checks + signal-specific checks.
/// Fast (<1ms per evaluation).
pub trait RiskGate: Send + Sync {
    /// Evaluate order against all risk limits.
    /// Returns `Ok(approval)` with optional warnings, or `Err(violations)` if rejected.
    fn evaluate(&self, order: &CanonicalOrder) -> std::result::Result<RiskApproval, Vec<RiskViolation>>;

    /// Current firm book snapshot (positions, allocations, P&L).
    fn firm_book(&self) -> &FirmBook;

    /// Hot-reload risk limits (called by Taleb L2 or Feynman L3).
    /// Updates configuration without requiring code deploy.
    fn update_limits(&mut self, limits: RiskLimits);

    /// Update internal state when a fill is processed.
    /// Called by Sequencer after fill confirmation from venue.
    fn on_fill(&mut self, fill: &Fill);

    /// Update internal state during reconciliation.
    /// Called when engine discovers position divergence from venue.
    fn on_position_corrected(&mut self, instrument: &InstrumentId, new_qty: Decimal);

    /// Get current limits (for display/monitoring).
    fn current_limits(&self) -> &RiskLimits;
}

/// Approval from RiskGate after successful evaluation.
#[derive(Debug, Clone)]
pub struct RiskApproval {
    /// When approval was granted
    pub approved_at: DateTime<Utc>,
    /// Non-blocking warnings (order still approved but approaching limits)
    pub warnings: Vec<RiskViolation>,
    /// Which checks were performed (for audit/debugging)
    pub checks_performed: Vec<RiskCheckResult>,
}

/// Result of a single risk check.
#[derive(Debug, Clone)]
pub struct RiskCheckResult {
    /// Name of the check (e.g., "position_notional_check")
    pub check_name: String,
    /// Whether the check passed
    pub passed: bool,
    /// Any diagnostic message
    pub message: Option<String>,
}

/// ─── Risk Limits Configuration ───

/// All risk limits: firm-level, per-agent, per-instrument, per-venue, prediction markets.
#[derive(Debug, Clone)]
pub struct RiskLimits {
    pub firm: FirmRiskLimits,
    pub per_agent: HashMap<AgentId, AgentRiskLimits>,
    pub per_instrument: HashMap<InstrumentId, InstrumentRiskLimits>,
    pub per_venue: HashMap<VenueId, VenueRiskLimits>,
    pub prediction_market: PredictionMarketLimits,
}

/// Firm-level risk limits (affect all agents).
#[derive(Debug, Clone)]
pub struct FirmRiskLimits {
    /// Total absolute exposure across all positions
    pub max_gross_notional: Decimal,
    /// Net directional exposure
    pub max_net_notional: Decimal,
    /// Halt if firm drawdown breaches this percentage
    pub max_drawdown_pct: Decimal,
    /// Halt if firm daily loss exceeds this amount
    pub max_daily_loss: Decimal,
    /// Maximum open orders across all agents
    pub max_open_orders: u32,
}

/// Current state of firm positions and P&L.
#[derive(Debug, Clone)]
pub struct FirmBook {
    /// Total realized P&L (sum across all agents)
    pub realized_pnl: Decimal,
    /// Total unrealized P&L
    pub unrealized_pnl: Decimal,
    /// Total absolute notional exposure (long + short)
    pub gross_notional: Decimal,
    /// Net directional exposure (long - short, signed)
    pub net_notional: Decimal,
    /// Total capital allocated to all agents
    pub allocated_capital: Decimal,
    /// Cash available
    pub cash_available: Decimal,
    /// Current net asset value
    pub nav: Decimal,
    /// Current drawdown from peak
    pub current_drawdown_pct: Decimal,
    /// Daily P&L (reset at 00:00 UTC)
    pub daily_pnl: Decimal,
    /// Hourly P&L (reset every hour)
    pub hourly_pnl: Decimal,
    /// Per-agent allocations and utilization
    pub agent_allocations: HashMap<AgentId, AgentAllocation>,
}

/// Current state of a single agent's capital and risk usage.
#[derive(Debug, Clone)]
pub struct AgentAllocation {
    /// Capital allocated to this agent
    pub allocated_capital: Decimal,
    /// Capital currently deployed in positions
    pub used_capital: Decimal,
    /// Free capital available for new positions
    pub free_capital: Decimal,
    /// Agent's realized P&L
    pub realized_pnl: Decimal,
    /// Agent's unrealized P&L
    pub unrealized_pnl: Decimal,
    /// Agent's current drawdown
    pub current_drawdown: Decimal,
    /// Agent's status (Active/Paused/etc)
    pub status: String,
}
