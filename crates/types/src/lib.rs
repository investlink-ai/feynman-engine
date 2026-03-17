//! Feynman-specific types that extend NautilusTrader
//!
//! Types here are custom to our agent orchestration model. NautilusTrader provides
//! the core trading types (Order, Position, Event); we add agent-centric wrappers.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── Identifiers (newtypes for type safety) ───

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrderId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VenueOrderId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VenueId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstrumentId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SignalId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub String);

// ─── Agent-level allocation & risk ───

/// Agent's capital allocation and current utilization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAllocation {
    pub agent: AgentId,
    pub allocated_capital: Decimal,
    pub used_capital: Decimal,
    pub free_capital: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub current_drawdown: Decimal,
    pub max_drawdown_limit: Decimal,
    pub status: AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Active,
    Paused { reason: String, since: DateTime<Utc> },
    DrawdownBreached,
}

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
}

/// Prediction market exposure caps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionMarketLimits {
    pub max_total_notional: Decimal,
    pub max_per_market_notional: Decimal,
    pub max_pct_of_nav: Decimal,
    pub max_unresolved_markets: u32,
}

// ─── Signal (from LLM agents) ───

/// Market signal from an LLM trader agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: SignalId,
    pub agent: AgentId,
    pub instrument: InstrumentId,
    pub direction: Side,
    pub conviction: Decimal,           // 0.0 – 1.0
    pub sizing_hint: Option<Decimal>,  // optional notional target
    pub arb_type: String,              // "funding_rate", "basis", "directional"
    pub stop_loss: Option<Decimal>,
    pub take_profit: Option<Decimal>,
    pub thesis: String,
    pub urgency: Urgency,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Urgency {
    Low,
    Normal,
    High,
    Immediate,
}

// ─── Portfolio & Position Tracking ───

/// Firm-wide portfolio view with agent attribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmBook {
    pub total_nav: Decimal,
    pub free_capital: Decimal,
    pub total_unrealized_pnl: Decimal,
    pub total_realized_pnl: Decimal,
    pub total_fees_paid: Decimal,
    pub instruments: Vec<InstrumentExposure>,
    pub agent_allocations: Vec<AgentAllocation>,
    pub prediction_exposure: PredictionExposureSummary,
    pub as_of: DateTime<Utc>,
}

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

/// Per-agent, per-venue, per-market position with agent attribution.
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

// ─── Prediction Market Tracking ───

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
    Resolved { outcome: String, resolved_at: DateTime<Utc> },
    Disputed,
    Voided,
}

// ─── Risk violations ───

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskViolation {
    pub check_name: String,
    pub violation_type: ViolationType,
    pub current_value: String,
    pub limit: String,
    pub suggested_action: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViolationType {
    Hard,    // order rejected
    Soft,    // warning, approaching limit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signal_serde() {
        let signal = Signal {
            id: SignalId("sig-123".to_string()),
            agent: AgentId("satoshi".to_string()),
            instrument: InstrumentId("BTC".to_string()),
            direction: Side::Buy,
            conviction: Decimal::new(75, 2), // 0.75
            sizing_hint: Some(Decimal::new(50000, 0)),
            arb_type: "funding_rate".to_string(),
            stop_loss: Some(Decimal::new(60000, 0)),
            take_profit: Some(Decimal::new(70000, 0)),
            thesis: "Negative funding on BTCUSDT".to_string(),
            urgency: Urgency::Normal,
            metadata: serde_json::json!({"source": "bybit"}),
            created_at: Utc::now(),
        };

        let json = serde_json::to_string(&signal).unwrap();
        let deserialized: Signal = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, signal.id);
    }

    #[test]
    fn test_allocation_arithmetic() {
        let mut alloc = AgentAllocation {
            agent: AgentId("satoshi".to_string()),
            allocated_capital: Decimal::new(50000, 0),
            used_capital: Decimal::new(32100, 0),
            free_capital: Decimal::new(17900, 0),
            realized_pnl: Decimal::new(890, 0),
            unrealized_pnl: Decimal::new(500, 0),
            current_drawdown: Decimal::new(-120, 3), // -0.12%
            max_drawdown_limit: Decimal::new(-30, 3), // -3%
            status: AgentStatus::Active,
        };

        // Invariant: allocated = used + free
        assert_eq!(
            alloc.allocated_capital,
            alloc.used_capital + alloc.free_capital
        );

        // Status remains Active until drawdown breaches limit
        assert!(alloc.current_drawdown > alloc.max_drawdown_limit);
    }
}
