//! Risk limit types — per-agent, per-instrument, per-venue, and firm-level.
//!
//! Canonical definitions — used by engine-core, risk, and gateway crates.

use std::collections::HashMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{AgentId, InstrumentId, VenueId};

/// Risk limits enforced at agent level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRiskLimits {
    pub allocated_capital: Decimal,
    pub max_position_notional: Decimal,
    pub max_gross_notional: Decimal,
    pub max_drawdown_pct: Decimal,
    pub max_daily_loss: Decimal,
    pub max_open_orders: u32,
    pub allowed_instruments: Option<Vec<InstrumentId>>,
    pub allowed_venues: Option<Vec<VenueId>>,
}

/// Per-instrument concentration and net directional limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentRiskLimits {
    pub instrument: InstrumentId,
    pub max_net_qty: Decimal,
    pub max_gross_qty: Decimal,
    pub max_concentration_pct: Decimal,
}

/// Per-venue counterparty risk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueRiskLimits {
    pub venue: VenueId,
    pub max_notional: Decimal,
    pub max_positions: u32,
    /// Max percentage of firm NAV on this venue (concentration risk).
    pub max_pct_of_nav: Decimal,
}

/// Prediction market exposure caps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionMarketLimits {
    pub max_total_notional: Decimal,
    pub max_per_market_notional: Decimal,
    pub max_pct_of_nav: Decimal,
    pub max_unresolved_markets: u32,
}

// ─── Risk violations ───

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskViolation {
    pub check_name: String,
    pub violation_type: ViolationType,
    pub current_value: String,
    pub limit: String,
    pub suggested_action: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViolationType {
    /// Order rejected unconditionally.
    Hard,
    /// Warning — approaching limit but order allowed.
    Soft,
}

/// Result of risk evaluation on an order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[must_use]
pub enum RiskOutcome {
    /// Order approved, possibly with soft warnings.
    Approved { warnings: Vec<RiskViolation> },
    /// Order resized — original qty was too large.
    Resized {
        new_qty: Decimal,
        reason: String,
        warnings: Vec<RiskViolation>,
    },
    /// Order rejected — hard violation(s).
    Rejected { violations: Vec<RiskViolation> },
}

// ─── Hierarchical risk limits ───

/// Full risk limits configuration (firm + per-agent + per-instrument + per-venue).
///
/// Canonical definition — used by engine-core and risk crates.
/// Hot-reloadable via `RiskGate::update_limits()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskLimits {
    pub firm: FirmRiskLimits,
    pub per_agent: HashMap<AgentId, AgentRiskLimits>,
    pub per_instrument: HashMap<InstrumentId, InstrumentRiskLimits>,
    pub per_venue: HashMap<VenueId, VenueRiskLimits>,
    pub prediction_market: PredictionMarketLimits,
}

/// Firm-wide risk limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmRiskLimits {
    pub max_gross_notional: Decimal,
    pub max_net_notional: Decimal,
    pub max_drawdown_pct: Decimal,
    pub max_daily_loss: Decimal,
    pub max_open_orders: u32,
}

/// Result of a single risk check (pass/fail with message).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskCheckResult {
    pub check_name: String,
    pub passed: bool,
    pub message: Option<String>,
}
