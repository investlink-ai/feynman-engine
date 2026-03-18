//! Portfolio & position tracking — firm-wide, per-agent, per-instrument, per-venue.
//!
//! Canonical types — used by engine-core, risk, and gateway crates.
//! There is ONE `FirmBook` and ONE `AgentAllocation`. No duplicates.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{AccountId, AgentId, InstrumentId, OrderId, SignalId, VenueId};

// ─── Agent allocation ───

/// Agent's capital allocation and current utilization.
///
/// Canonical definition — used by engine-core and risk crates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAllocation {
    pub allocated_capital: Decimal,
    pub used_capital: Decimal,
    pub free_capital: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub current_drawdown: Decimal,
    pub max_drawdown_limit: Decimal,
    pub status: AgentStatus,
}

/// Agent activity status.
///
/// Exhaustive — no `_` wildcard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Active,
    Paused {
        reason: String,
        since: DateTime<Utc>,
    },
    /// Agent breached drawdown limit — orders blocked until reset.
    DrawdownBreached,
    /// Emergency halt — all agents halted.
    Halted,
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Paused { .. } => write!(f, "paused"),
            Self::DrawdownBreached => write!(f, "drawdown_breached"),
            Self::Halted => write!(f, "halted"),
        }
    }
}

// ─── Firm book ───

/// Firm-wide portfolio view with agent attribution.
///
/// Canonical definition — the single source of truth for firm state.
/// Used by engine-core (Sequencer owns it) and risk crate (reads it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmBook {
    pub nav: Decimal,
    pub gross_notional: Decimal,
    pub net_notional: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub daily_pnl: Decimal,
    pub hourly_pnl: Decimal,
    pub current_drawdown_pct: Decimal,
    pub allocated_capital: Decimal,
    pub cash_available: Decimal,
    pub total_fees_paid: Decimal,
    pub agent_allocations: HashMap<AgentId, AgentAllocation>,
    pub instruments: Vec<InstrumentExposure>,
    pub prediction_exposure: PredictionExposureSummary,
    pub as_of: DateTime<Utc>,
}

// ─── Exposure tracking ───

/// Position exposure for a single instrument across all venues/agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentExposure {
    pub instrument: InstrumentId,
    pub net_qty: Decimal,
    pub gross_qty: Decimal,
    pub net_notional_usd: Decimal,
    pub gross_notional_usd: Decimal,
    pub by_venue: Vec<VenueExposure>,
    pub by_agent: Vec<AgentExposure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueExposure {
    pub venue: VenueId,
    pub account: AccountId,
    pub net_qty: Decimal,
    pub notional_usd: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExposure {
    pub agent: AgentId,
    pub net_qty: Decimal,
    pub notional_usd: Decimal,
    pub pnl: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionExposureSummary {
    pub total_notional: Decimal,
    pub pct_of_nav: Decimal,
    pub unresolved_markets: u32,
}

// ─── Position tracking ───

/// Per-agent, per-venue, per-market position with full attribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedPosition {
    pub agent: AgentId,
    pub venue: VenueId,
    pub account: AccountId,
    pub instrument: InstrumentId,
    pub qty: Decimal,
    pub avg_entry_price: Decimal,
    pub unrealized_pnl: Decimal,
    pub realized_pnl: Decimal,
    pub total_fees_paid: Decimal,
    pub accumulated_funding: Decimal,
    pub fill_ids: Vec<(OrderId, u64)>,
    pub signal_ids: Vec<SignalId>,
    pub opened_at: DateTime<Utc>,
    pub last_fill_at: DateTime<Utc>,
}

// ─── Prediction market tracking ───

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionTracker {
    pub market_id: String,
    pub position_qty: Decimal,
    pub entry_price: Decimal,
    pub resolution_status: ResolutionStatus,
    pub pnl: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionStatus {
    Open,
    Resolved {
        outcome: String,
        resolved_at: DateTime<Utc>,
    },
    Disputed,
    Voided,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocation_arithmetic() {
        let alloc = AgentAllocation {
            allocated_capital: Decimal::new(50000, 0),
            used_capital: Decimal::new(32100, 0),
            free_capital: Decimal::new(17900, 0),
            realized_pnl: Decimal::new(890, 0),
            unrealized_pnl: Decimal::new(500, 0),
            current_drawdown: Decimal::new(-120, 3),
            max_drawdown_limit: Decimal::new(-30, 3),
            status: AgentStatus::Active,
        };

        // Invariant: allocated = used + free
        assert_eq!(
            alloc.allocated_capital,
            alloc.used_capital + alloc.free_capital
        );
    }
}
